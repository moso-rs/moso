//! Where each provider's endpoints are, and how to find them for one that is
//! not built in.
//!
//! Two ways to know a provider's endpoints, and this module has both:
//!
//! - **A table**, for the seven providers Moso ships. Hard-coding URLs is
//!   usually a smell; here it means one fewer network round trip on the first
//!   login of every process, and these seven have not moved in a decade.
//! - **A discovery document**, for everything else. `Provider::oidc` takes the
//!   `.well-known/openid-configuration` URL and reads the endpoints out of it,
//!   once, on first use.
//!
//! A built-in provider that is *also* given a discovery URL uses the document:
//! that is how a self-hosted GitLab or a single-tenant Entra directory is
//! configured, and the table is only the default.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use super::http::{HttpRequest, HttpTransport};
use crate::oauth::ProviderId;
use crate::{Error, Result};

/// The endpoints one authorization flow uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Endpoints {
    /// Where the browser is sent.
    pub authorization: String,
    /// Where the code is exchanged.
    pub token: String,
    /// Where the profile is read, when there is such an endpoint.
    pub userinfo: Option<String>,
    /// The `iss` an OIDC identity token must carry, when it is known ahead of
    /// time. `None` means "the issuer varies" — Entra's `common` endpoint is
    /// the case that matters — and the claim is then checked for presence and
    /// shape rather than for an exact value.
    pub issuer: Option<String>,
    /// Where the signing keys are, for a caller that wants to verify an
    /// identity token itself.
    pub jwks_uri: Option<String>,
    /// Whether this provider issues an OIDC identity token, which is what
    /// decides whether a `nonce` is sent and verified.
    pub oidc: bool,
}

/// The subset of an OIDC discovery document Moso reads.
///
/// The document has forty-odd fields; these are the ones an authorization code
/// flow uses. Unknown fields are ignored, so a provider adding one does not
/// break the parse.
///
/// ```no_run
/// use moso_auth::oauth::Discovery;
///
/// # fn f(d: &Discovery) {
/// let _ = &d.token_endpoint;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Discovery {
    /// The issuer, which an identity token's `iss` must equal exactly.
    pub issuer: String,
    /// Where the browser is sent.
    pub authorization_endpoint: String,
    /// Where the code is exchanged.
    pub token_endpoint: String,
    /// Where the profile is read.
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
    /// Where the signing keys are.
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// Which PKCE methods the provider supports.
    ///
    /// Read for one reason: a provider that advertises support and does not
    /// list `S256` cannot do PKCE properly, and Moso refuses rather than
    /// silently downgrading to `plain`.
    #[serde(default)]
    pub code_challenge_methods_supported: Option<Vec<String>>,
    /// Which scopes it offers, for a boot-time warning.
    #[serde(default)]
    pub scopes_supported: Option<Vec<String>>,
}

impl Discovery {
    /// The endpoints this document describes.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the provider advertises its PKCE methods and
    /// `S256` is not among them.
    pub(crate) fn endpoints(&self) -> Result<Endpoints> {
        if let Some(methods) = &self.code_challenge_methods_supported
            && !methods.iter().any(|m| m == "S256")
        {
            return Err(Error::Config(std::borrow::Cow::Owned(format!(
                "the provider at `{}` advertises PKCE methods {methods:?}, which do not include \
                 `S256`; Moso does not fall back to `plain`, because a `plain` challenge is the \
                 verifier and defends against nothing",
                self.issuer
            ))));
        }

        Ok(Endpoints {
            authorization: self.authorization_endpoint.clone(),
            token: self.token_endpoint.clone(),
            userinfo: self.userinfo_endpoint.clone(),
            issuer: Some(self.issuer.clone()),
            jwks_uri: self.jwks_uri.clone(),
            oidc: true,
        })
    }
}

