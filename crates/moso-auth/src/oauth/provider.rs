//! One configured identity provider, and the two halves of an authorization
//! code flow.
//!
//! [`Provider::authorize`] builds the URL the browser is sent to and the three
//! secrets that go into the session. [`Provider::exchange`] takes what came
//! back and turns it into a profile, refusing at every point the flow can be
//! attacked:
//!
//! | Check | What it stops |
//! | --- | --- |
//! | `state` compared against the session, in constant time | cross-site request forgery on the callback: an attacker completing *their* login in the victim's browser |
//! | The provider on the request matched against the session | a code for GitHub redeemed at Google's token endpoint |
//! | PKCE verifier sent, and required to be present | a stolen authorization code being redeemed by whoever stole it |
//! | `nonce` compared against the identity token | an identity token from another sign-in replayed here |
//! | `aud` compared against the client id | an identity token minted for another application |
//! | `next` validated before it is stored | an open redirect that survives a tampered session |

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use moso_core::config::SecretString;
use moso_schema::Url;

use super::endpoints::{self, Endpoints, Resolver};
use super::http::{HttpRequest, HttpTransport, RustlsTransport};
use super::idtoken::IdToken;
use super::pkce::{Pkce, random_token};
use super::profile::RawProfile;
use crate::oauth::{
    AuthorizationRequest, CallbackParams, LinkPolicy, OAuthConfig, OAuthProfile, ProviderId,
    TokenSet, check_link,
};
use crate::{Error, Result};

/// The process-wide default transport, built once.
///
/// Building a `rustls` client configuration parses the whole Mozilla root
/// store; doing it per provider, let alone per request, is measurable waste for
/// no benefit.
static SHARED_TRANSPORT: OnceLock<Arc<dyn HttpTransport>> = OnceLock::new();

/// One configured provider.
///
/// ```no_run
/// use moso_auth::{OAuthConfig, Provider};
///
/// # fn f(c: OAuthConfig) {
/// let _ = Provider::google(c).scopes(["openid", "email", "profile"]);
/// # }
/// ```
#[derive(Clone)]
pub struct Provider {
    /// Which provider.
    id: ProviderId,
    /// Its credentials and endpoints.
    config: OAuthConfig,
    /// What linking requires.
    link_policy: LinkPolicy,
    /// The scopes actually requested: the provider's defaults plus whatever the
    /// application added.
    scopes: Vec<String>,
    /// Extra query parameters on the authorization URL, for the handful of
    /// provider-specific knobs that matter (`prompt`, `access_type`, `hd`).
    extra: Vec<(String, String)>,
    /// Where `next` is allowed to point. Empty means "same-site paths only".
    redirect_allowlist: Vec<String>,
    /// The endpoints, resolved on first use.
    endpoints: Resolver,
    /// How to reach the provider. `None` means the bundled transport.
    transport: Option<Arc<dyn HttpTransport>>,
}

impl core::fmt::Debug for Provider {
    /// The client secret lives in [`OAuthConfig`], which redacts it, but the
    /// transport is not `Debug` and the endpoint cache is noise.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Provider")
            .field("id", &self.id)
            .field("config", &self.config)
            .field("link_policy", &self.link_policy)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

impl Provider {
    /// Build one, filling in the provider's default scopes.
    fn build(id: ProviderId, config: OAuthConfig) -> Self {
        let mut scopes: Vec<String> = endpoints::default_scopes(&id)
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        for extra in &config.scopes {
            if !scopes.iter().any(|s| s == extra) {
                scopes.push(extra.clone());
            }
        }

        Self {
            id,
            config,
            link_policy: LinkPolicy::default(),
            scopes,
            extra: Vec::new(),
            redirect_allowlist: Vec::new(),
            endpoints: Resolver::default(),
            transport: None,
        }
    }

    /// Google, with OIDC.
    ///
    /// Asserts `email_verified` and means it, so a Google login links to an
    /// existing account under the default policy.
    ///
    /// ```no_run
    /// # use moso_auth::{OAuthConfig, Provider};
    /// # fn f(c: OAuthConfig) { let _ = Provider::google(c); }
    /// ```
    #[must_use]
    pub fn google(config: OAuthConfig) -> Self {
        Self::build(ProviderId::Google, config)
    }

