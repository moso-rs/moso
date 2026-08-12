//! Starting an OAuth2/OIDC flow, and coming back from one.
//!
//! Two routes, however many providers are configured: the provider is a path
//! parameter matched against the list [`AuthRoutes::oauth`](crate::AuthRoutes)
//! was given, so a name nobody configured is a 404 rather than a route that
//! exists and fails later.
//!
//! # What lives where
//!
//! ```text
//! GET /auth/oauth/{provider}
//!   └─ authorize      PKCE verifier, `state`, and an OIDC `nonce`
//!   └─ session        the flow, as a `StoredFlow` — see below
//!   └─ 303            to the provider
//!
//! GET /auth/oauth/{provider}/callback
//!   └─ session        the request this callback must belong to
//!   └─ exchange       checks `state`, then the code, then the id token
//!   └─ link           by verified email, or refuse                    (401)
//!   └─ session.log_in cycles the id; `SessionLayer` writes the cookie
//!   └─ 303            to the `next` this flow started with
//! ```
//!
//! The three secrets a flow carries — the PKCE verifier, the `state` and the
//! `nonce` — never leave the session. They are in the authorization URL's query
//! string too, which is why
//! [`AuthorizationRequest`](crate::AuthorizationRequest) prints its URL without
//! one.
//!
//! # `next` is validated twice, on purpose
//!
//! Once when the flow starts, before it is written to the session, and once
//! when the callback is about to redirect. The second check is what makes a
//! tampered session store useless as an open redirect.

use std::sync::Arc;

use moso_core::extract::{Path, Query};
use moso_core::response::Redirect;
use moso_core::{Depends, Inject, Router, SecretString};
use serde::{Deserialize, Serialize};

use super::AuthState;
use super::support;
use crate::{
    AccountStore, AuthSession, AuthUser, AuthorizationRequest, CallbackParams, DefaultUser, Error,
    NewAccount, OAuthProfile, Provider, ProviderId, Result,
};

/// Where a flow with no `next` lands.
const DEFAULT_NEXT: &str = "/";

/// An [`AuthorizationRequest`] in the shape a session can actually hold.
///
/// Its three secrets are [`SecretString`]s, and a `SecretString` refuses to
/// serialise — structurally, so no derived `Serialize` can put one on a wire by
/// accident. They still have to survive between the two halves of a flow, and
/// the session is where OAuth's own security model says to keep them: the whole
/// point of binding `state` and the PKCE verifier to a browser is that they are
/// stored per browser. This struct is the single place the refusal is stepped
/// around, and it is why the step is one conversion rather than a `#[serde]`
/// attribute somebody could copy onto something else.
#[derive(Debug, Deserialize, Serialize)]
struct StoredFlow {
    /// Where the browser was sent.
    url: String,
    /// The PKCE code verifier.
    verifier: String,
    /// The `state`, compared on the way back.
    state: String,
    /// The OIDC `nonce`, when the provider speaks OIDC.
    nonce: Option<String>,
    /// Where the flow was heading, already validated.
    next: Option<String>,
    /// Which provider this flow belongs to.
    provider: ProviderId,
}

impl StoredFlow {
    /// The storable form of an in-flight request.
    fn of(request: &AuthorizationRequest) -> Self {
        Self {
            url: request.url.as_str().to_owned(),
            verifier: request.verifier.expose().to_owned(),
            state: request.state.expose().to_owned(),
            nonce: request
                .nonce
                .as_ref()
                .map(|nonce| nonce.expose().to_owned()),
            next: request.next.clone(),
            provider: request.provider.clone(),
        }
    }

    /// The request back, with its secrets secret again.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`] when the stored URL no longer parses, which means the
    /// session was tampered with — the same refusal a mismatched `state` gets.
    fn restore(self) -> Result<AuthorizationRequest> {
        let url = moso_schema::Url::parse(&self.url)
            .map_err(|_| Error::ceremony("oauth", "the stored authorization URL does not parse"))?;

        Ok(AuthorizationRequest {
            url,
            verifier: SecretString::new(self.verifier),
            state: SecretString::new(self.state),
            nonce: self.nonce.map(SecretString::new),
            next: self.next,
            provider: self.provider,
        })
    }
}

/// The query string `GET /auth/oauth/{provider}` accepts.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct StartParams {
    /// Where to land once the provider comes back.
    pub(crate) next: Option<String>,
}

dto! {
    StartParams, "Where to land once the provider comes back.",
    next: Option<String> = "Where to land once the provider comes back.", false;
}

/// The query string a provider redirects back with.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct CallbackQuery {
    /// The authorization code, on success.
    pub(crate) code: Option<String>,
    /// The `state` this flow started with.
    pub(crate) state: Option<String>,
    /// The error code, when the user refused or the provider failed.
    pub(crate) error: Option<String>,
    /// The provider's description of the error.
    pub(crate) error_description: Option<String>,
}

