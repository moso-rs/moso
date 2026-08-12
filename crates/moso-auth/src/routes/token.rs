//! The bearer-token flow: exchange a password for a signed access token and an
//! opaque refresh token, then rotate the refresh token for the next pair.
//!
//! # Two routes, one shape
//!
//! ```text
//! POST /auth/token
//!   └─ gate            per-address quota, then per-identity backoff   (429)
//!   └─ find identity   one round trip, hit or miss
//!   └─ verify          a real hash, or `dummy_verify` — the same cost  (401)
//!   └─ second factor   when one is enrolled; the code rides this same request
//!   └─ issue           refresh (opaque, stored hashed) + access (short JWT)
//!   └─ record          the outcome, which never fails the request
//!
//! POST /auth/refresh
//!   └─ exchange        compare-and-set; a replayed token burns the family (401)
//! ```
//!
//! # Why this is separate from `/auth/login`
//!
//! `/auth/login` sets an `HttpOnly` cookie and is a browser flow;
//! [`SessionLayer`](crate::SessionLayer) turns its session into a `Set-Cookie`.
//! This flow sets **no** cookie and touches no session: it is for a client that
//! holds its own credentials — a CLI, a mobile app, a service — and stores the
//! refresh token itself. Keeping them apart is what lets the cookie flow return
//! no token in its body (where JavaScript could read it) while this one returns
//! both.
//!
//! # What each route reuses
//!
//! The credential half is the same work `/auth/login` does — the throttle
//! [`gate`](super::support::gate), the miss-path
//! [`dummy_verify`](crate::password::dummy_verify) that keeps "no such account"
//! as slow as "wrong password", and the TOTP check — so the two cannot drift on
//! how a login can fail. The token half is
//! [`RefreshStore::issue`](crate::RefreshStore::issue) and
//! [`RefreshStore::exchange`](crate::RefreshStore::exchange), which already hold
//! the reuse detection; this module only makes them reachable over HTTP.

use moso_core::extract::ClientIp;
use moso_core::extract::{Headers, Json};
use moso_core::{Inject, Router};

use super::support::{self, ClientHeaders};
use super::{AuthState, LoginResponse, RefreshRequest, TokenIssueRequest};
use crate::{AccountStore, AuthUser, Claims, Error, RefreshOutcome, Result};

/// Mount `POST /auth/token` and `POST /auth/refresh`.
///
/// `/auth/token` is throttled exactly as `/auth/login` is — it takes an identity
/// and a password from an unauthenticated caller, so it documents the same 429.
/// `/auth/refresh` is not: the refresh token it takes is 256 bits of opaque
/// entropy, so there is nothing for a per-identity backoff to protect and no
/// identity to key one on. Its defence is the reuse detection, which revokes the
/// family the moment a copied token is presented twice.
pub(crate) fn mount() -> Router {
    let issue = Router::new()
        .post("/auth/token", issue)
        .tag(super::AUTH_TAG)
        .responds(401, super::unauthenticated_response())
        .responds(429, super::throttled_response())
        .responds(503, super::unavailable_response());

    let refresh = Router::new()
        .post("/auth/refresh", refresh)
        .tag(super::AUTH_TAG)
        .responds(401, super::unauthenticated_response())
        .responds(503, super::unavailable_response());

    issue.merge(refresh)
}

// ---------------------------------------------------------------------------
// Issue
// ---------------------------------------------------------------------------

/// `POST /auth/token` — one answer for every way it can fail.
async fn issue(
    Inject(state): Inject<AuthState>,
    address: Option<ClientIp>,
    Headers(headers): Headers<ClientHeaders>,
    Json(body): Json<TokenIssueRequest>,
) -> moso_core::Result<Json<LoginResponse>> {
    let ctx = support::auth_ctx(address.as_ref(), &headers, Some(&body.identity));
    support::gate(&state, &ctx, headers.x_captcha_response.as_deref()).await?;

    match authenticate(&state, &body).await {
        Ok(response) => {
            support::record(&state, &ctx, true).await;
            Ok(Json(response))
        }
        Err(error) => {
            support::record(&state, &ctx, error.counts_as_attempt()).await;
            Err(error.into())
        }
    }
}

