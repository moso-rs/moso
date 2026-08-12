//! OAuth2 and OIDC social login.
//!
//! Three things are enforced structurally rather than left to the caller:
//!
//! 1. **PKCE always**, even for a confidential client. There is no constructor
//!    that omits it.
//! 2. **`state` is bound to the session** and `nonce` is verified for OIDC. A
//!    `state` that is merely random and merely compared defends against nothing
//!    if the attacker can pick both halves.
//! 3. **Unverified-email auto-linking is refused.** "Sign in with X" where X
//!    asserts an unverified address is a documented account-takeover path:
//!    anybody who can create an account at X claiming a victim's address takes
//!    over the victim's account here. Overriding it is possible, explicit, and
//!    documented as dangerous.
//!
//! # The flow
//!
//! ```no_run
//! use moso_auth::{CallbackParams, OAuthConfig, Provider};
//! use moso_core::config::SecretString;
//!
//! # async fn f(secret: SecretString) -> moso_auth::Result<()> {
//! let google = Provider::google(OAuthConfig::new(
//!     "client-id",
//!     secret,
//!     "https://app.example.com/auth/oauth/google/callback",
//! ));
//!
//! // GET /auth/oauth/google — send the browser on, keep the rest in the session.
//! let request = google.authorize(Some("/dashboard")).await?;
//! # let session_copy = request.clone();
//!
//! // GET /auth/oauth/google/callback — everything in `params` is untrusted.
//! # let params = CallbackParams::new(Some("code"), "state");
//! let profile = google.exchange(&session_copy, &params).await?;
//!
//! // …and only now may it be attached to an account.
//! google.check_link(&profile, /* already signed in? */ false)?;
//! # Ok(()) }
//! ```
//!
//! # The map
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`mod@http`] | [`HttpTransport`] and the bundled `rustls` client |
//! | [`mod@pkce`] | [`Pkce`] |
//! | [`mod@endpoints`] | the built-in provider table and [`Discovery`] |
//! | [`mod@idtoken`] | [`IdToken`], and the `nonce` and `aud` checks |
//! | [`mod@flow`] | [`AuthorizationRequest`], [`CallbackParams`], [`OAuthProfile`], [`TokenSet`] |
//! | [`mod@link`] | [`LinkPolicy`] and [`check_link`] |
//! | [`mod@provider`] | [`Provider`] |

pub mod endpoints;
pub mod flow;
pub mod http;
pub mod idtoken;
pub mod link;
pub mod pkce;
pub mod profile;
pub mod provider;

pub use self::endpoints::Discovery;
pub use self::flow::{AuthorizationRequest, CallbackParams, OAuthProfile, TokenSet};
pub use self::http::{HttpRequest, HttpResponse, HttpTransport, RustlsTransport};
pub use self::idtoken::{Audience, CLOCK_SKEW, IdToken, IdTokenClaims};
pub use self::link::{LinkPolicy, check_link};
pub use self::pkce::Pkce;
pub use self::provider::Provider;

use moso_core::config::SecretString;
use serde::{Deserialize, Serialize};

/// Which identity provider.
///
/// ```
/// use moso_auth::ProviderId;
///
/// assert_eq!(ProviderId::Google.as_str(), "google");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderId {
    /// Google.
    Google,
    /// GitHub.
    GitHub,
    /// Microsoft Entra ID.
    Microsoft,
    /// Apple.
    Apple,
    /// GitLab.
    GitLab,
    /// Discord.
    Discord,
    /// Slack.
    Slack,
    /// Anything with an OIDC discovery document.
    Oidc(String),
}