dto! {
    CallbackQuery, "What a provider redirects back with.",
    code: Option<String> = "The authorization code, on success.", false;
    state: Option<String> = "The `state` this flow started with.", false;
    error: Option<String> = "The error code, when the user refused.", false;
    error_description: Option<String> = "The provider's description of the error.", false;
}

/// Mount the OAuth routes for `providers`.
pub(crate) fn mount(providers: Vec<Provider>, allowlist: Arc<[String]>) -> Router {
    let providers: Arc<[Provider]> = Arc::from(providers);
    let start_providers = Arc::clone(&providers);
    let start_allowlist = Arc::clone(&allowlist);

    Router::new()
        .get(
            "/auth/oauth/{provider}",
            move |Depends(AuthSession(session)): Depends<AuthSession>,
                  Path(provider): Path<String>,
                  Query(params): Query<StartParams>| {
                let providers = Arc::clone(&start_providers);
                let allowlist = Arc::clone(&start_allowlist);
                async move { start(&providers, &session, &provider, params, &allowlist).await }
            },
        )
        .get(
            "/auth/oauth/{provider}/callback",
            move |Inject(state): Inject<AuthState>,
                  Depends(AuthSession(session)): Depends<AuthSession>,
                  Path(provider): Path<String>,
                  Query(query): Query<CallbackQuery>| {
                let providers = Arc::clone(&providers);
                let allowlist = Arc::clone(&allowlist);
                async move {
                    callback(&state, &providers, &session, &provider, query, &allowlist).await
                }
            },
        )
        .tag(super::AUTH_TAG)
        .responds(401, super::unauthenticated_response())
        .responds(503, super::unavailable_response())
}

/// `GET /auth/oauth/{provider}` — send the browser to the provider.
async fn start(
    providers: &[Provider],
    session: &crate::Session,
    name: &str,
    params: StartParams,
    allowlist: &[String],
) -> moso_core::Result<Redirect> {
    let provider = find(providers, name)?;
    let next = support::checked_next(params.next.as_deref(), allowlist)?;

    let request = provider.authorize(next.as_deref()).await?;
    let url = request.url.as_str().to_owned();
    session.insert(&session_key(name), StoredFlow::of(&request))?;

    Ok(Redirect::to(url))
}

/// `GET /auth/oauth/{provider}/callback` — finish the flow and sign in.
async fn callback(
    state: &AuthState,
    providers: &[Provider],
    session: &crate::Session,
    name: &str,
    query: CallbackQuery,
    allowlist: &[String],
) -> moso_core::Result<Redirect> {
    let provider = find(providers, name)?;

    // Taken, not read: a callback consumes the flow it belongs to, so a
    // replayed one has nothing to check itself against.
    let request = session
        .take::<StoredFlow>(&session_key(name))?
        .ok_or(Error::InvalidCredentials)?
        .restore()?;

    let callback = CallbackParams {
        code: query.code,
        state: query.state.unwrap_or_default(),
        error: query.error,
        error_description: query.error_description,
    };

    let profile = provider.exchange(&request, &callback).await?;
    // `has_session` is what `LinkPolicy::VerifiedEmailOrSession` reads: an
    // already-authenticated request may link a provider whose email is not
    // verified, because the account is being proved a different way.
    provider.check_link(&profile, session.user_id().is_some())?;

    let user = link(state, &profile).await?;
    let identity = profile
        .verified_email()
        .map_or_else(|| profile.identity_key(), str::to_owned);
    super::password::finish(session, &user, &identity).await?;

    // Checked again on the way out. The first check was before the value was
    // stored; this one is after it came back out of a store.
    let next = support::checked_next(request.next.as_deref(), allowlist)?;
    Ok(Redirect::to(
        next.unwrap_or_else(|| DEFAULT_NEXT.to_owned()),
    ))
}

/// The account a provider profile belongs to, creating one if it is new.
///
/// The join key is the *verified* address when the provider gave one, and
/// `<provider>:<subject>` otherwise — never the unverified address, which is
/// the documented account-takeover path
/// [`LinkPolicy`](crate::LinkPolicy) exists to close.
async fn link(state: &AuthState, profile: &OAuthProfile) -> Result<DefaultUser> {
    let accounts = state.require_accounts()?;
    let identity = profile
        .verified_email()
        .map_or_else(|| profile.identity_key(), str::to_owned);
    let identity = crate::lifecycle::normalise(&identity);

    if let Some(user) = accounts.store().find_by_identity(&identity).await? {
        if !user.is_active() {
            return Err(Error::InvalidCredentials);
        }
        return Ok(user);
    }

    // A provider account has no password, and the column is not nullable in
    // every store, so it gets the hash of a value nobody holds. `password_hash`
    // returning this means every password login against the account fails the
    // verification, which is the intended behaviour.
    let unusable = crate::PasswordHash::new(&moso_schema::Password::from_trusted(
        support::opaque_token()?,
    ))
    .await?;
    accounts
        .store()
        .create(&NewAccount::new(identity, unusable).profile(profile.raw.clone()))
        .await
}