/// Verify the credentials and, when every factor is cleared, mint a pair.
///
/// Every failure is spelled [`Error::InvalidCredentials`], so "no such account",
/// "wrong password" and "suspended" are one answer with one timing.
async fn authenticate(state: &AuthState, body: &TokenIssueRequest) -> Result<LoginResponse> {
    let accounts = state.require_accounts()?;
    let identity = crate::lifecycle::normalise(&body.identity);
    let found = accounts.store().find_by_identity(&identity).await?;

    // The miss path pays for a verification too, so "no such account" is not a
    // faster 401 than "wrong password".
    let user = match found {
        Some(user) => match accounts.store().password_hash(&user.auth_id()).await? {
            Some(hash) if hash.verify(&body.password).await?.is_valid() => user,
            Some(_) => return Err(Error::InvalidCredentials),
            None => {
                crate::password::dummy_verify().await?;
                return Err(Error::InvalidCredentials);
            }
        },
        None => {
            crate::password::dummy_verify().await?;
            return Err(Error::InvalidCredentials);
        }
    };

    // Checked after the verification, so a suspended account is not a faster
    // answer than a wrong password either.
    if !user.is_active() {
        return Err(Error::InvalidCredentials);
    }

    let subject = crate::session::encode_subject(&user.auth_id())?;

    if let Some(mut enrolment) = super::totp::confirmed(state, &subject).await? {
        let Some(code) = body.totp.as_deref() else {
            // A second factor is enrolled but no code rode this request. Say so
            // — without saying anything a wrong password would not — so a client
            // knows to retry with the code rather than concluding its password
            // was wrong.
            return Ok(second_factor_required());
        };
        if !enrolment.check(code)? {
            return Err(Error::InvalidCredentials);
        }
        // The period the code came from is written back, which is what makes a
        // code single-use: replaying it inside its own thirty seconds is
        // refused.
        super::totp::save_confirmed(state, &subject, &enrolment).await?;
    }

    issue_pair(state, &subject).await
}

/// Mint the first pair of a new family.
///
/// The refresh token is opaque and 256 bits, minted and stored hashed by the
/// [`RefreshStore`](crate::RefreshStore); the access token is a short-lived JWT
/// signed by the configured [`Jwt`](crate::Jwt). The two `ttl`s come from
/// [`JwtConfig`](crate::JwtConfig), so "how long is this good for" is one
/// decision made in one place.
async fn issue_pair(state: &AuthState, subject: &str) -> Result<LoginResponse> {
    let jwt = state.require_jwt()?;
    let refresh_store = state.require_refresh()?;

    let refresh = refresh_store
        .issue(subject, jwt.config().refresh_ttl)
        .await?;
    let access = jwt.issue(&Claims::new(subject), jwt.config().access_ttl)?;

    Ok(LoginResponse {
        requires_second_factor: false,
        challenge: None,
        access_token: Some(access),
        refresh_token: Some(refresh.expose().to_owned()),
        next: None,
    })
}

