//! The two WebAuthn ceremonies, as four routes.
//!
//! # The state lives in the session, and only in the session
//!
//! A ceremony is two requests, and the second one has to be checked against
//! what the first one generated. That state goes into the session — bound to
//! the browser that started the ceremony, expiring with the challenge's own
//! `expires_at`, and leaving no server-side table of half-finished ceremonies
//! for anybody to race. See [`StoredChallenge`] for the one line of that which
//! is not free.
//!
//! | Route | What it does |
//! | --- | --- |
//! | `POST /auth/passkeys/register/start` | options for a signed-in user to enrol a key |
//! | `POST /auth/passkeys/register/finish` | store the credential the browser made |
//! | `POST /auth/passkeys/login/start` | options for the discoverable (usernameless) flow |
//! | `POST /auth/passkeys/login/finish` | verify the assertion and mint a session |
//!
//! # All four check the wiring first, and the same way
//!
//! A ceremony needs a relying party *and* somewhere to keep the credential.
//! `login/start` needs only the relying party to produce options — which is
//! precisely why it checks for the store as well, in [`configured`], before it
//! writes anything into the session. A start that succeeded against a
//! deployment with no store would hand out a ceremony that nothing can finish:
//! a button that spins, an authenticator prompt the user answers, and then a
//! 500. A refusal on the first request is the honest answer, and it is a **501**
//! rather than a 500 because the condition is exactly the one that status
//! names — the operation is routed and not implemented here. A front end can
//! branch on it and hide the button; it cannot branch on a 500.
//!
//! The account store is not part of that check. It is the crate-wide
//! [`AuthState::accounts`](crate::AuthState::accounts) dependency that eight
//! other routes share, and a deployment missing it is not a deployment without
//! passkeys — it is a deployment without accounts. It keeps the 500 the rest of
//! the crate gives it.
//!
//! # Cloned authenticators
//!
//! [`WebAuthn::assert`](crate::WebAuthn::assert) refuses a credential whose
//! signature counter went backwards and reports it distinctly, because two
//! devices presenting one private key is not a "try again" failure. This module
//! is what acts on it: [`quarantine`] calls
//! [`PasskeyStore::disable`](crate::PasskeyStore::disable), so that *neither*
//! copy works until a person has looked, and logs
//! [`CLONE_EVENT`](crate::webauthn::CLONE_EVENT) at `ERROR` with the account and
//! the credential on it. Leaving the row live would make clone detection a log
//! line an attacker simply retries past with a plausible counter.
//!
//! The client is told nothing extra: the response is the same 401 a wrong
//! credential gets, because whether a key was copied is a fact about somebody
//! else's account.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use moso_core::extract::Json;
use moso_core::response::NoContent;
use moso_core::{Depends, ErrorKind, Inject, Router, SecretString};
use moso_openapi::builder::ResponseSpec;
use serde::{Deserialize, Serialize};

use super::support;
use super::{
    AuthState, OpaqueJson, PasskeyChallengeResponse, PasskeyFinishRequest, PasskeySummary,
};
use crate::webauthn::{CLONE_EVENT, is_clone_detected};
use crate::{
    AccountStore, AuthSession, AuthUser, Error, PasskeyCredential, PasskeyStore, Result, WebAuthn,
    WebAuthnChallenge,
};

/// The session key a registration ceremony's challenge is kept under.
const REGISTER_KEY: &str = "_passkey_register";

/// The session key an authentication ceremony's challenge is kept under.
const LOGIN_KEY: &str = "_passkey_login";

/// A [`WebAuthnChallenge`] in the shape a session can actually hold.
///
/// [`SecretString`] refuses to serialise — structurally, so that no derived
/// `Serialize` can put one on a wire by accident — and
/// [`WebAuthnChallenge::state`] is one. This struct is the single place that
/// refusal is stepped around, and the step is narrow on purpose: the value is a
/// ceremony state that lives for [`DEFAULT_TIMEOUT`](crate::webauthn::DEFAULT_TIMEOUT),
/// is bound to the browser that started the ceremony, and is useless without the
/// authenticator it names. `webauthn-rs` ships
/// `danger-allow-state-serialisation` for exactly this: without persisting it,
/// passkeys work only on a single process with sticky sessions.
#[derive(Debug, Deserialize, Serialize)]
struct StoredChallenge {
    /// What the browser was given. Not a secret — the browser has it.
    options: serde_json::Value,
    /// The ceremony state the finish step checks against.
    state: String,
    /// When the challenge stops being accepted.
    expires_at: DateTime<Utc>,
}

impl StoredChallenge {
    /// The storable form of a challenge.
    fn of(challenge: &WebAuthnChallenge) -> Self {
        Self {
            options: challenge.options.clone(),
            state: challenge.state.expose().to_owned(),
            expires_at: challenge.expires_at,
        }
    }