    /// GitHub.
    ///
    /// Not an OIDC provider: there is no identity token and no `nonce`. The
    /// address takes a second request to `/user/emails`, because `/user`
    /// returns only the *public* one, which most users have not set.
    ///
    /// ```no_run
    /// # use moso_auth::{OAuthConfig, Provider};
    /// # fn f(c: OAuthConfig) { let _ = Provider::github(c); }
    /// ```
    #[must_use]
    pub fn github(config: OAuthConfig) -> Self {
        Self::build(ProviderId::GitHub, config)
    }

    /// Microsoft Entra ID.
    ///
    /// Defaults to the multi-tenant `common` endpoint, whose issuer varies with
    /// the signing-in tenant and therefore cannot be pinned. A single-tenant
    /// application should say so — [`OAuthConfig::tenant`] — which pins the
    /// issuer and stops any Microsoft account in the world from being a valid
    /// login.
    ///
    /// Entra asserts nothing about an address, so an Entra profile always
    /// reports `email_verified: false`.
    ///
    /// ```no_run
    /// # use moso_auth::{OAuthConfig, Provider};
    /// # fn f(c: OAuthConfig) { let _ = Provider::microsoft(c); }
    /// ```
    #[must_use]
    pub fn microsoft(config: OAuthConfig) -> Self {
        Self::build(ProviderId::Microsoft, config)
    }

    /// Apple.
    ///
    /// Apple's client secret is an ES256-signed JWT with a six-month maximum
    /// lifetime, not a fixed string: put the signed token in
    /// [`OAuthConfig::client_secret`] and rotate it. Moso checks its shape and
    /// its expiry before every exchange, so an expired secret is a
    /// configuration error naming the problem rather than a
    /// `invalid_client` from Apple.
    ///
    /// There is no `userinfo` endpoint: the identity token is the profile, and
    /// the user's name is sent once, on the first authorization only.
    ///
    /// ```no_run
    /// # use moso_auth::{OAuthConfig, Provider};
    /// # fn f(c: OAuthConfig) { let _ = Provider::apple(c); }
    /// ```
    #[must_use]
    pub fn apple(config: OAuthConfig) -> Self {
        // Asking for a name or an address forces `response_mode=form_post`,
        // which Apple requires and rejects the request without.
        Self::build(ProviderId::Apple, config).param("response_mode", "form_post")
    }

    /// GitLab.
    ///
    /// `gitlab.com`. A self-hosted instance is [`Provider::oidc`] with its own
    /// discovery URL.
    ///
    /// ```no_run
    /// # use moso_auth::{OAuthConfig, Provider};
    /// # fn f(c: OAuthConfig) { let _ = Provider::gitlab(c); }
    /// ```
    #[must_use]
    pub fn gitlab(config: OAuthConfig) -> Self {
        Self::build(ProviderId::GitLab, config)
    }

    /// Discord.
    ///
    /// OAuth2 only, no identity token. Reports `verified`, which is whether
    /// Discord confirmed the address.
    ///
    /// ```no_run
    /// # use moso_auth::{OAuthConfig, Provider};
    /// # fn f(c: OAuthConfig) { let _ = Provider::discord(c); }
    /// ```
    #[must_use]
    pub fn discord(config: OAuthConfig) -> Self {
        Self::build(ProviderId::Discord, config)
    }

    /// Slack.
    ///
    /// "Sign in with Slack", which is OIDC on its own hostnames — not the
    /// `oauth.v2.access` bot flow.
    ///
    /// ```no_run
    /// # use moso_auth::{OAuthConfig, Provider};
    /// # fn f(c: OAuthConfig) { let _ = Provider::slack(c); }
    /// ```
    #[must_use]
    pub fn slack(config: OAuthConfig) -> Self {
        Self::build(ProviderId::Slack, config)
    }