/// The endpoints resolved once and then reused.
///
/// A built-in provider resolves without a network call; an OIDC one fetches its
/// discovery document on the first authorization and holds it for the life of
/// the process. `OnceCell` rather than a lock so that a hundred concurrent
/// logins on a cold process make one request, not a hundred.
#[derive(Clone, Debug, Default)]
pub(crate) struct Resolver {
    /// Filled on first use.
    cell: Arc<OnceCell<Endpoints>>,
}

impl Resolver {
    /// Fill the cache directly, for a provider whose endpoints the application
    /// already knows — a self-hosted instance with no discovery document.
    ///
    /// Setting it twice loses: whichever call arrives first wins, and there is
    /// no path that sets it to two different values.
    pub(crate) fn seed(&self, endpoints: Endpoints) {
        let _ = self.cell.set(endpoints);
    }

    /// The endpoints, fetching the discovery document if that is where they
    /// live.
    pub(crate) async fn resolve(
        &self,
        id: &ProviderId,
        tenant: Option<&str>,
        discovery_url: Option<&str>,
        transport: &dyn HttpTransport,
    ) -> Result<Endpoints> {
        if let Some(cached) = self.cell.get() {
            return Ok(cached.clone());
        }

        let resolved = match discovery_url {
            Some(url) => fetch_discovery(url, transport).await?.endpoints()?,
            None => builtin(id, tenant).ok_or_else(|| {
                Error::Config(std::borrow::Cow::Owned(format!(
                    "`{}` has no built-in endpoints, so it needs a discovery URL; build it with \
                     `Provider::oidc(\"{}\", \"https://…/.well-known/openid-configuration\", \
                     config)`",
                    id.as_str(),
                    id.as_str()
                )))
            })?,
        };

        // A race here is harmless: both futures computed the same value, and
        // `set` on a filled cell simply loses.
        let _ = self.cell.set(resolved.clone());
        Ok(resolved)
    }
}

/// Fetch and parse a discovery document.
async fn fetch_discovery(url: &str, transport: &dyn HttpTransport) -> Result<Discovery> {
    let response = transport.send(&HttpRequest::get(url)).await?;
    if !response.is_success() {
        return Err(Error::Unavailable {
            component: "identity provider",
            detail: format!(
                "the discovery document at `{url}` answered {} ({})",
                response.status,
                response.text().chars().take(200).collect::<String>()
            ),
            source: None,
        });
    }
    response.json::<Discovery>("discovery document")
}