    /// The challenge back, with its state secret again.
    fn restore(self) -> WebAuthnChallenge {
        WebAuthnChallenge {
            options: self.options,
            state: SecretString::new(self.state),
            expires_at: self.expires_at,
        }
    }
}

/// Mount the passkey ceremonies.
pub(crate) fn mount() -> Router {
    Router::new()
        .post("/auth/passkeys/register/start", register_start)
        .post("/auth/passkeys/register/finish", register_finish)
        .post("/auth/passkeys/login/start", login_start)
        .post("/auth/passkeys/login/finish", login_finish)
        .tag(super::AUTH_TAG)
        .responds(401, super::unauthenticated_response())
        .responds(501, not_configured_response())
        .responds(503, super::unavailable_response())
}

/// The 501 all four routes answer when the flag was set and the dependencies
/// were not.
///
/// Declared on the group rather than per route because the condition is a
/// property of the deployment: either every passkey route can work or none can.
fn not_configured_response() -> ResponseSpec {
    ResponseSpec::problem(
        "this deployment mounted the passkey routes without a WebAuthn relying party or without a \
         passkey store, so no ceremony here can be completed",
    )
}

/// The relying party and the store, or the 501 naming what was not registered.
///
/// Called first by all four routes, before a challenge is generated and before
/// anything is written to the session. Splitting it out is what stops
/// `login/start` from being the one route that appears to work.
///
/// # Errors
///
/// A 501 whose detail is the sentence [`AuthState`] produced, naming the builder
/// call that fixes it. The detail stays server-side: 501 is a server error, so
/// `detail_is_client_safe` is false and only the status crosses the wire.
fn configured(state: &AuthState) -> moso_core::Result<(&WebAuthn, &Arc<dyn PasskeyStore>)> {
    let relying_party = state.require_webauthn().map_err(unconfigured)?;
    let store = state.require_passkeys().map_err(unconfigured)?;
    Ok((relying_party, store))
}

/// A missing registration, as the status that actually describes it.
///
/// `require_*` reports [`Error::Config`], which the crate's `From` impl renders
/// as a 500 — right for a contradictory configuration, wrong for this one. A
/// dependency that was never registered is not a bug in the request path; it is
/// an operation that is routed and not implemented on this deployment.
fn unconfigured(error: Error) -> moso_core::Error {
    moso_core::Error::new(ErrorKind::NotImplemented).with_detail(error.to_string())
}

/// `POST /auth/passkeys/register/start` — options for enrolling a key.
async fn register_start(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
) -> moso_core::Result<Json<PasskeyChallengeResponse>> {
    let (relying_party, store) = configured(&state)?;
    let subject = support::subject_of(&session)?;

    let account = session
        .get::<String>(super::password::IDENTITY_KEY)?
        .unwrap_or_else(|| subject.clone());
    // The keys already enrolled are excluded, so a browser that already holds
    // one offers to replace it rather than silently registering a duplicate.
    let existing = store.list_for_user(&subject).await?;

    let challenge = relying_party.start_registration(&subject, &account, &account, &existing)?;
    session.insert(REGISTER_KEY, StoredChallenge::of(&challenge))?;

    Ok(Json(PasskeyChallengeResponse {
        options: OpaqueJson(challenge.options.clone()),
    }))
}

/// `POST /auth/passkeys/register/finish` — store what the browser made.
async fn register_finish(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
    Json(body): Json<PasskeyFinishRequest>,
) -> moso_core::Result<Json<PasskeySummary>> {
    let (relying_party, store) = configured(&state)?;
    let subject = support::subject_of(&session)?;

    let challenge = take_challenge(&session, REGISTER_KEY)?;
    let mut credential =
        relying_party.finish_registration_for(&subject, &challenge, &body.response.0)?;
    credential.label = body.label.clone();

    store.insert(&credential).await?;

    Ok(Json(PasskeySummary {
        credential_id: credential.credential_id.clone(),
        label: credential.label.clone(),
        created_at: support::rfc3339(credential.created_at),
    }))
}

/// `POST /auth/passkeys/login/start` — options for the usernameless flow.
///
/// No identity is asked for and none is accepted: a start that took one would
/// answer differently for an address that has a passkey, which is an
/// enumeration oracle with a ceremony wrapped around it.
///
/// The store is required even though generating options does not read it, so
/// that this route cannot be the one that appears to work on a deployment where
/// `login/finish` cannot.
async fn login_start(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
) -> moso_core::Result<Json<PasskeyChallengeResponse>> {
    let (relying_party, _store) = configured(&state)?;

    let challenge = relying_party.start_authentication(&[])?;
    session.insert(LOGIN_KEY, StoredChallenge::of(&challenge))?;

    Ok(Json(PasskeyChallengeResponse {
        options: OpaqueJson(challenge.options.clone()),
    }))
}