    /// Anything with an OIDC discovery document.
    ///
    /// The endpoints are fetched from the document on first use, so a provider
    /// that rotates them does not need a redeploy.
    ///
    /// ```no_run
    /// # use moso_auth::{OAuthConfig, Provider};
    /// # fn f(c: OAuthConfig) {
    /// let _ = Provider::oidc("keycloak", "https://id.example.com/.well-known/openid-configuration", c);
    /// # }
    /// ```
    #[must_use]
    pub fn oidc(
        name: impl Into<String>,
        discovery_url: impl Into<String>,
        config: OAuthConfig,
    ) -> Self {
        let mut config = config;
        config.discovery_url = Some(discovery_url.into());
        Self::build(ProviderId::Oidc(name.into()), config)
    }

    /// Request these scopes in addition to the provider's defaults.
    ///
    /// ```no_run
    /// # use moso_auth::Provider;
    /// # fn f(p: Provider) { let _ = p.scopes(["openid", "email"]); }
    /// ```
    #[must_use]
    pub fn scopes<S: Into<String>>(mut self, scopes: impl IntoIterator<Item = S>) -> Self {
        for scope in scopes {
            let scope = scope.into();
            if !self.scopes.contains(&scope) {
                self.scopes.push(scope);
            }
        }
        self
    }

    /// Replace the scope list entirely, defaults included.
    ///
    /// For the provider whose defaults are wrong for one application — asking
    /// GitHub for `read:user` when only `user:email` is wanted, say.
    ///
    /// ```no_run
    /// # use moso_auth::Provider;
    /// # fn f(p: Provider) { let _ = p.only_scopes(["user:email"]); }
    /// ```
    #[must_use]
    pub fn only_scopes<S: Into<String>>(mut self, scopes: impl IntoIterator<Item = S>) -> Self {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Add a query parameter to the authorization URL.
    ///
    /// The escape hatch for the provider-specific knobs: Google's
    /// `access_type=offline` and `prompt=consent` (which is how a refresh token
    /// is obtained), `hd` to restrict to one Workspace domain, Entra's
    /// `domain_hint`.
    ///
    /// ```no_run
    /// # use moso_auth::Provider;
    /// # fn f(p: Provider) { let _ = p.param("access_type", "offline"); }
    /// ```
    #[must_use]
    pub fn param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        self.extra.retain(|(existing, _)| existing != &name);
        self.extra.push((name, value.into()));
        self
    }

    /// Change what linking requires.
    ///
    /// ```no_run
    /// # use moso_auth::{LinkPolicy, Provider};
    /// # fn f(p: Provider) { let _ = p.link_policy(LinkPolicy::SessionOnly); }
    /// ```
    #[must_use]
    pub fn link_policy(mut self, policy: LinkPolicy) -> Self {
        self.link_policy = policy;
        self
    }

    /// Where `next` may point after a successful login.
    ///
    /// Empty — the default — means same-site paths only: `next` must begin with
    /// a single `/`. Anything else is an origin prefix, compared before the
    /// value is ever stored, so an open redirect is not reachable even from a
    /// tampered session.
    ///
    /// ```no_run
    /// # use moso_auth::Provider;
    /// # fn f(p: Provider) { let _ = p.redirect_allowlist(["https://app.example.com"]); }
    /// ```
    #[must_use]
    pub fn redirect_allowlist<S: Into<String>>(
        mut self,
        allowed: impl IntoIterator<Item = S>,
    ) -> Self {
        self.redirect_allowlist = allowed.into_iter().map(Into::into).collect();
        self
    }