/// The built-in endpoint table.
///
/// The comments are the part that ages: each entry says what the provider
/// asserts about an address, because that is what decides whether
/// [`LinkPolicy::VerifiedEmailOrSession`](crate::LinkPolicy::VerifiedEmailOrSession)
/// will let a login attach to an existing account.
pub(crate) fn builtin(id: &ProviderId, tenant: Option<&str>) -> Option<Endpoints> {
    Some(match id {
        // Google asserts `email_verified` and means it.
        ProviderId::Google => Endpoints {
            authorization: "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
            token: "https://oauth2.googleapis.com/token".to_owned(),
            userinfo: Some("https://openidconnect.googleapis.com/v1/userinfo".to_owned()),
            issuer: Some("https://accounts.google.com".to_owned()),
            jwks_uri: Some("https://www.googleapis.com/oauth2/v3/certs".to_owned()),
            oidc: true,
        },
        // GitHub is OAuth2 and not OIDC: no identity token, no nonce, and the
        // address needs a second call because `/user` only returns the *public*
        // one, which is often absent.
        ProviderId::GitHub => Endpoints {
            authorization: "https://github.com/login/oauth/authorize".to_owned(),
            token: "https://github.com/login/oauth/access_token".to_owned(),
            userinfo: Some("https://api.github.com/user".to_owned()),
            issuer: None,
            jwks_uri: None,
            oidc: false,
        },
        // Entra's issuer contains the tenant id, so `common` cannot be checked
        // against a fixed string. A single-tenant application should pass its
        // tenant, which pins the issuer.
        ProviderId::Microsoft => {
            let tenant = tenant.unwrap_or("common");
            Endpoints {
                authorization: format!(
                    "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize"
                ),
                token: format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"),
                userinfo: Some("https://graph.microsoft.com/oidc/userinfo".to_owned()),
                issuer: None,
                jwks_uri: Some(format!(
                    "https://login.microsoftonline.com/{tenant}/discovery/v2.0/keys"
                )),
                oidc: true,
            }
        }
        // Apple has no userinfo endpoint at all: the identity token *is* the
        // profile, and the name is sent once, on the first authorization, in
        // the form post. Its client secret is a signed JWT, not a string.
        ProviderId::Apple => Endpoints {
            authorization: "https://appleid.apple.com/auth/authorize".to_owned(),
            token: "https://appleid.apple.com/auth/token".to_owned(),
            userinfo: None,
            issuer: Some("https://appleid.apple.com".to_owned()),
            jwks_uri: Some("https://appleid.apple.com/auth/keys".to_owned()),
            oidc: true,
        },
        // gitlab.com. A self-hosted instance is `Provider::oidc` with its own
        // discovery URL.
        ProviderId::GitLab => Endpoints {
            authorization: "https://gitlab.com/oauth/authorize".to_owned(),
            token: "https://gitlab.com/oauth/token".to_owned(),
            userinfo: Some("https://gitlab.com/oauth/userinfo".to_owned()),
            issuer: Some("https://gitlab.com".to_owned()),
            jwks_uri: Some("https://gitlab.com/oauth/discovery/keys".to_owned()),
            oidc: true,
        },
        // Discord is OAuth2 only. It reports `verified`, which is whether the
        // address was confirmed by Discord.
        ProviderId::Discord => Endpoints {
            authorization: "https://discord.com/oauth2/authorize".to_owned(),
            token: "https://discord.com/api/oauth2/token".to_owned(),
            userinfo: Some("https://discord.com/api/users/@me".to_owned()),
            issuer: None,
            jwks_uri: None,
            oidc: false,
        },
        // Slack's "Sign in with Slack" is OIDC, on its own hostnames.
        ProviderId::Slack => Endpoints {
            authorization: "https://slack.com/openid/connect/authorize".to_owned(),
            token: "https://slack.com/api/openid.connect.token".to_owned(),
            userinfo: Some("https://slack.com/api/openid.connect.userInfo".to_owned()),
            issuer: Some("https://slack.com".to_owned()),
            jwks_uri: Some("https://slack.com/openid/connect/keys".to_owned()),
            oidc: true,
        },
        ProviderId::Oidc(_) => return None,
    })
}

/// The scopes a provider needs by default, before the application adds any.
pub(crate) fn default_scopes(id: &ProviderId) -> &'static [&'static str] {
    match id {
        ProviderId::Google | ProviderId::Microsoft | ProviderId::GitLab | ProviderId::Slack => {
            &["openid", "email", "profile"]
        }
        // GitHub's address is behind `user:email`, and `read:user` is the
        // narrowest scope that returns a profile.
        ProviderId::GitHub => &["read:user", "user:email"],
        // Discord's `identify` is the profile and `email` is the address.
        ProviderId::Discord => &["identify", "email"],
        // Apple returns nothing but `sub` without these, and asking for them
        // forces `response_mode=form_post`.
        ProviderId::Apple => &["name", "email"],
        ProviderId::Oidc(_) => &["openid", "email", "profile"],
    }
}

