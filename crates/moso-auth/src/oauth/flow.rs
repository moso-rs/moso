//! What an authorization flow carries: the request that starts it, the
//! callback that ends it, and the profile and tokens it produced.
//!
//! The split matters. [`AuthorizationRequest`] holds three values that must
//! reach the session and must not reach the client — the PKCE verifier, the
//! `state` and the OIDC `nonce`. [`CallbackParams`] is entirely attacker
//! controlled: it arrives in a query string that anybody can construct.
//! Everything in between is the checking that turns the second into a
//! [`OAuthProfile`].

use moso_core::config::SecretString;
use moso_schema::Url;
use serde::{Deserialize, Serialize};

use crate::oauth::ProviderId;

/// What starting an authorization produced.
///
/// The URL goes to the browser. Everything else goes into the session and must
/// not leave the server: `verifier` is the PKCE secret and `state` is what binds
/// the callback to this browser.
///
/// ```no_run
/// use moso_auth::AuthorizationRequest;
///
/// # fn f(r: &AuthorizationRequest) {
/// let _ = &r.url;
/// # }
/// ```
#[derive(Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AuthorizationRequest {
    /// Where to send the browser.
    ///
    /// Its query string carries the `state` and the OIDC `nonce`, which is why
    /// this type's `Debug` prints the URL without one. Anybody who learns the
    /// `state` can have a callback of their own accepted.
    pub url: Url,
    /// The PKCE code verifier. Session only.
    pub verifier: SecretString,
    /// The `state`. Session only, and compared on the way back.
    pub state: SecretString,
    /// The OIDC `nonce`, when the provider speaks OIDC. Session only.
    pub nonce: Option<SecretString>,
    /// Where to send the user after a successful login. Validated against the
    /// allowlist *before* being stored, so an open redirect is not reachable
    /// even from a tampered session.
    pub next: Option<String>,
    /// Which provider this request was built for.
    ///
    /// Carried so that a callback arriving on `/auth/oauth/github/callback`
    /// with a session that started a Google flow is a refusal rather than an
    /// exchange against the wrong token endpoint.
    pub provider: ProviderId,
}

impl core::fmt::Debug for AuthorizationRequest {
    /// The three session secrets are `SecretString`s and redact themselves, but
    /// two of them are *also* in the URL's query string, where a derived
    /// `Debug` would print them in full. An `AuthorizationRequest` in a log
    /// line would then be a live authorization anybody could complete.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut endpoint = self.url.as_url().clone();
        endpoint.set_query(None);
        endpoint.set_fragment(None);

        f.debug_struct("AuthorizationRequest")
            .field("provider", &self.provider)
            .field("url", &format_args!("{endpoint}?***"))
            .field("verifier", &self.verifier)
            .field("state", &self.state)
            .field("nonce", &self.nonce)
            .field("next", &self.next)
            .finish()
    }
}

/// What the provider sent back.
///
/// Every field is attacker-controlled: this is a query string on a public
/// endpoint. Nothing here is trusted until
/// [`Provider::exchange`](crate::Provider::exchange) has compared `state`
/// against the session.
///
/// ```no_run
/// use moso_auth::CallbackParams;
///
/// # fn f(p: &CallbackParams) {
/// let _ = &p.state;
/// # }
/// ```
#[derive(Clone, Debug, Deserialize)]
#[non_exhaustive]
pub struct CallbackParams {
    /// The authorization code, on success.
    pub code: Option<String>,
    /// The `state` echoed back.
    pub state: String,
    /// The error code, when the user refused or the provider failed.
    pub error: Option<String>,
    /// The provider's description of the error.
    pub error_description: Option<String>,
}

impl CallbackParams {
    /// Build one from what a route handler parsed out of the query string.
    ///
    /// ```
    /// use moso_auth::CallbackParams;
    ///
    /// let params = CallbackParams::new(Some("the-code"), "the-state");
    /// assert_eq!(params.state, "the-state");
    /// ```
    #[must_use]
    pub fn new(code: Option<&str>, state: impl Into<String>) -> Self {
        Self {
            code: code.map(str::to_owned),
            state: state.into(),
            error: None,
            error_description: None,
        }
    }

    /// Build the refusal case, which is what arrives when a user presses
    /// "cancel".
    ///
    /// ```
    /// use moso_auth::CallbackParams;
    ///
    /// let params = CallbackParams::refused("access_denied", Some("the user said no"), "st");
    /// assert!(params.code.is_none());
    /// ```
    #[must_use]
    pub fn refused(
        error: impl Into<String>,
        description: Option<&str>,
        state: impl Into<String>,
    ) -> Self {
        Self {
            code: None,
            state: state.into(),
            error: Some(error.into()),
            error_description: description.map(str::to_owned),
        }
    }
}

/// Who the provider says this is.
///
/// ```no_run
/// use moso_auth::OAuthProfile;
///
/// # fn f(p: &OAuthProfile) {
/// let _ = p.email_verified;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OAuthProfile {
    /// Which provider.
    pub provider: ProviderId,
    /// The provider's own identifier for this account. The stable join key —
    /// **not** the email, which users change.
    pub subject: String,
    /// The address, when the provider gave one.
    pub email: Option<String>,
    /// Whether the provider says it verified that address.
    ///
    /// The field [`LinkPolicy::VerifiedEmailOrSession`](crate::LinkPolicy::VerifiedEmailOrSession)
    /// turns on. A provider that does not report verification is treated as
    /// unverified.
    pub email_verified: bool,
    /// A display name.
    pub name: Option<String>,
    /// An avatar.
    pub picture: Option<String>,
    /// The whole claim set, for an application that needs a provider-specific
    /// field.
    pub raw: serde_json::Value,
    /// The tokens, when the application will call the provider's API later.
    pub tokens: TokenSet,
}