    /// Use this transport instead of the bundled one.
    ///
    /// For an application behind an egress proxy, one that needs a header on
    /// every outbound request, or a test pointing at something that is not
    /// really Google.
    ///
    /// ```no_run
    /// # use moso_auth::Provider;
    /// # use moso_auth::oauth::http::{HttpTransport, RustlsTransport};
    /// # use std::sync::Arc;
    /// # fn f(p: Provider) -> moso_auth::Result<Provider> {
    /// Ok(p.transport(RustlsTransport::shared()?))
    /// # }
    /// ```
    #[must_use]
    pub fn transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Point a built-in provider at a self-hosted instance.
    ///
    /// GitHub Enterprise Server is the case this exists for: it is the same
    /// provider, with the same non-OIDC flow and the same second request for an
    /// address, on somebody else's hostname — and because it is not OpenID
    /// Connect, there is no discovery document to point
    /// [`Provider::oidc`] at.
    ///
    /// The three arguments are the authorization endpoint, the token endpoint,
    /// and the profile endpoint (`None` for a provider that has none). Setting
    /// them skips discovery entirely.
    ///
    /// **For a self-hosted provider that *does* speak OpenID Connect** — a
    /// GitLab instance, a Keycloak realm — prefer [`Provider::oidc`] with its
    /// discovery URL: the document pins the `iss` an identity token must carry,
    /// and this method cannot.
    ///
    /// ```no_run
    /// # use moso_auth::{OAuthConfig, Provider};
    /// # fn f(c: OAuthConfig) {
    /// let _ = Provider::github(c).self_hosted(
    ///     "https://github.example.com/login/oauth/authorize",
    ///     "https://github.example.com/login/oauth/access_token",
    ///     Some("https://github.example.com/api/v3/user"),
    /// );
    /// # }
    /// ```
    #[must_use]
    pub fn self_hosted(
        self,
        authorization: impl Into<String>,
        token: impl Into<String>,
        userinfo: Option<&str>,
    ) -> Self {
        // Inherit whether this provider issues identity tokens from the table:
        // a self-hosted GitHub is still not OpenID Connect, and a self-hosted
        // GitLab still is.
        let oidc = endpoints::builtin(&self.id, self.config.tenant.as_deref())
            .is_none_or(|template| template.oidc);

        self.endpoints.seed(Endpoints {
            authorization: authorization.into(),
            token: token.into(),
            userinfo: userinfo.map(str::to_owned),
            issuer: None,
            jwks_uri: None,
            oidc,
        });
        self
    }

    /// Which provider this is.
    ///
    /// ```no_run
    /// # use moso_auth::{Provider, ProviderId};
    /// # fn f(p: &Provider) { let _: &ProviderId = p.id(); }
    /// ```
    #[must_use]
    pub fn id(&self) -> &ProviderId {
        &self.id
    }

    /// What linking a provider identity to a local account requires here.
    ///
    /// ```no_run
    /// # use moso_auth::{LinkPolicy, Provider};
    /// # fn f(p: &Provider) -> LinkPolicy { p.policy() }
    /// ```
    #[must_use]
    pub fn policy(&self) -> LinkPolicy {
        self.link_policy
    }

    /// The scopes this provider will ask for.
    ///
    /// ```no_run
    /// # use moso_auth::Provider;
    /// # fn f(p: &Provider) -> &[String] { p.requested_scopes() }
    /// ```
    #[must_use]
    pub fn requested_scopes(&self) -> &[String] {
        &self.scopes
    }

    /// Apply this provider's link policy to a profile.
    ///
    /// The same rule as [`check_link`], with the policy already filled in — so
    /// a route handler cannot accidentally pass a different one.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`] when the policy refuses.
    ///
    /// ```no_run
    /// # use moso_auth::{OAuthProfile, Provider};
    /// # fn f(p: &Provider, profile: &OAuthProfile) -> moso_auth::Result<()> {
    /// p.check_link(profile, false)
    /// # }
    /// ```
    pub fn check_link(&self, profile: &OAuthProfile, has_session: bool) -> Result<()> {
        check_link(profile, self.link_policy, has_session)
    }