/// The published discovery URL of a built-in provider, when it has one.
///
/// Moso does not need it — the table above is the fast path — but it is the
/// right answer to "where do I point [`Provider::oidc`](crate::Provider::oidc)
/// for my own Entra directory or Google Workspace", and looking it up in a
/// vendor's documentation is five minutes nobody should spend twice.
///
/// `None` for GitHub and Discord, which are OAuth2 and not OpenID Connect, and
/// for a provider that is already generic.
///
/// ```
/// use moso_auth::ProviderId;
/// use moso_auth::oauth::endpoints::discovery_url;
///
/// assert_eq!(
///     discovery_url(&ProviderId::Google, None).as_deref(),
///     Some("https://accounts.google.com/.well-known/openid-configuration"),
/// );
/// assert!(discovery_url(&ProviderId::GitHub, None).is_none());
/// ```
#[must_use]
pub fn discovery_url(id: &ProviderId, tenant: Option<&str>) -> Option<String> {
    Some(match id {
        ProviderId::Google => {
            "https://accounts.google.com/.well-known/openid-configuration".to_owned()
        }
        ProviderId::Microsoft => format!(
            "https://login.microsoftonline.com/{}/v2.0/.well-known/openid-configuration",
            tenant.unwrap_or("common")
        ),
        ProviderId::Apple => {
            "https://appleid.apple.com/.well-known/openid-configuration".to_owned()
        }
        ProviderId::GitLab => "https://gitlab.com/.well-known/openid-configuration".to_owned(),
        ProviderId::Slack => "https://slack.com/.well-known/openid-configuration".to_owned(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every built-in provider resolves without a network call, and every
    /// endpoint it names is HTTPS. A `http:` endpoint in this table would send
    /// an authorization code in the clear.
    #[test]
    fn every_builtin_resolves_to_https_endpoints() {
        for id in [
            ProviderId::Google,
            ProviderId::GitHub,
            ProviderId::Microsoft,
            ProviderId::Apple,
            ProviderId::GitLab,
            ProviderId::Discord,
            ProviderId::Slack,
        ] {
            let endpoints = builtin(&id, None).unwrap_or_else(|| panic!("{id:?} is built in"));
            for url in [
                Some(&endpoints.authorization),
                Some(&endpoints.token),
                endpoints.userinfo.as_ref(),
                endpoints.jwks_uri.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                assert!(url.starts_with("https://"), "{id:?}: {url}");
            }
            assert!(
                !default_scopes(&id).is_empty(),
                "{id:?} must ask for something"
            );
        }
    }

    /// A generic OIDC provider has no table entry, and the error says what to
    /// do about it.
    #[test]
    fn a_generic_oidc_provider_has_no_builtin() {
        assert!(builtin(&ProviderId::Oidc("keycloak".to_owned()), None).is_none());
    }

    /// The Entra tenant is substituted into three URLs, so a single-tenant
    /// application is not silently talking to the multi-tenant endpoint.
    #[test]
    fn the_entra_tenant_is_substituted() {
        let common = builtin(&ProviderId::Microsoft, None).expect("built in");
        assert!(common.authorization.contains("/common/"));
        assert!(
            common.issuer.is_none(),
            "`common` issues tokens from many issuers, so `iss` cannot be pinned"
        );

        let tenant =
            builtin(&ProviderId::Microsoft, Some("contoso.onmicrosoft.com")).expect("built in");
        assert!(tenant.authorization.contains("contoso.onmicrosoft.com"));
        assert!(tenant.token.contains("contoso.onmicrosoft.com"));
        assert!(
            tenant
                .jwks_uri
                .as_deref()
                .is_some_and(|u| u.contains("contoso.onmicrosoft.com"))
        );
    }

    /// Apple has no userinfo endpoint; a mapper that assumed one would deref a
    /// `None` on every Apple login.
    #[test]
    fn apple_has_no_userinfo_endpoint() {
        assert!(
            builtin(&ProviderId::Apple, None)
                .expect("built in")
                .userinfo
                .is_none()
        );
    }

    /// The two non-OIDC providers must be marked as such, or a `nonce` would be
    /// sent to a provider that ignores it and then verified against an identity
    /// token that never arrives.
    #[test]
    fn the_non_oidc_providers_are_marked() {
        assert!(!builtin(&ProviderId::GitHub, None).expect("built in").oidc);
        assert!(!builtin(&ProviderId::Discord, None).expect("built in").oidc);
        assert!(builtin(&ProviderId::Google, None).expect("built in").oidc);
    }

    /// A discovery document turns into endpoints, and unknown fields are
    /// ignored rather than fatal.
    #[test]
    fn a_discovery_document_becomes_endpoints() {
        let document: Discovery = serde_json::from_str(
            r#"{
                "issuer": "https://id.example.com",
                "authorization_endpoint": "https://id.example.com/auth",
                "token_endpoint": "https://id.example.com/token",
                "userinfo_endpoint": "https://id.example.com/userinfo",
                "jwks_uri": "https://id.example.com/keys",
                "code_challenge_methods_supported": ["S256", "plain"],
                "something_new_in_2027": true
            }"#,
        )
        .expect("the document parses");

        let endpoints = document.endpoints().expect("S256 is supported");
        assert_eq!(endpoints.token, "https://id.example.com/token");
        assert_eq!(endpoints.issuer.as_deref(), Some("https://id.example.com"));
        assert!(endpoints.oidc);
    }

    /// A provider that advertises PKCE support without `S256` is refused, with
    /// the reason. Falling back to `plain` would leave the flow looking secure
    /// and defending against nothing.
    #[test]
    fn a_provider_without_s256_is_refused() {
        let document: Discovery = serde_json::from_str(
            r#"{
                "issuer": "https://id.example.com",
                "authorization_endpoint": "https://id.example.com/auth",
                "token_endpoint": "https://id.example.com/token",
                "code_challenge_methods_supported": ["plain"]
            }"#,
        )
        .expect("the document parses");

        let error = document
            .endpoints()
            .expect_err("plain-only must be refused");
        let message = format!("{error}");
        assert!(message.contains("S256"), "{message}");
        assert!(message.contains("plain"), "{message}");
    }

    /// A document that says nothing about PKCE is accepted — most providers
    /// support S256 and do not advertise it — but the challenge is still sent.
    #[test]
    fn a_silent_document_is_accepted() {
        let document: Discovery = serde_json::from_str(
            r#"{
                "issuer": "https://id.example.com",
                "authorization_endpoint": "https://id.example.com/auth",
                "token_endpoint": "https://id.example.com/token"
            }"#,
        )
        .expect("the document parses");
        assert!(document.endpoints().is_ok());
    }

    /// The discovery URLs point at `.well-known`, which is the one thing an
    /// operator copying them cares about.
    #[test]
    fn the_discovery_urls_are_well_known() {
        for id in [
            ProviderId::Google,
            ProviderId::Microsoft,
            ProviderId::Apple,
            ProviderId::GitLab,
            ProviderId::Slack,
        ] {
            let url = discovery_url(&id, None).unwrap_or_else(|| panic!("{id:?} has one"));
            assert!(url.contains("/.well-known/openid-configuration"), "{url}");
        }
        assert!(
            discovery_url(&ProviderId::GitHub, None).is_none(),
            "GitHub is not an OIDC provider"
        );
    }

    /// The resolver caches, so a hundred logins on a cold process make one
    /// discovery request.
    #[tokio::test]
    async fn the_resolver_caches() {
        struct Never;
        impl HttpTransport for Never {
            fn send<'a>(
                &'a self,
                _: &'a HttpRequest,
            ) -> moso_core::BoxFuture<'a, Result<super::super::http::HttpResponse>> {
                Box::pin(async { panic!("a cached resolver must not make a request") })
            }
        }

        let resolver = Resolver::default();
        let first = resolver
            .resolve(&ProviderId::Google, None, None, &Never)
            .await
            .expect("the table resolves");
        let second = resolver
            .resolve(&ProviderId::Google, None, None, &Never)
            .await
            .expect("the cache answers");
        assert_eq!(first, second);
    }
}