impl ProviderId {
    /// The name used in the callback path and in the log.
    ///
    /// ```
    /// use moso_auth::ProviderId;
    ///
    /// assert_eq!(ProviderId::GitHub.as_str(), "github");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Google => "google",
            Self::GitHub => "github",
            Self::Microsoft => "microsoft",
            Self::Apple => "apple",
            Self::GitLab => "gitlab",
            Self::Discord => "discord",
            Self::Slack => "slack",
            Self::Oidc(name) => name,
        }
    }

    /// Parse the segment out of a callback path.
    ///
    /// A name that is not one of the seven built-in ones becomes
    /// [`ProviderId::Oidc`], which is what a generic provider was registered
    /// as. The caller still has to look the id up in its own provider list —
    /// this only names it.
    ///
    /// ```
    /// use moso_auth::ProviderId;
    ///
    /// assert_eq!(ProviderId::parse("github"), ProviderId::GitHub);
    /// assert_eq!(
    ///     ProviderId::parse("keycloak"),
    ///     ProviderId::Oidc("keycloak".to_owned())
    /// );
    /// ```
    #[must_use]
    pub fn parse(name: &str) -> Self {
        match name {
            "google" => Self::Google,
            "github" => Self::GitHub,
            "microsoft" => Self::Microsoft,
            "apple" => Self::Apple,
            "gitlab" => Self::GitLab,
            "discord" => Self::Discord,
            "slack" => Self::Slack,
            other => Self::Oidc(other.to_owned()),
        }
    }
}

impl core::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The credentials and endpoints of one provider.
///
/// ```no_run
/// use moso_auth::OAuthConfig;
/// use moso_core::config::SecretString;
///
/// # fn f(secret: SecretString) {
/// let _ = OAuthConfig::new("client-id", secret, "https://app.example.com/auth/oauth/google/callback");
/// # }
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct OAuthConfig {
    /// The client identifier.
    pub client_id: String,
    /// The client secret, redacted in every `Debug` and log.
    pub client_secret: SecretString,
    /// Where the provider sends the user back. Registered with the provider,
    /// and compared against the request, so it cannot be redirected elsewhere.
    pub redirect_uri: String,
    /// Extra scopes beyond the provider's defaults.
    pub scopes: Vec<String>,
    /// The discovery document, for [`ProviderId::Oidc`].
    pub discovery_url: Option<String>,
    /// The Entra directory this application belongs to.
    ///
    /// `None` means Microsoft's multi-tenant `common` endpoint, which accepts
    /// **any** Microsoft account in the world and issues tokens from an issuer
    /// that varies with the account — so `iss` cannot be pinned. A
    /// single-tenant application should name its tenant; it is the difference
    /// between "our staff can sign in" and "anyone with a Hotmail address can".
    ///
    /// Ignored by every other provider.
    pub tenant: Option<String>,
}

impl OAuthConfig {
    /// Credentials and a redirect URI.
    ///
    /// ```no_run
    /// # use moso_auth::OAuthConfig;
    /// # use moso_core::config::SecretString;
    /// # fn f(s: SecretString) { let _ = OAuthConfig::new("id", s, "https://a/cb"); }
    /// ```
    #[must_use]
    pub fn new(
        client_id: impl Into<String>,
        client_secret: SecretString,
        redirect_uri: impl Into<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret,
            redirect_uri: redirect_uri.into(),
            scopes: Vec::new(),
            discovery_url: None,
            tenant: None,
        }
    }

    /// Fetch the endpoints from a discovery document instead of the built-in
    /// table.
    ///
    /// How a self-hosted GitLab, a Keycloak realm or one Entra directory is
    /// configured.
    ///
    /// ```no_run
    /// # use moso_auth::OAuthConfig;
    /// # fn f(c: OAuthConfig) {
    /// let _ = c.discovery("https://id.example.com/.well-known/openid-configuration");
    /// # }
    /// ```
    #[must_use]
    pub fn discovery(mut self, url: impl Into<String>) -> Self {
        self.discovery_url = Some(url.into());
        self
    }

    /// Restrict an Entra application to one directory.
    ///
    /// ```no_run
    /// # use moso_auth::OAuthConfig;
    /// # fn f(c: OAuthConfig) { let _ = c.tenant("contoso.onmicrosoft.com"); }
    /// ```
    #[must_use]
    pub fn tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }

    /// Ask for these scopes on top of the provider's defaults.
    ///
    /// ```no_run
    /// # use moso_auth::OAuthConfig;
    /// # fn f(c: OAuthConfig) { let _ = c.scopes(["https://www.googleapis.com/auth/calendar"]); }
    /// ```
    #[must_use]
    pub fn scopes<S: Into<String>>(mut self, scopes: impl IntoIterator<Item = S>) -> Self {
        self.scopes.extend(scopes.into_iter().map(Into::into));
        self
    }
}

#[cfg(test)]
mod tests;