    /// Check the configuration without making a request.
    ///
    /// What a boot report calls: a redirect URI that is not a URL, an empty
    /// client id, a generic OIDC provider with no discovery URL, or an Apple
    /// client secret that is not a JWT are all things worth finding at boot
    /// rather than on the first login.
    ///
    /// # Errors
    ///
    /// [`Error::Config`], naming the field and the fix.
    ///
    /// ```no_run
    /// # use moso_auth::Provider;
    /// # fn f(p: &Provider) -> moso_auth::Result<()> { p.validate() }
    /// ```
    pub fn validate(&self) -> Result<()> {
        if self.config.client_id.trim().is_empty() {
            return Err(config(format!(
                "`{}` has an empty client id; it is usually an environment variable that was not \
                 set",
                self.id.as_str()
            )));
        }
        if self.config.client_secret.is_empty() {
            return Err(config(format!(
                "`{}` has an empty client secret; it is usually an environment variable that was \
                 not set",
                self.id.as_str()
            )));
        }

        Url::parse_with_schemes(&self.config.redirect_uri, &["https", "http"]).map_err(|e| {
            config(format!(
                "`{}` has a redirect URI that is not an absolute http(s) URL ({e}); it must be \
                 the exact string registered with the provider, e.g. \
                 `https://app.example.com/auth/oauth/{}/callback`",
                self.id.as_str(),
                self.id.as_str()
            ))
        })?;

        if let Some(url) = &self.config.discovery_url {
            Url::parse_with_schemes(url, &["https", "http"]).map_err(|e| {
                config(format!(
                    "`{}` has a discovery URL that is not an absolute http(s) URL ({e})",
                    self.id.as_str()
                ))
            })?;
        } else if matches!(self.id, ProviderId::Oidc(_)) {
            return Err(config(format!(
                "`{}` is a generic OIDC provider and has no discovery URL; build it with \
                 `Provider::oidc(name, discovery_url, config)`",
                self.id.as_str()
            )));
        }

        if self.id == ProviderId::Apple {
            self.apple_secret()?;
        }

        for scope in &self.scopes {
            if scope.contains(' ') {
                return Err(config(format!(
                    "`{}` was given the scope `{scope}`, which contains a space; scopes are \
                     passed one per item, not as one space-separated string",
                    self.id.as_str()
                )));
            }
        }

        Ok(())
    }

    /// Apple's client secret, checked for shape and expiry.
    fn apple_secret(&self) -> Result<&SecretString> {
        let secret = &self.config.client_secret;
        let token = IdToken::parse(secret.expose()).map_err(|_| {
            config(
                "Apple's client secret is an ES256-signed JWT, not a fixed string, and this one \
                 is not a JWT; mint it from the private key in the Apple developer portal, with \
                 `iss` = your team id, `sub` = your services id and `aud` = \
                 `https://appleid.apple.com`",
            )
        })?;

        let now = chrono::Utc::now().timestamp();
        if token.claims.exp < now {
            return Err(config(format!(
                "Apple's client secret expired {} days ago; it has a six-month maximum lifetime \
                 and has to be rotated",
                (now - token.claims.exp) / 86_400
            )));
        }

        Ok(secret)
    }

    /// The transport to use.
    fn http(&self) -> Result<Arc<dyn HttpTransport>> {
        if let Some(transport) = &self.transport {
            return Ok(Arc::clone(transport));
        }
        if let Some(shared) = SHARED_TRANSPORT.get() {
            return Ok(Arc::clone(shared));
        }
        let built = RustlsTransport::shared()?;
        Ok(Arc::clone(SHARED_TRANSPORT.get_or_init(|| built)))
    }

    /// The endpoints, resolving discovery if that is where they live.
    async fn endpoints(&self) -> Result<Endpoints> {
        let transport = self.http()?;
        self.endpoints
            .resolve(
                &self.id,
                self.config.tenant.as_deref(),
                self.config.discovery_url.as_deref(),
                transport.as_ref(),
            )
            .await
    }