/// The response that asks for the second factor, carrying no tokens.
fn second_factor_required() -> LoginResponse {
    LoginResponse {
        requires_second_factor: true,
        challenge: None,
        access_token: None,
        refresh_token: None,
        next: None,
    }
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

/// `POST /auth/refresh` — rotate a refresh token, or refuse it.
///
/// A [`RefreshOutcome::Rotated`] returns the next pair. Both other outcomes are
/// the same 401: an unknown or expired token and a replayed one are
/// indistinguishable to the client, and the replay has already burned the family
/// and emitted its audit event inside
/// [`exchange`](crate::RefreshStore::exchange). Saying which one it was would
/// tell an attacker holding a stolen token whether the legitimate client has
/// used it yet.
async fn refresh(
    Inject(state): Inject<AuthState>,
    Json(body): Json<RefreshRequest>,
) -> moso_core::Result<Json<LoginResponse>> {
    let store = state.require_refresh()?;

    match store.exchange(&body.refresh_token).await? {
        RefreshOutcome::Rotated { access, refresh } => Ok(Json(LoginResponse {
            requires_second_factor: false,
            challenge: None,
            access_token: Some(access),
            refresh_token: Some(refresh.expose().to_owned()),
            next: None,
        })),
        RefreshOutcome::ReuseDetected { .. } | RefreshOutcome::Invalid => {
            Err(Error::InvalidCredentials.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use http_body_util::BodyExt as _;
    use moso_core::config::{ConfigKey, SecretBytes};
    use moso_core::deps::axum;
    use moso_core::deps::tower::ServiceExt as _;
    use moso_core::{BootErrors, Config, ConfigDescriptor, ConfigLoader, Profile};
    use moso_schema::Password;

    use super::*;
    use crate::lifecycle::KvLifecycleTokens;
    use crate::store::MemorySessionStore;
    use crate::{
        AuthState, Claims, DefaultUser, HashParams, Jwt, JwtAlgorithm, JwtConfig,
        MemoryRefreshStore, NewAccount, PasswordHash, SessionStore,
    };

    /// The plaintext the seeded account signs in with. Twelve characters, so it
    /// clears the default policy the extractor enforces.
    const SECRET: &str = "correct horse battery";

    /// The smallest thing that satisfies `App::new`. The bearer routes read
    /// nothing out of the root configuration — every setting lives on
    /// [`AuthState`] — so it is empty.
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

    /// A one-row account store: enough for the credential half of `/auth/token`,
    /// with the other seven methods implemented against the same map so nothing
    /// can return a plausible wrong answer.
    #[derive(Default)]
    struct MapAccounts {
        /// Rows by id: `(identity, phc)`.
        rows: std::sync::Mutex<HashMap<String, (String, String)>>,
    }

    impl MapAccounts {
        fn rows(&self) -> std::sync::MutexGuard<'_, HashMap<String, (String, String)>> {
            self.rows
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        /// Seed one account and return the principal it describes.
        fn seed(&self, id: &str, identity: &str, phc: &str) -> DefaultUser {
            self.rows()
                .insert(id.to_owned(), (identity.to_owned(), phc.to_owned()));
            DefaultUser::new(id, phc.as_bytes().to_vec())
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
                    .find(|(_, (id, _))| id == identity)
                    .map(|(key, (_, phc))| DefaultUser::new(key, phc.as_bytes().to_vec())))
            })
        }

        fn find_by_id<'a>(
            &'a self,
            id: &'a String,
        ) -> moso_core::BoxFuture<'a, Result<Option<DefaultUser>>> {
            Box::pin(async move {
                Ok(self
                    .rows()
                    .get(id)
                    .map(|(_, phc)| DefaultUser::new(id, phc.as_bytes().to_vec())))
            })
        }

        fn create<'a>(
            &'a self,
            account: &'a NewAccount,
        ) -> moso_core::BoxFuture<'a, Result<DefaultUser>> {
            Box::pin(async move {
                let id = format!("usr_{}", self.rows().len() + 1);
                let phc = account.password_hash().as_str().to_owned();
                Ok(self.seed(&id, account.identity(), &phc))
            })
        }

        fn password_hash<'a>(
            &'a self,
            id: &'a String,
        ) -> moso_core::BoxFuture<'a, Result<Option<PasswordHash>>> {
            Box::pin(async move {
                self.rows()
                    .get(id)
                    .map(|(_, phc)| PasswordHash::parse(phc))
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
                    row.1 = hash.as_str().to_owned();
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
                    row.0 = identity.to_owned();
                }
                Ok(())
            })
        }

        fn mark_verified<'a>(&'a self, _id: &'a String) -> moso_core::BoxFuture<'a, Result<()>> {
            Box::pin(async move { Ok(()) })
        }

        fn bump_epoch<'a>(&'a self, _id: &'a String) -> moso_core::BoxFuture<'a, Result<()>> {
            Box::pin(async move { Ok(()) })
        }
    }

    /// A booted application with the bearer routes and `/auth/me` mounted, driven
    /// over HTTP through the composed service. No socket, no database: the whole
    /// flow runs in memory, so this case runs everywhere.
    struct Harness {
        /// The composed application.
        service: axum::Router<()>,
    }

    impl Harness {
        /// Build one signer's worth of key material, shared by the state's
        /// issuer and the refresh store's, so a token minted by either verifies
        /// against the other.
        fn jwt() -> Jwt {
            let config = JwtConfig {
                algorithm: JwtAlgorithm::HS256,
                allow_symmetric: true,
                ..JwtConfig::default()
            };
            Jwt::issuer(config, "test-key", SecretBytes::new(vec![9u8; 32]))
                .expect("an HS256 issuer")
        }

        async fn wired() -> Self {
            // A deliberately weak hash: OWASP parameters would make this test one
            // nobody runs. The verifier reads the parameters out of the PHC, so a
            // cheap seed is a cheap verification.
            let phc = PasswordHash::with_params(
                &Password::new(SECRET).expect("a long-enough password"),
                HashParams::new(8, 1, 1),
            )
            .await
            .expect("the seed hashes");

            let accounts = Arc::new(MapAccounts::default());
            accounts.seed("usr_1", "ada@example.com", phc.as_str());

            let sessions = MemorySessionStore::shared() as Arc<dyn SessionStore>;
            let refresh = Arc::new(MemoryRefreshStore::new(Arc::new(Self::jwt())));

            let state = AuthState::new(Arc::clone(&sessions))
                .accounts(
                    accounts as Arc<dyn AccountStore<User = DefaultUser>>,
                    KvLifecycleTokens::shared(
                        moso_kv::Kv::in_memory("token-test").expect("an in-memory kv"),
                    ),
                )
                .refresh(refresh as Arc<dyn crate::RefreshStore>)
                .jwt(Self::jwt());

            let app = moso_core::App::new(TestConfig)
                .profile(Profile::Test)
                .provide(state)
                .mount(crate::routes().password().bearer().build())
                .build()
                .expect("the test application boots");

            Self {
                service: app.into_service(),
            }
        }

        /// `POST path` with a JSON body, returning the status and parsed body.
        async fn post(
            &self,
            path: &str,
            body: serde_json::Value,
        ) -> (http::StatusCode, serde_json::Value) {
            self.send(
                http::Request::builder()
                    .method(http::Method::POST)
                    .uri(path)
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .expect("a request"),
            )
            .await
        }

        /// `GET path` carrying an `Authorization: Bearer` header.
        async fn get_with_bearer(
            &self,
            path: &str,
            token: &str,
        ) -> (http::StatusCode, serde_json::Value) {
            self.send(
                http::Request::builder()
                    .method(http::Method::GET)
                    .uri(path)
                    .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("a request"),
            )
            .await
        }

        async fn send(
            &self,
            request: http::Request<axum::body::Body>,
        ) -> (http::StatusCode, serde_json::Value) {
            let response = self
                .service
                .clone()
                .oneshot(request)
                .await
                .expect("the composed service answers");
            let status = response.status();
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

    /// The whole flow, over HTTP: a password buys a pair, the access token
    /// authenticates a protected route, the refresh token rotates, and the old
    /// refresh token — replayed — is refused and takes its whole family down.
    #[tokio::test]
    async fn the_bearer_flow_issues_authenticates_rotates_and_detects_reuse() {
        let harness = Harness::wired().await;

        // 1. Issue.
        let (status, body) = harness
            .post(
                "/auth/token",
                serde_json::json!({ "identity": "ada@example.com", "password": SECRET }),
            )
            .await;
        assert_eq!(status, http::StatusCode::OK, "issuance: {body}");
        assert_eq!(body["requires_second_factor"], false);
        let access = body["access_token"]
            .as_str()
            .expect("an access token")
            .to_owned();
        let first_refresh = body["refresh_token"]
            .as_str()
            .expect("a refresh token")
            .to_owned();

        // 2. The access token authenticates a protected route.
        let (status, me) = harness.get_with_bearer("/auth/me", &access).await;
        assert_eq!(status, http::StatusCode::OK, "protected route: {me}");
        assert_eq!(me["kind"], "token", "it authenticated as a bearer token");
        assert!(me["subject"].as_str().is_some_and(|s| !s.is_empty()));

        // 3. Rotate.
        let (status, rotated) = harness
            .post(
                "/auth/refresh",
                serde_json::json!({ "refresh_token": first_refresh }),
            )
            .await;
        assert_eq!(status, http::StatusCode::OK, "rotation: {rotated}");
        let second_refresh = rotated["refresh_token"]
            .as_str()
            .expect("a rotated refresh token")
            .to_owned();
        assert_ne!(
            second_refresh, first_refresh,
            "rotation mints a fresh token"
        );
        assert!(rotated["access_token"].as_str().is_some());

        // 4. Replaying the first, now-spent refresh token is refused.
        let (status, _) = harness
            .post(
                "/auth/refresh",
                serde_json::json!({ "refresh_token": first_refresh }),
            )
            .await;
        assert_eq!(
            status,
            http::StatusCode::UNAUTHORIZED,
            "a replayed refresh token is a 401"
        );

        // 5. And reuse burned the family, so the token it rotated into is dead too.
        let (status, _) = harness
            .post(
                "/auth/refresh",
                serde_json::json!({ "refresh_token": second_refresh }),
            )
            .await;
        assert_eq!(
            status,
            http::StatusCode::UNAUTHORIZED,
            "reuse revokes the whole family, the good descendant included"
        );
    }

    /// A wrong password is the same 401 as a wrong address, and issues nothing.
    #[tokio::test]
    async fn a_wrong_password_issues_no_token() {
        let harness = Harness::wired().await;

        let (status, _) = harness
            .post(
                "/auth/token",
                serde_json::json!({ "identity": "ada@example.com", "password": "not the secret" }),
            )
            .await;
        assert_eq!(status, http::StatusCode::UNAUTHORIZED);
    }

    /// An unknown refresh token is refused without a family to revoke.
    #[tokio::test]
    async fn an_unknown_refresh_token_is_refused() {
        let harness = Harness::wired().await;

        let (status, _) = harness
            .post(
                "/auth/refresh",
                serde_json::json!({ "refresh_token": "nonsense" }),
            )
            .await;
        assert_eq!(status, http::StatusCode::UNAUTHORIZED);
    }

    /// A minted access token, presented as a bearer credential, is enough for a
    /// protected route on its own — no `/auth/token` round trip needed to prove
    /// the [`Principal`](crate::Principal) extractor resolves one.
    #[tokio::test]
    async fn an_access_token_alone_authenticates_the_protected_route() {
        let harness = Harness::wired().await;
        let access = Harness::jwt()
            .issue(&Claims::new("usr_1"), Duration::from_secs(300))
            .expect("an access token");

        let (status, me) = harness.get_with_bearer("/auth/me", &access).await;
        assert_eq!(status, http::StatusCode::OK, "{me}");
        assert_eq!(me["subject"], "usr_1");
        assert_eq!(me["kind"], "token");

        // A garbage bearer token authenticates nobody: `/auth/me` is a 401.
        let (status, _) = harness.get_with_bearer("/auth/me", "not.a.jwt").await;
        assert_eq!(status, http::StatusCode::UNAUTHORIZED);
    }
}