/// `POST /auth/passkeys/login/finish` — verify the assertion and sign in.
async fn login_finish(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
    Json(body): Json<PasskeyFinishRequest>,
) -> moso_core::Result<NoContent> {
    let (relying_party, store) = configured(&state)?;
    // Not folded into `configured`: an account store is what every credential
    // flow needs, not something specific to passkeys, so a deployment without
    // one is broken rather than passkey-less.
    let accounts = state.require_accounts()?;

    let challenge = take_challenge(&session, LOGIN_KEY)?;

    // Client input, and treated as such: it names which stored credential to
    // check the signature against and proves nothing on its own.
    let discovered = relying_party.identify_discoverable(&body.response.0)?;
    let credential = store
        .find(&discovered.credential_id)
        .await?
        .ok_or(Error::InvalidCredentials)?;

    let assertion = match relying_party.assert(&challenge, &body.response.0, &credential) {
        Ok(assertion) => assertion,
        Err(error) if is_clone_detected(&error) => {
            quarantine(store, &credential).await;
            return Err(error.into());
        }
        Err(error) => return Err(error.into()),
    };
    store
        .update_counter(&credential.credential_id, assertion.sign_count)
        .await?;

    let id = crate::session::decode_subject::<String>(&credential.user_id)?;
    let user = accounts
        .store()
        .find_by_id(&id)
        .await?
        .ok_or(Error::InvalidCredentials)?;
    if !user.is_active() {
        return Err(Error::InvalidCredentials.into());
    }

    // A passkey proves the key, not the address, so the identity recorded on
    // the session is whatever the account already signed in with — or the
    // subject, which is what `/auth/email/change` will refuse to work from.
    let identity = session
        .get::<String>(super::password::IDENTITY_KEY)?
        .unwrap_or_else(|| credential.user_id.clone());
    super::password::finish(&session, &user, &identity).await?;

    Ok(NoContent)
}

/// Take a cloned credential out of service, and say so where an alert can see
/// it.
///
/// Called only when [`is_clone_detected`] says the signature counter went
/// backwards. Two devices are presenting one private key, and which of them is
/// the legitimate one is not knowable from here — so both are refused until a
/// person has looked. Advancing the stored counter instead, or leaving the row
/// alone, would let the next attempt with a plausible counter straight through,
/// which makes clone detection a log line rather than a defence.
///
/// This never fails the request it was called from. The request has already
/// failed: turning a store outage into a 503 on top of a 401 would only replace
/// the correct answer with a worse one.
async fn quarantine(store: &Arc<dyn PasskeyStore>, credential: &PasskeyCredential) {
    match store.disable(&credential.credential_id).await {
        Ok(true) => tracing::error!(
            target: "moso.auth",
            event = CLONE_EVENT,
            user_id = %credential.user_id,
            credential_id = %credential.credential_id,
            "a passkey's signature counter went backwards, so the private key exists on more than \
             one device; the credential is now disabled and the account holder has to be told"
        ),
        Ok(false) => tracing::warn!(
            target: "moso.auth",
            event = CLONE_EVENT,
            user_id = %credential.user_id,
            credential_id = %credential.credential_id,
            "a cloned passkey was presented again; it was already disabled"
        ),
        Err(error) => tracing::error!(
            target: "moso.auth",
            event = CLONE_EVENT,
            %error,
            user_id = %credential.user_id,
            credential_id = %credential.credential_id,
            "a passkey was detected as cloned and the store refused to disable it; it is still \
             live and has to be revoked by hand"
        ),
    }
}