    /// Begin an authorization: the URL to send the user to, and what to store.
    ///
    /// The returned [`AuthorizationRequest`] carries the PKCE verifier and the
    /// `state`, both of which go into the session and neither of which goes to
    /// the client.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the provider's endpoints
    /// are not resolved yet, and
    /// [`Error::Unavailable`] when discovery fails.
    ///
    /// ```no_run
    /// # use moso_auth::{AuthorizationRequest, Provider};
    /// # async fn f(p: &Provider) -> moso_auth::Result<AuthorizationRequest> {
    /// p.authorize(None).await
    /// # }
    /// ```
    pub async fn authorize(&self, next: Option<&str>) -> Result<AuthorizationRequest> {
        self.validate()?;
        let endpoints = self.endpoints().await?;

        let pkce = Pkce::generate();
        let state = random_token();
        let nonce = endpoints.oidc.then(random_token);

        // Validated *before* it is stored, so a tampered session cannot smuggle
        // one past the check on the way out.
        let next = match next {
            Some(next) => Some(self.check_redirect(next)?),
            None => None,
        };

        let mut query = form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", &self.config.redirect_uri)
            .append_pair("scope", &self.scopes.join(" "))
            .append_pair("state", &state)
            .append_pair("code_challenge", pkce.challenge())
            .append_pair("code_challenge_method", pkce.method());
        if let Some(nonce) = &nonce {
            query.append_pair("nonce", nonce);
        }
        for (name, value) in &self.extra {
            query.append_pair(name, value);
        }

        let separator = if endpoints.authorization.contains('?') {
            '&'
        } else {
            '?'
        };
        let url = format!("{}{separator}{}", endpoints.authorization, query.finish());

        Ok(AuthorizationRequest {
            url: Url::parse_with_schemes(&url, &["https", "http"]).map_err(|e| {
                config(format!(
                    "`{}` produced an authorization URL that does not parse ({e}); its \
                     authorization endpoint is `{}`",
                    self.id.as_str(),
                    endpoints.authorization
                ))
            })?,
            verifier: pkce.verifier().clone(),
            state: SecretString::new(state),
            nonce: nonce.map(SecretString::new),
            next,
            provider: self.id.clone(),
        })
    }

    /// Complete an authorization: exchange the code and load the profile.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`] when the `state` does not
    /// match what the session holds, when the PKCE verifier is missing, or when
    /// an OIDC `nonce` does not match — each with a distinct reason in the log
    /// and the same response to the client.
    ///
    /// ```no_run
    /// # use moso_auth::{AuthorizationRequest, CallbackParams, OAuthProfile, Provider};
    /// # async fn f(p: &Provider, r: &AuthorizationRequest, c: &CallbackParams)
    /// #     -> moso_auth::Result<OAuthProfile> {
    /// p.exchange(r, c).await
    /// # }
    /// ```
    pub async fn exchange(
        &self,
        request: &AuthorizationRequest,
        callback: &CallbackParams,
    ) -> Result<OAuthProfile> {
        let code = self.check_callback(request, callback)?;
        let endpoints = self.endpoints().await?;
        let transport = self.http()?;

        let tokens = self
            .redeem(&endpoints, request, code, transport.as_ref())
            .await?;

        let id_token = match (&tokens.id_token, endpoints.oidc) {
            (Some(raw), _) => {
                let token = IdToken::parse(raw.expose())?;
                token.check(
                    endpoints.issuer.as_deref(),
                    &self.config.client_id,
                    request.nonce.as_ref().map(SecretString::expose),
                    0,
                )?;
                Some(token)
            }
            // An OIDC provider that returned no identity token did not do what
            // it advertised, and the `nonce` this request sent is then bound to
            // nothing at all.
            (None, true) => {
                return Err(ceremony(Cow::Owned(format!(
                    "`{}` speaks OpenID Connect and returned no identity token, so nothing binds \
                     this response to the request that started it",
                    self.id.as_str()
                ))));
            }
            (None, false) => None,
        };

        let mut raw = RawProfile {
            id_token,
            ..RawProfile::default()
        };

        if let Some(userinfo) = &endpoints.userinfo {
            let response = transport
                .send(
                    &HttpRequest::get(userinfo)
                        .bearer(tokens.access_token.expose())
                        .header("user-agent", "moso-auth"),
                )
                .await?;
            if !response.is_success() {
                return Err(unavailable(format!(
                    "`{}` answered {} at its userinfo endpoint: {}",
                    self.id.as_str(),
                    response.status,
                    response.text().chars().take(200).collect::<String>()
                )));
            }
            let body: serde_json::Value = response.json("userinfo endpoint")?;

            // Slack answers 200 with `{"ok": false, "error": "..."}`.
            if body.get("ok") == Some(&serde_json::Value::Bool(false)) {
                return Err(unavailable(format!(
                    "`{}` refused the userinfo request: {}",
                    self.id.as_str(),
                    body.get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("no reason given")
                )));
            }
            raw.userinfo = Some(body);
        }

        if self.id == ProviderId::GitHub
            && let Some(userinfo) = &endpoints.userinfo
        {
            // Derived from the profile endpoint rather than hard-coded, so a
            // GitHub Enterprise host configured through the endpoint table gets
            // its own `/user/emails` instead of github.com's.
            let response = transport
                .send(
                    &HttpRequest::get(format!("{}/emails", userinfo.trim_end_matches('/')))
                        .bearer(tokens.access_token.expose())
                        .header("user-agent", "moso-auth")
                        .header("accept", "application/vnd.github+json"),
                )
                .await?;
            // A token without `user:email` gets a 403 here. That is not a
            // failure of the login — it is a login without a verified address,
            // and the link policy will decide what that is worth.
            if response.is_success() {
                raw.emails = response.json::<serde_json::Value>("GitHub emails").ok();
            }
        }

        raw.into_profile(&self.id, tokens)
    }