impl OAuthProfile {
    /// The key a stored identity row is looked up by: provider plus subject.
    ///
    /// Never the address. A user who changes their Google address is the same
    /// account; a user whose address is reassigned to somebody else at a
    /// corporate provider is not.
    ///
    /// ```no_run
    /// # use moso_auth::OAuthProfile;
    /// # fn f(p: &OAuthProfile) -> String { p.identity_key() }
    /// ```
    #[must_use]
    pub fn identity_key(&self) -> String {
        format!("{}:{}", self.provider.as_str(), self.subject)
    }

    /// The address, if and only if the provider says it verified it.
    ///
    /// The address it is safe to match against an existing account. See
    /// [`check_link`](crate::check_link) for why the distinction is not
    /// cosmetic.
    ///
    /// ```no_run
    /// # use moso_auth::OAuthProfile;
    /// # fn f(p: &OAuthProfile) -> Option<&str> { p.verified_email() }
    /// ```
    #[must_use]
    pub fn verified_email(&self) -> Option<&str> {
        if self.email_verified {
            self.email.as_deref()
        } else {
            None
        }
    }
}

/// The tokens an exchange produced.
///
/// ```no_run
/// use moso_auth::TokenSet;
///
/// # fn f(t: &TokenSet) {
/// let _ = &t.access_token;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TokenSet {
    /// The access token.
    pub access_token: SecretString,
    /// The refresh token, when the provider issued one.
    pub refresh_token: Option<SecretString>,
    /// The OIDC identity token, already verified.
    pub id_token: Option<SecretString>,
    /// When the access token expires, as a Unix timestamp.
    pub expires_at: Option<i64>,
    /// The scopes actually granted, which may be fewer than those requested.
    pub scopes: Vec<String>,
}

impl TokenSet {
    /// Whether the provider granted every scope that was asked for.
    ///
    /// A provider may grant fewer — a user unticking "see your email address"
    /// on Google's consent screen is the common case — and an application that
    /// needs one has to find out here rather than from a 403 an hour later.
    ///
    /// ```
    /// use moso_auth::TokenSet;
    ///
    /// # fn f(t: &TokenSet) { let _ = t.granted(&["email"]); }
    /// ```
    #[must_use]
    pub fn granted(&self, required: &[&str]) -> bool {
        // An empty `scope` in the response means "everything you asked for",
        // per RFC 6749 §5.1: the parameter is only required when the grant
        // differs from the request.
        self.scopes.is_empty()
            || required
                .iter()
                .all(|needed| self.scopes.iter().any(|granted| granted == needed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(email: Option<&str>, verified: bool) -> OAuthProfile {
        OAuthProfile {
            provider: ProviderId::Google,
            subject: "1234567890".to_owned(),
            email: email.map(str::to_owned),
            email_verified: verified,
            name: None,
            picture: None,
            raw: serde_json::Value::Null,
            tokens: TokenSet {
                access_token: SecretString::new("token"),
                refresh_token: None,
                id_token: None,
                expires_at: None,
                scopes: Vec::new(),
            },
        }
    }

    /// The join key is provider plus subject, never the address: a user who
    /// changes their Google address is the same account, and an address
    /// reassigned at a corporate provider is a different person.
    #[test]
    fn the_identity_key_is_provider_and_subject() {
        assert_eq!(
            profile(Some("ada@example.com"), true).identity_key(),
            "google:1234567890"
        );
    }

    /// `verified_email` is the one accessor a linking decision may use.
    #[test]
    fn only_a_verified_address_is_offered_for_matching() {
        assert_eq!(
            profile(Some("ada@example.com"), true).verified_email(),
            Some("ada@example.com")
        );
        assert_eq!(
            profile(Some("ada@example.com"), false).verified_email(),
            None
        );
        assert_eq!(profile(None, true).verified_email(), None);
    }

    /// An empty `scope` in a token response means "as requested" (RFC 6749
    /// §5.1), so treating it as "nothing granted" would break every provider
    /// that omits it.
    #[test]
    fn an_absent_scope_means_everything_was_granted() {
        let mut tokens = profile(None, false).tokens;
        assert!(tokens.granted(&["email", "profile"]));

        tokens.scopes = vec!["openid".to_owned(), "profile".to_owned()];
        assert!(tokens.granted(&["openid"]));
        assert!(!tokens.granted(&["email"]));
        assert!(tokens.granted(&["openid", "profile"]));
    }

    /// The refusal constructor is what a route handler builds when the query
    /// string carries `error=access_denied` and no code.
    #[test]
    fn a_refusal_carries_no_code() {
        let params = CallbackParams::refused("access_denied", Some("nope"), "st");
        assert!(params.code.is_none());
        assert_eq!(params.error.as_deref(), Some("access_denied"));
        assert_eq!(params.error_description.as_deref(), Some("nope"));
    }

    /// A callback parses straight out of a query string, which is how a route
    /// handler will actually build it.
    #[test]
    fn a_callback_deserialises_from_a_query_string() {
        let params: CallbackParams =
            serde_urlencoded_lite("code=abc&state=xyz").expect("the query string parses");
        assert_eq!(params.code.as_deref(), Some("abc"));
        assert_eq!(params.state, "xyz");
        assert!(params.error.is_none());
    }

    /// A tiny query-string decoder, so this test does not add a dependency for
    /// one assertion.
    fn serde_urlencoded_lite(query: &str) -> Result<CallbackParams, serde_json::Error> {
        let map: serde_json::Map<String, serde_json::Value> =
            form_urlencoded::parse(query.as_bytes())
                .map(|(k, v)| (k.into_owned(), serde_json::Value::String(v.into_owned())))
                .collect();
        serde_json::from_value(serde_json::Value::Object(map))
    }
}