/// Read a ceremony's challenge out of the session and consume it.
///
/// Consumed whether or not the finish succeeds: a challenge that survives a
/// failed attempt is a challenge an attacker can keep trying against.
///
/// # Errors
///
/// [`Error::InvalidCredentials`] when there is no challenge, and
/// [`Error::Expired`] when there is one and its window has closed.
fn take_challenge(session: &crate::Session, key: &str) -> Result<WebAuthnChallenge> {
    let challenge = session
        .take::<StoredChallenge>(key)?
        .ok_or(Error::InvalidCredentials)?
        .restore();

    if challenge.has_expired() {
        return Err(Error::Expired {
            kind: "webauthn challenge",
        });
    }
    Ok(challenge)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Duration;
    use http_body_util::BodyExt as _;
    use moso_core::config::{ConfigKey, SecretBytes};
    use moso_core::deps::axum;
    use moso_core::deps::tower::ServiceExt as _;
    use moso_core::middleware::Slot;
    use moso_core::{BootErrors, Config, ConfigDescriptor, ConfigLoader, Profile};

    use super::*;
    use crate::lifecycle::KvLifecycleTokens;
    use crate::store::MemoryPasskeyStore;
    use crate::webauthn::testing::{ORIGIN, RP_ID, VirtualBrowser};
    use crate::{
        DefaultUser, MemorySessionStore, NewAccount, PasswordHash, Session, SessionConfig,
        SessionLayer, SessionStore,
    };

    async fn session() -> crate::Session {
        let session = crate::Session::detached(
            crate::MemorySessionStore::shared(),
            SessionConfig::default(),
        );
        session.load().await.expect("loaded");
        session
    }

    fn challenge(expires_in: Duration) -> WebAuthnChallenge {
        WebAuthnChallenge {
            options: serde_json::json!({"challenge": "abc"}),
            state: SecretString::new("state"),
            expires_at: Utc::now() + expires_in,
        }
    }

    #[tokio::test]
    async fn a_ceremony_challenge_survives_a_round_trip_through_the_session() {
        let session = session().await;
        session
            .insert(
                REGISTER_KEY,
                StoredChallenge::of(&challenge(Duration::minutes(1))),
            )
            .expect("stored");

        let back = take_challenge(&session, REGISTER_KEY).expect("read");

        assert_eq!(back.options["challenge"], "abc");
        assert_eq!(back.state.expose(), "state");
    }

    #[tokio::test]
    async fn a_challenge_is_consumed_by_the_attempt_that_reads_it() {
        let session = session().await;
        session
            .insert(
                LOGIN_KEY,
                StoredChallenge::of(&challenge(Duration::minutes(1))),
            )
            .expect("stored");

        take_challenge(&session, LOGIN_KEY).expect("the first read");

        assert!(
            matches!(
                take_challenge(&session, LOGIN_KEY),
                Err(Error::InvalidCredentials)
            ),
            "a second attempt has nothing left to try against"
        );
    }

    #[tokio::test]
    async fn a_challenge_past_its_window_is_refused_rather_than_used() {
        let session = session().await;
        session
            .insert(
                LOGIN_KEY,
                StoredChallenge::of(&challenge(Duration::minutes(-1))),
            )
            .expect("stored");

        assert!(matches!(
            take_challenge(&session, LOGIN_KEY),
            Err(Error::Expired { .. })
        ));
    }

    #[tokio::test]
    async fn a_request_with_no_ceremony_in_flight_is_a_credential_failure() {
        let session = session().await;

        assert!(matches!(
            take_challenge(&session, REGISTER_KEY),
            Err(Error::InvalidCredentials)
        ));
    }

    // ── the application these routes are mounted on ───────────────────────

    /// The smallest thing that satisfies `App::new`.
    ///
    /// The passkey routes read nothing out of the application's configuration —
    /// every setting they honour lives on [`AuthState`] — so the root config is
    /// an empty struct rather than a fixture that would imply otherwise.
    #[derive(Debug)]
    struct TestConfig;

    impl Config for TestConfig {
        fn descriptor() -> &'static ConfigDescriptor {
            static DESCRIPTOR: ConfigDescriptor = ConfigDescriptor {
                type_name: "TestConfig",
                fields: &[],
            };
            &DESCRIPTOR
        }

        fn load_nested(_: &ConfigLoader, _: &ConfigKey, _: &mut BootErrors) -> Option<Self> {
            Some(TestConfig)
        }
    }

    /// One account row, with the columns [`AccountStore`] actually names.
    #[derive(Clone)]
    struct AccountRow {
        /// The address a login would present.
        identity: String,
        /// Its PHC hash, when it has one. A passkey-only account has none.
        hash: Option<String>,
        /// Bumped by "log out everywhere"; half of `auth_hash`.
        epoch: u32,
        /// Whether the address has been proved reachable.
        verified: bool,
    }

    /// The account store `login/finish` resolves a credential's owner through.
    ///
    /// A map rather than a table because what is under test here is the route
    /// wiring, and the passkey flow reads exactly one method of this trait. The
    /// other seven are implemented against the same map rather than left as
    /// stubs, so nothing in this fixture can return a plausible-looking wrong
    /// answer to a route that starts calling it.
    #[derive(Default)]
    struct MapAccounts {
        /// Rows by identifier.
        rows: std::sync::Mutex<HashMap<String, AccountRow>>,
    }

    impl MapAccounts {
        /// Take the lock, treating poisoning as "carry on with what is there":
        /// a poisoned fixture would fail every case with the wrong message.
        fn rows(&self) -> std::sync::MutexGuard<'_, HashMap<String, AccountRow>> {
            self.rows
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        /// Seed one account and hand back the principal a session logs in.
        fn add(&self, id: &str, identity: &str) -> DefaultUser {
            let row = AccountRow {
                identity: identity.to_owned(),
                hash: None,
                epoch: 0,
                verified: true,
            };
            self.rows().insert(id.to_owned(), row.clone());
            Self::user(id, &row)
        }

        /// The principal a row describes.
        fn user(id: &str, row: &AccountRow) -> DefaultUser {
            let mut material = row.hash.clone().unwrap_or_default().into_bytes();
            material.extend_from_slice(&row.epoch.to_le_bytes());
            DefaultUser::new(id, material)
        }
    }

    impl AccountStore for MapAccounts {
        type User = DefaultUser;

        fn find_by_identity<'a>(
            &'a self,
            identity: &'a str,
        ) -> moso_core::BoxFuture<'a, Result<Option<DefaultUser>>> {
            Box::pin(async move {
                Ok(self
                    .rows()
                    .iter()
                    .find(|(_, row)| row.identity == identity)
                    .map(|(id, row)| Self::user(id, row)))
            })
        }

        fn find_by_id<'a>(
            &'a self,
            id: &'a String,
        ) -> moso_core::BoxFuture<'a, Result<Option<DefaultUser>>> {
            Box::pin(async move { Ok(self.rows().get(id).map(|row| Self::user(id, row))) })
        }

        fn create<'a>(
            &'a self,
            account: &'a NewAccount,
        ) -> moso_core::BoxFuture<'a, Result<DefaultUser>> {
            Box::pin(async move {
                let id = format!("usr_{}", self.rows().len() + 1);
                let row = AccountRow {
                    identity: account.identity().to_owned(),
                    hash: Some(account.password_hash().as_str().to_owned()),
                    epoch: 0,
                    verified: false,
                };
                self.rows().insert(id.clone(), row.clone());
                Ok(Self::user(&id, &row))
            })
        }

        fn password_hash<'a>(
            &'a self,
            id: &'a String,
        ) -> moso_core::BoxFuture<'a, Result<Option<PasswordHash>>> {
            Box::pin(async move {
                self.rows()
                    .get(id)
                    .and_then(|row| row.hash.clone())
                    .map(|phc| PasswordHash::parse(&phc))
                    .transpose()
            })
        }

        fn set_password_hash<'a>(
            &'a self,
            id: &'a String,
            hash: &'a PasswordHash,
        ) -> moso_core::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if let Some(row) = self.rows().get_mut(id) {
                    row.hash = Some(hash.as_str().to_owned());
                }
                Ok(())
            })
        }

        fn set_identity<'a>(
            &'a self,
            id: &'a String,
            identity: &'a str,
        ) -> moso_core::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if let Some(row) = self.rows().get_mut(id) {
                    row.identity = identity.to_owned();
                }
                Ok(())
            })
        }

        fn mark_verified<'a>(&'a self, id: &'a String) -> moso_core::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if let Some(row) = self.rows().get_mut(id) {
                    row.verified = true;
                }
                Ok(())
            })
        }

        fn bump_epoch<'a>(&'a self, id: &'a String) -> moso_core::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if let Some(row) = self.rows().get_mut(id) {
                    row.epoch += 1;
                }
                Ok(())
            })
        }
    }

    /// Everything a case needs, with the application already booted.
    ///
    /// The routes are driven over HTTP through the composed service rather than
    /// called as functions: calling a handler skips the provider map, the
    /// session cookie, the body deserialisation and the error rendering, which
    /// is where the wiring this file owns actually is.
    struct Harness {
        /// The composed application.
        service: axum::Router<()>,
        /// The layer, kept so a case can mint the cookie a prior `/auth/login`
        /// would have left behind.
        layer: SessionLayer,
        /// Where sessions live, shared with the layer.
        sessions: Arc<dyn SessionStore>,
        /// Where credentials live, so a case can read back what a route wrote.
        passkeys: Arc<MemoryPasskeyStore>,
        /// The accounts a credential's owner is resolved through.
        accounts: Arc<MapAccounts>,
        /// The cookie the last response set, if any.
        cookie: Option<String>,
    }

    impl Harness {
        /// A fully configured deployment: a relying party, a passkey store and
        /// an account store.
        fn wired() -> Self {
            let passkeys = Arc::new(MemoryPasskeyStore::new());
            let accounts = Arc::new(MapAccounts::default());
            let sessions = MemorySessionStore::shared() as Arc<dyn SessionStore>;
            let state = AuthState::new(Arc::clone(&sessions))
                .webauthn(WebAuthn::new(RP_ID, ORIGIN, "Example"))
                .passkeys(Arc::clone(&passkeys) as Arc<dyn PasskeyStore>)
                .accounts(
                    Arc::clone(&accounts) as Arc<dyn AccountStore<User = DefaultUser>>,
                    KvLifecycleTokens::shared(
                        moso_kv::Kv::in_memory("passkeys").expect("an in-memory kv"),
                    ),
                );
            Self::boot(state, sessions, passkeys, accounts)
        }

        /// A deployment that set the flag and registered no store — the case
        /// `login/start` used to answer with a ceremony nothing could finish.
        fn without_a_store() -> Self {
            let passkeys = Arc::new(MemoryPasskeyStore::new());
            let accounts = Arc::new(MapAccounts::default());
            let sessions = MemorySessionStore::shared() as Arc<dyn SessionStore>;
            let state = AuthState::new(Arc::clone(&sessions))
                .webauthn(WebAuthn::new(RP_ID, ORIGIN, "Example"));
            Self::boot(state, sessions, passkeys, accounts)
        }

        /// Mount `state` on a real application with a real session layer.
        fn boot(
            state: AuthState,
            sessions: Arc<dyn SessionStore>,
            passkeys: Arc<MemoryPasskeyStore>,
            accounts: Arc<MapAccounts>,
        ) -> Self {
            let layer = SessionLayer::new(Arc::clone(&sessions), SessionConfig::default())
                .keys(vec![SecretBytes::new(vec![7; 32])]);
            let installed = layer.clone();
            let app = moso_core::App::new(TestConfig)
                .profile(Profile::Test)
                .provide(state)
                .with_middleware(move |stack| {
                    stack.replace_custom(Slot::Session, installed);
                })
                .mount(crate::routes().passkeys().build())
                .build()
                .expect("the test application boots");

            Self {
                service: app.into_service(),
                layer,
                sessions,
                passkeys,
                accounts,
                cookie: None,
            }
        }

        /// Present the cookie a completed password login would have left.
        ///
        /// Minted rather than obtained from `/auth/login`, because that route is
        /// a different flow with its own throttle, account store and password
        /// policy — none of which this file owns, and all of which would have to
        /// be configured to reach the one fact these cases need: a session that
        /// names a subject.
        async fn sign_in(&mut self, user: &DefaultUser) {
            let session = Session::detached(Arc::clone(&self.sessions), SessionConfig::default());
            session.load().await.expect("an empty session loads");
            session.log_in(user).await.expect("the session records who");
            session.save().await.expect("the session is written");

            self.cookie = Some(format!(
                "{}={}",
                SessionConfig::default().cookie.full_name(),
                self.layer.sign(&session.id())
            ));
        }

        /// `POST path`, carrying and capturing the session cookie.
        async fn post(
            &mut self,
            path: &str,
            body: &serde_json::Value,
        ) -> (http::StatusCode, serde_json::Value) {
            let mut request = http::Request::builder()
                .method(http::Method::POST)
                .uri(path)
                .header(http::header::CONTENT_TYPE, "application/json");
            if let Some(cookie) = self.cookie.as_deref() {
                request = request.header(http::header::COOKIE, cookie);
            }
            let request = request
                .body(axum::body::Body::from(body.to_string()))
                .expect("a well-formed request");

            let response = self
                .service
                .clone()
                .oneshot(request)
                .await
                .expect("the composed service answers");

            let status = response.status();
            if let Some(set) = response.headers().get(http::header::SET_COOKIE) {
                let value = set.to_str().expect("a printable Set-Cookie");
                self.cookie = value.split(';').next().map(str::to_owned);
            }
            let bytes = response
                .into_body()
                .collect()
                .await
                .expect("a readable body")
                .to_bytes();
            let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            (status, json)
        }
    }

    // ── the round trip, through the mounted routes ────────────────────────

    /// The acceptance criterion from `docs/03-batteries/30-auth.md`, driven the
    /// way a browser drives it: four HTTP requests against the mounted routes,
    /// with a virtual authenticator answering the two challenges. What this
    /// proves over the library-level round trip is the wiring — the provider
    /// map, the session the ceremony state lives in, the store the credential
    /// lands in, and the session the assertion mints.
    #[tokio::test]
    async fn a_passkey_registers_and_then_signs_in_through_the_mounted_routes() {
        let mut harness = Harness::wired();
        let user = harness.accounts.add("usr_1", "ada@example.com");
        harness.sign_in(&user).await;
        let mut browser = VirtualBrowser::new();

        // 1. Enrol.
        let (status, body) = harness
            .post("/auth/passkeys/register/start", &serde_json::json!({}))
            .await;
        assert_eq!(status, http::StatusCode::OK, "{body}");
        let created = browser.register(&options_of(&body));

        let (status, body) = harness
            .post(
                "/auth/passkeys/register/finish",
                &serde_json::json!({ "response": created, "label": "Ada's laptop" }),
            )
            .await;
        assert_eq!(status, http::StatusCode::OK, "{body}");
        assert_eq!(body["label"], "Ada's laptop");

        let credential_id = body["credential_id"]
            .as_str()
            .expect("the summary names the credential")
            .to_owned();
        let stored = harness
            .passkeys
            .find(&credential_id)
            .await
            .expect("the store answers")
            .expect("the route wrote the credential");
        assert_eq!(stored.user_id, "usr_1", "filed under the signed-in account");
        assert!(stored.is_active());

        // 2. Sign in with it, from a browser that has never logged in.
        harness.cookie = None;
        let (status, body) = harness
            .post("/auth/passkeys/login/start", &serde_json::json!({}))
            .await;
        assert_eq!(status, http::StatusCode::OK, "{body}");
        assert!(
            harness.cookie.is_some(),
            "the challenge has to be kept in a session, so the start sets a cookie"
        );

        // The soft authenticator has no resident-key store, so the browser is
        // told which credential to use and fills in the handle a compliant one
        // returns. The signature itself is the authenticator's own.
        let challenge = options_of(&body);
        let mut assertion = browser.authenticate(&crate::webauthn::testing::with_allow_list(
            &challenge, &stored,
        ));
        assertion["response"]["userHandle"] = serde_json::Value::String(stored.user_handle.clone());

        let (status, body) = harness
            .post(
                "/auth/passkeys/login/finish",
                &serde_json::json!({ "response": assertion }),
            )
            .await;
        assert_eq!(status, http::StatusCode::NO_CONTENT, "{body}");

        // 3. The counter moved, and the session names the account.
        let after = harness
            .passkeys
            .find(&credential_id)
            .await
            .expect("the store answers")
            .expect("still there");
        assert!(
            after.sign_count > stored.sign_count,
            "the route must persist the counter the ceremony returned: {} <= {}",
            after.sign_count,
            stored.sign_count
        );
        assert_eq!(
            signed_in_subject(&harness).await.as_deref(),
            Some("usr_1"),
            "the assertion has to mint a session for the credential's owner"
        );
    }

    /// The gap this closes: `login/start` needs only the relying party to
    /// produce options, so it used to succeed on a deployment where nothing
    /// could finish the ceremony — a worse failure than an error, because it
    /// looks like it works.
    #[tokio::test]
    async fn login_start_refuses_when_no_passkey_store_is_registered() {
        let mut harness = Harness::without_a_store();

        let (status, _) = harness
            .post("/auth/passkeys/login/start", &serde_json::json!({}))
            .await;

        assert_eq!(
            status,
            http::StatusCode::NOT_IMPLEMENTED,
            "a ceremony that cannot be finished must not be started"
        );
        assert!(
            harness.cookie.is_none(),
            "a refused start must not leave a challenge in a session"
        );
    }

    /// All four answer the same way, so a front end learns "this deployment
    /// does not do passkeys" from whichever one it asks first.
    #[tokio::test]
    async fn every_passkey_route_answers_501_on_a_deployment_with_no_store() {
        let mut harness = Harness::without_a_store();
        let body = serde_json::json!({ "response": {} });

        for path in [
            "/auth/passkeys/register/start",
            "/auth/passkeys/register/finish",
            "/auth/passkeys/login/start",
            "/auth/passkeys/login/finish",
        ] {
            let (status, _) = harness.post(path, &body).await;
            assert_eq!(status, http::StatusCode::NOT_IMPLEMENTED, "{path}");
        }
    }

    /// The sentence that fixes it is for the operator, not the client: 501 is a
    /// server error, so the detail stays in the log and out of the response.
    #[tokio::test]
    async fn the_501_does_not_tell_a_client_how_the_deployment_is_configured() {
        let mut harness = Harness::without_a_store();

        let (status, body) = harness
            .post("/auth/passkeys/login/start", &serde_json::json!({}))
            .await;

        assert_eq!(status, http::StatusCode::NOT_IMPLEMENTED);
        let printed = body.to_string();
        assert!(!printed.contains("AuthState"), "{printed}");
        assert!(!printed.contains("passkey store"), "{printed}");
    }

    // ── cloned authenticators ────────────────────────────────────────────

    /// A signature counter that goes backwards means the private key exists
    /// twice. The route must refuse *and* quarantine: leaving the row live turns
    /// clone detection into a log line the attacker retries past.
    #[tokio::test]
    async fn a_cloned_credential_is_refused_and_taken_out_of_service() {
        let mut harness = Harness::wired();
        let user = harness.accounts.add("usr_1", "ada@example.com");
        harness.sign_in(&user).await;
        let mut browser = VirtualBrowser::new();

        let (_, body) = harness
            .post("/auth/passkeys/register/start", &serde_json::json!({}))
            .await;
        let created = browser.register(&options_of(&body));
        let (_, body) = harness
            .post(
                "/auth/passkeys/register/finish",
                &serde_json::json!({ "response": created }),
            )
            .await;
        let credential_id = body["credential_id"]
            .as_str()
            .expect("the summary names the credential")
            .to_owned();

        // What a clone looks like from the server's side: the *other* copy has
        // already signed with a higher counter, so the stored one is ahead of
        // the authenticator now presenting itself.
        harness
            .passkeys
            .update_counter(&credential_id, 5_000)
            .await
            .expect("the store takes the counter");

        harness.cookie = None;
        let (_, body) = harness
            .post("/auth/passkeys/login/start", &serde_json::json!({}))
            .await;
        let stored = harness
            .passkeys
            .find(&credential_id)
            .await
            .expect("the store answers")
            .expect("still there");
        let mut assertion = browser.authenticate(&crate::webauthn::testing::with_allow_list(
            &options_of(&body),
            &stored,
        ));
        assertion["response"]["userHandle"] = serde_json::Value::String(stored.user_handle.clone());

        let (status, _) = harness
            .post(
                "/auth/passkeys/login/finish",
                &serde_json::json!({ "response": assertion }),
            )
            .await;

        assert_eq!(
            status,
            http::StatusCode::UNAUTHORIZED,
            "a client is told the same thing every credential failure is told"
        );
        let after = harness
            .passkeys
            .find(&credential_id)
            .await
            .expect("the store answers")
            .expect("a quarantined credential is kept");
        assert!(
            !after.is_active(),
            "the cloned credential has to be disabled, or the next attempt with a plausible \
             counter walks straight in"
        );
    }

    /// And it stays out of service: a disabled credential is refused before its
    /// signature is looked at, so the copy that *was* legitimate cannot silently
    /// resume either.
    #[tokio::test]
    async fn a_quarantined_credential_cannot_sign_in_afterwards() {
        let mut harness = Harness::wired();
        let user = harness.accounts.add("usr_1", "ada@example.com");
        harness.sign_in(&user).await;
        let mut browser = VirtualBrowser::new();

        let (_, body) = harness
            .post("/auth/passkeys/register/start", &serde_json::json!({}))
            .await;
        let created = browser.register(&options_of(&body));
        let (_, body) = harness
            .post(
                "/auth/passkeys/register/finish",
                &serde_json::json!({ "response": created }),
            )
            .await;
        let credential_id = body["credential_id"]
            .as_str()
            .expect("the summary names the credential")
            .to_owned();
        harness
            .passkeys
            .disable(&credential_id)
            .await
            .expect("the store quarantines it");

        harness.cookie = None;
        let (_, body) = harness
            .post("/auth/passkeys/login/start", &serde_json::json!({}))
            .await;
        let stored = harness
            .passkeys
            .find(&credential_id)
            .await
            .expect("the store answers")
            .expect("still there");
        let mut assertion = browser.authenticate(&crate::webauthn::testing::with_allow_list(
            &options_of(&body),
            &stored,
        ));
        assertion["response"]["userHandle"] = serde_json::Value::String(stored.user_handle.clone());

        let (status, _) = harness
            .post(
                "/auth/passkeys/login/finish",
                &serde_json::json!({ "response": assertion }),
            )
            .await;

        assert_eq!(status, http::StatusCode::UNAUTHORIZED);
        assert!(
            harness.cookie.is_some(),
            "there is a session to inspect; without one the next assertion proves nothing"
        );
        assert_eq!(
            signed_in_subject(&harness).await,
            None,
            "a disabled credential must not mint a session"
        );
    }

    // ── the small helpers the cases above read through ───────────────────

    /// The ceremony options out of a challenge response body.
    fn options_of(body: &serde_json::Value) -> WebAuthnChallenge {
        WebAuthnChallenge {
            options: body["options"].clone(),
            // The finish step reads the state from the session, never from the
            // response — which is the property being relied on here.
            state: SecretString::new(""),
            expires_at: Utc::now() + Duration::minutes(5),
        }
    }

    /// Whom the cookie the harness is holding signs in, if anybody.
    async fn signed_in_subject(harness: &Harness) -> Option<String> {
        let value = harness.cookie.as_deref()?.split_once('=')?.1.to_owned();
        let id = harness.layer.verify(&value)?;
        let record = harness.sessions.load(&id).await.ok()??;
        record.user_id
    }
}