    /// Everything about a callback that can be checked before a byte leaves the
    /// process.
    ///
    /// Separated out because it is the security-critical half and it is pure:
    /// `state`, the provider, the error parameter and the presence of a code.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`], with a distinct reason for each failure in the log
    /// and the same response to the client.
    ///
    /// ```no_run
    /// # use moso_auth::{AuthorizationRequest, CallbackParams, Provider};
    /// # fn f(p: &Provider, r: &AuthorizationRequest, c: &CallbackParams)
    /// #     -> moso_auth::Result<()> {
    /// p.check_callback(r, c).map(|_| ())
    /// # }
    /// ```
    pub fn check_callback<'a>(
        &self,
        request: &AuthorizationRequest,
        callback: &'a CallbackParams,
    ) -> Result<&'a str> {
        // The provider first: a session that started a Google flow must not
        // redeem a code at GitHub's token endpoint, whatever the state says.
        if request.provider != self.id {
            return Err(ceremony(Cow::Owned(format!(
                "the session holds a `{}` authorization and this callback arrived for `{}`",
                request.provider.as_str(),
                self.id.as_str()
            ))));
        }

        // Then `state`, before anything else is looked at, because everything
        // else is attacker-controlled until this passes.
        if !constant_time_eq(request.state.expose(), &callback.state) {
            return Err(ceremony(Cow::Borrowed(
                "the `state` in the callback is not the one this session issued; the callback \
                 belongs to a different browser, or to an attacker's own authorization",
            )));
        }

        if let Some(error) = &callback.error {
            return Err(ceremony(Cow::Owned(match &callback.error_description {
                Some(detail) => format!("the provider refused: {error} ({detail})"),
                None => format!("the provider refused: {error}"),
            })));
        }