/// The provider this path names, or a 404.
///
/// A 404 rather than a 400: a name nobody configured is a path that does not
/// exist, and answering anything else would enumerate which providers a
/// deployment has.
fn find<'a>(providers: &'a [Provider], name: &str) -> moso_core::Result<&'a Provider> {
    providers
        .iter()
        .find(|provider| provider.id().as_str() == name)
        .ok_or_else(|| moso_core::Error::not_found("OAuth provider"))
}

/// The session key one provider's in-flight request occupies.
///
/// Scoped by provider, so starting a Google flow and then a GitHub one leaves
/// two independent requests rather than one that silently replaced the other.
fn session_key(name: &str) -> String {
    format!("_oauth_{name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OAuthConfig, ProviderId};

    fn google() -> Provider {
        Provider::google(OAuthConfig::new(
            "client-id",
            moso_core::SecretString::new("client-secret"),
            "https://app.example.com/auth/oauth/google/callback",
        ))
    }

    #[test]
    fn a_provider_nobody_configured_is_a_404_and_not_a_different_answer() {
        let providers = [google()];

        assert!(find(&providers, "google").is_ok());

        let error = find(&providers, "github").expect_err("not configured");
        assert_eq!(error.status(), 404);
    }

    #[test]
    fn two_providers_do_not_share_one_in_flight_request() {
        assert_ne!(session_key("google"), session_key("github"));
        assert!(session_key("google").starts_with('_'));
    }

    #[test]
    fn a_provider_is_matched_by_the_name_it_spells_itself_with() {
        assert_eq!(google().id().as_str(), ProviderId::Google.as_str());
    }

    // ── the reason `StoredFlow` exists ──────────────────────────────────────

    /// [`AuthorizationRequest`] derives `Serialize`, and that derive is a trap:
    /// three of its fields are [`SecretString`]s, which refuse to serialise, so
    /// putting the request into the session directly compiles and then fails at
    /// runtime — halfway through a login, on the machine of whoever deployed it.
    ///
    /// This is asserted rather than commented because the failure is invisible
    /// until a real flow runs, and the fix (`StoredFlow`) is easy to delete by
    /// someone tidying up what looks like a redundant conversion.
    #[tokio::test]
    async fn an_authorization_request_cannot_go_into_a_session_as_itself() {
        let request = google()
            .authorize(Some("/dashboard"))
            .await
            .expect("a same-site path needs no allowlist");

        assert!(
            serde_json::to_value(&request).is_err(),
            "if this ever succeeds, `SecretString` has started serialising and \
             every secret in the crate is one derive away from a log file"
        );
    }

    /// …and the conversion that steps around it carries every secret the
    /// callback compares against. Losing one of them silently turns the `state`
    /// check or the PKCE exchange into a no-op, which is the failure OAuth's
    /// binding exists to prevent.
    #[tokio::test]
    async fn a_stored_flow_round_trips_every_secret_the_callback_needs() {
        let request = google()
            .authorize(Some("/dashboard"))
            .await
            .expect("a same-site path needs no allowlist");

        let json =
            serde_json::to_value(StoredFlow::of(&request)).expect("the stored shape is JSON");
        let restored: StoredFlow = serde_json::from_value(json).expect("and reads back");
        let restored = restored.restore().expect("the stored URL still parses");

        assert_eq!(restored.state.expose(), request.state.expose());
        assert_eq!(restored.verifier.expose(), request.verifier.expose());
        assert_eq!(
            restored.nonce.as_ref().map(SecretString::expose),
            request.nonce.as_ref().map(SecretString::expose),
        );
        assert!(restored.nonce.is_some(), "google speaks OIDC");
        assert_eq!(restored.url.as_str(), request.url.as_str());
        assert_eq!(restored.next.as_deref(), Some("/dashboard"));
        assert_eq!(restored.provider, request.provider);
    }

    /// A session an attacker has rewritten is refused, not trusted: the stored
    /// URL is parsed again on the way back, and a value that no longer parses is
    /// the same refusal a mismatched `state` gets.
    #[test]
    fn a_tampered_stored_flow_is_refused_rather_than_restored() {
        let tampered = StoredFlow {
            url: "not a url".to_owned(),
            verifier: "v".to_owned(),
            state: "s".to_owned(),
            nonce: None,
            next: None,
            provider: ProviderId::Google,
        };

        let error = tampered
            .restore()
            .expect_err("a broken URL is not restored");
        assert!(matches!(error, Error::Ceremony { .. }));
    }
}