        let code = callback.code.as_deref().filter(|c| !c.is_empty());
        code.ok_or_else(|| {
            ceremony(Cow::Borrowed(
                "the callback carries neither an authorization code nor an error",
            ))
        })
    }

    /// Redeem the code at the token endpoint.
    async fn redeem(
        &self,
        endpoints: &Endpoints,
        request: &AuthorizationRequest,
        code: &str,
        transport: &dyn HttpTransport,
    ) -> Result<TokenSet> {
        // PKCE is not optional and there is no path here that omits it. A
        // session that lost its verifier cannot be completed: redeeming without
        // one is exactly what a stolen code needs.
        let verifier = Pkce::from_verifier(request.verifier.expose()).map_err(|_| {
            ceremony(Cow::Borrowed(
                "the session holds no usable PKCE verifier, so this authorization cannot be \
                 completed; without it a stolen code is enough to sign in",
            ))
        })?;

        let secret = if self.id == ProviderId::Apple {
            self.apple_secret()?
        } else {
            &self.config.client_secret
        };

        let body = form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", &self.config.redirect_uri)
            .append_pair("client_id", &self.config.client_id)
            .append_pair("client_secret", secret.expose())
            .append_pair("code_verifier", verifier.verifier().expose())
            .finish();

        let response = transport
            .send(&HttpRequest::form(&endpoints.token, body).header("user-agent", "moso-auth"))
            .await?;

        let payload: serde_json::Value = response.json("token endpoint")?;

        // The OAuth error response (RFC 6749 §5.2) is a 400 with a body worth
        // reading: `invalid_grant` on a reused code, `invalid_client` on a
        // rotated secret. Repeating it saves an afternoon.
        if !response.is_success()
            || payload.get("error").is_some()
            || payload.get("ok") == Some(&serde_json::Value::Bool(false))
        {
            let code = payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("no error code");
            let detail = payload
                .get("error_description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            return Err(ceremony(Cow::Owned(format!(
                "`{}` refused the token exchange with {}: {code} {detail}",
                self.id.as_str(),
                response.status
            ))));
        }

        let access_token = payload
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                ceremony(Cow::Owned(format!(
                    "`{}` answered the token exchange without an access token",
                    self.id.as_str()
                )))
            })?;

        let expires_at = payload
            .get("expires_in")
            .and_then(serde_json::Value::as_i64)
            .map(|seconds| chrono::Utc::now().timestamp() + seconds);

        Ok(TokenSet {
            access_token: SecretString::new(access_token),
            refresh_token: payload
                .get("refresh_token")
                .and_then(serde_json::Value::as_str)
                .filter(|t| !t.is_empty())
                .map(SecretString::new),
            id_token: payload
                .get("id_token")
                .and_then(serde_json::Value::as_str)
                .filter(|t| !t.is_empty())
                .map(SecretString::new),
            expires_at,
            scopes: payload
                .get("scope")
                .and_then(serde_json::Value::as_str)
                .map(|s| {
                    s.split([' ', ','])
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Check a `next` against the allowlist.
    fn check_redirect(&self, next: &str) -> Result<String> {
        if self.redirect_allowlist.is_empty() {
            // A single leading slash, and no backslash: `//evil.example` is a
            // protocol-relative URL and `/\evil.example` is treated as one by
            // several browsers. Both are open redirects that look relative.
            let same_site = next.starts_with('/')
                && !next.starts_with("//")
                && !next.starts_with("/\\")
                && !next.contains('\\');
            if same_site {
                return Ok(next.to_owned());
            }
            return Err(ceremony(Cow::Owned(format!(
                "`{next}` is not a same-site path, and no redirect allowlist is configured; pass \
                 a path beginning with a single `/`, or call \
                 `.redirect_allowlist([\"https://app.example.com\"])`"
            ))));
        }

        if self
            .redirect_allowlist
            .iter()
            .any(|allowed| is_within(next, allowed))
        {
            return Ok(next.to_owned());
        }

        Err(ceremony(Cow::Owned(format!(
            "`{next}` is not in this provider's redirect allowlist ({:?})",
            self.redirect_allowlist
        ))))
    }
}

/// Whether `candidate` is at or under `allowed`, compared on origin and path
/// boundaries rather than as a string prefix.
///
/// A prefix comparison would let `https://app.example.com.evil.test` through
/// against an allowlist entry of `https://app.example.com`, which is the
/// classic way this check is written wrong.
fn is_within(candidate: &str, allowed: &str) -> bool {
    let (Ok(candidate), Ok(allowed)) = (Url::parse(candidate), Url::parse(allowed)) else {
        return false;
    };
    if candidate.scheme() != allowed.scheme() || candidate.host_str() != allowed.host_str() {
        return false;
    }
    if candidate.as_url().port_or_known_default() != allowed.as_url().port_or_known_default() {
        return false;
    }

    let allowed_path = allowed.as_url().path().trim_end_matches('/');
    if allowed_path.is_empty() {
        return true;
    }
    let path = candidate.as_url().path();
    path == allowed_path
        || path
            .strip_prefix(allowed_path)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Compare without returning early on the first differing byte.
fn constant_time_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq as _;
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// A ceremony failure.
fn ceremony(reason: Cow<'static, str>) -> Error {
    Error::Ceremony {
        ceremony: "oauth",
        reason,
    }
}

/// A configuration failure.
fn config(detail: impl Into<Cow<'static, str>>) -> Error {
    Error::Config(detail.into())
}

/// The provider could not be reached, or answered nonsense.
fn unavailable(detail: impl Into<String>) -> Error {
    Error::Unavailable {
        component: "identity provider",
        detail: detail.into(),
        source: None,
    }
}
