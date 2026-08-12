//! The public surface, exercised from outside the crate.
//!
//! What matters here is what an application actually writes: an [`AuthUser`] on
//! its own entity, an [`AuthBackend`] for a credential source Moso does not
//! ship, and the extractors in a handler's signature. All three are coherence
//! and bound questions, and a unit test inside the crate cannot answer them — it
//! can see private items, and it resolves paths the way the crate does rather
//! than the way a dependent does.
//!
//! **Every test in this file runs.** It used to hold compile-only `fn`s that
//! proved the frozen signatures composed while their bodies were `todo!()`;
//! the bodies are real now, so each of those exercises has become an assertion
//! about behaviour. The few facts that only a compiler can check — dyn
//! compatibility, a blanket impl, `Send + Sync` — are checked inside a `#[test]`
//! body rather than in a function nothing calls, so that a bound which stops
//! holding is a failing target and not silently dead code.

use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "passkeys")]
use moso_auth::PasskeyStore;
use moso_auth::{
    ApiKey, ApiKeyStore, AuthBackend, AuthCtx, AuthUser, CaptchaVerifier, CookieConfig,
    CurrentUser, DefaultUser, HashParams, JwtAlgorithm, KeyEnvironment, LinkPolicy, MaybeUser,
    MemoryApiKeyStore, MemorySessionStore, PasswordHash, PasswordPolicy, Principal, PrincipalKind,
    RefreshStore, Result, SameSite, SessionConfig, SessionId, SessionRecord, SessionStore,
    TotpSecret, UserStore,
};
use moso_core::BoxFuture;
use moso_schema::Password;

// ---------------------------------------------------------------------------
// What an application writes
// ---------------------------------------------------------------------------

/// The application's own account record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    /// The primary key, which is what [`AuthUser::Id`] is bound to.
    pub id: u64,
    /// The address the account logs in with.
    pub email: String,
    /// The stored PHC string. Half of what makes `auth_hash` work.
    pub password_hash: String,
    /// Bumped by "log out everywhere". The other half.
    pub session_epoch: i32,
    /// Whether an operator has disabled the account.
    pub suspended: bool,
}

impl AuthUser for User {
    type Id = u64;

    fn auth_id(&self) -> u64 {
        self.id
    }

    fn auth_hash(&self) -> Vec<u8> {
        // The password hash *and* an epoch: the hash covers a reset, the epoch
        // covers "log me out of my other devices" without a password change.
        let mut bytes = self.password_hash.as_bytes().to_vec();
        bytes.extend_from_slice(&self.session_epoch.to_le_bytes());
        bytes
    }

    fn is_active(&self) -> bool {
        !self.suspended
    }
}

/// A backend for a credential source Moso does not ship — the case
/// [`DatabaseBackend`](moso_auth::DatabaseBackend) cannot cover, and the reason
/// [`AuthBackend`] is public.
///
/// The directory is in memory rather than over LDAP because what is under test
/// is the *shape* of the trait: that `authenticate` can carry a credential type
/// of the implementor's choosing in, and a user out, and that `load` can find
/// the same record again by id. A network round trip would test the network.
pub struct LdapBackend {
    /// The accounts this directory knows about, with their plaintext passwords.
    /// Plaintext is correct here and only here: a directory server is exactly
    /// the case where Moso does not own the hashing.
    directory: Vec<(User, &'static str)>,
}

impl LdapBackend {
    /// A directory holding one active account and one suspended one.
    fn fixture() -> Self {
        Self {
            directory: vec![
                (
                    User {
                        id: 1,
                        email: "ada@example.com".to_owned(),
                        password_hash: "$argon2id$v=19$…$aaa".to_owned(),
                        session_epoch: 0,
                        suspended: false,
                    },
                    "correct horse battery staple",
                ),
                (
                    User {
                        id: 2,
                        email: "grace@example.com".to_owned(),
                        password_hash: "$argon2id$v=19$…$bbb".to_owned(),
                        session_epoch: 3,
                        suspended: true,
                    },
                    "hopper hopper hopper",
                ),
            ],
        }
    }
}

impl AuthBackend for LdapBackend {
    type User = User;
    type Credentials = (String, String);

    fn authenticate<'a>(
        &'a self,
        credentials: (String, String),
        _ctx: &'a AuthCtx,
    ) -> BoxFuture<'a, Result<Option<User>>> {
        Box::pin(async move {
            let (email, password) = credentials;
            Ok(self
                .directory
                .iter()
                .find(|(user, secret)| user.email == email && *secret == password)
                .map(|(user, _)| user.clone()))
        })
    }

    fn load<'a>(&'a self, id: &'a u64) -> BoxFuture<'a, Result<Option<User>>> {
        Box::pin(async move {
            Ok(self
                .directory
                .iter()
                .find(|(user, _)| user.id == *id)
                .map(|(user, _)| user.clone()))
        })
    }
}

// ---------------------------------------------------------------------------
// Composition: the traits an application implements or names
// ---------------------------------------------------------------------------

/// Every backend is a user store, through the blanket impl, and the erased form
/// is what goes into the provider map. This is what lets `CurrentUser<User>`
/// resolve without naming the backend's `Credentials`, which a request does not
/// carry — so the test goes through `Arc<dyn UserStore<User>>` rather than
/// through `LdapBackend`, because the erased path is the one a route uses.
#[tokio::test]
async fn a_backend_reaches_a_route_as_an_erased_user_store() {
    let store: Arc<dyn UserStore<User>> = Arc::new(LdapBackend::fixture());

    let found = store.load_user(&1).await.expect("the directory answers");
    assert_eq!(
        found.map(|user| user.email),
        Some("ada@example.com".to_owned())
    );

    assert_eq!(
        store
            .load_user(&404)
            .await
            .expect("an unknown id is not an error"),
        None,
        "an account nobody has is `None`, not an error — the caller must not be \
         able to tell the difference between absent and refused"
    );
}

/// The credential type is the implementor's, and it survives the trait. A
/// backend whose `Credentials` were pinned to Moso's own type could not front
/// a directory server, which is the whole reason the associated type exists.
#[tokio::test]
async fn a_backend_carries_its_own_credential_type_in_and_a_user_out() {
    let backend = LdapBackend::fixture();
    let ctx = AuthCtx::default();

    let ok = backend
        .authenticate(
            (
                "ada@example.com".to_owned(),
                "correct horse battery staple".to_owned(),
            ),
            &ctx,
        )
        .await
        .expect("the directory answers");
    assert_eq!(ok.map(|user| user.id), Some(1));

    let wrong_password = backend
        .authenticate(("ada@example.com".to_owned(), "hunter2".to_owned()), &ctx)
        .await
        .expect("a wrong password is not an error");
    let no_such_account = backend
        .authenticate(
            ("nobody@example.com".to_owned(), "hunter2".to_owned()),
            &ctx,
        )
        .await
        .expect("an unknown account is not an error");
    assert_eq!(
        wrong_password, no_such_account,
        "a wrong password and an account that does not exist must be the same \
         answer, or the backend is an account-enumeration oracle"
    );
}

/// `CurrentUser<U>` derefs to the user, so a handler reads `user.email` rather
/// than `user.0.email`, and `MaybeUser<U>` hands back the same type.
#[test]
fn the_extractors_deref_to_the_application_type() {
    let user = User {
        id: 7,
        email: "ada@example.com".to_owned(),
        password_hash: String::new(),
        session_epoch: 0,
        suspended: false,
    };

    let current = CurrentUser(user.clone());
    let read_through_deref: &str = &current.email;
    assert_eq!(read_through_deref, "ada@example.com");
    assert_eq!(current.into_inner(), user);

    assert_eq!(MaybeUser(Some(user.clone())).into_inner(), Some(user));
    assert_eq!(MaybeUser::<User>(None).into_inner(), None);
}

/// The extractors are `Dependency` impls, which is what makes them memoised per
/// request and what makes them contribute to the OpenAPI operation; the guards
/// are `Guard` impls, so what they refuse appears in the document instead of
/// being an undocumented 403. Both are bounds, so the assertion is the
/// type-check — it is written inside a test so that losing the bound fails a
/// target somebody runs.
#[test]
fn the_extractors_are_dependencies_and_the_guards_are_guards() {
    fn assert_dependency<D: moso_core::di::Dependency>() {}
    assert_dependency::<CurrentUser<User>>();
    assert_dependency::<MaybeUser<User>>();
    assert_dependency::<moso_auth::AuthSession>();
    assert_dependency::<Principal>();

    fn assert_guard<G: moso_core::Guard>() {}
    assert_guard::<moso_auth::Csrf>();
    assert_guard::<moso_auth::RequireKind>();
}

/// A user is held across `.await` in every authenticated handler, so it has to
/// be `Send + Sync`, and `Clone` for the dependency cache.
#[test]
fn users_are_send_sync_and_clone() {
    fn assert<T: Send + Sync + Clone + 'static>() {}
    assert::<User>();
    assert::<DefaultUser>();
    assert::<CurrentUser<User>>();
    assert::<MaybeUser<User>>();
}

/// Every store is dyn-compatible, because which one an application uses is
/// configuration and not a type parameter — and the shipped stores really do
/// work through the erased form, which naming `dyn` alone would not prove.
#[tokio::test]
async fn the_shipped_stores_work_through_their_erased_form() {
    let sessions: Arc<dyn SessionStore> = MemorySessionStore::shared();
    let record = SessionRecord::new(SessionId::generate());
    let id = record.id.clone();
    sessions
        .save(&record, Duration::from_secs(60))
        .await
        .expect("the memory store saves");
    assert!(
        sessions
            .load(&id)
            .await
            .expect("the store answers")
            .is_some(),
        "a session that was just written must load back"
    );
    assert!(
        sessions.delete(&id).await.expect("the store answers"),
        "deleting a session that exists reports that it did something"
    );
    assert!(
        sessions
            .load(&id)
            .await
            .expect("an unknown id is not an error")
            .is_none(),
        "a deleted session is indistinguishable from one that never existed"
    );

    let keys: Arc<dyn ApiKeyStore> = Arc::new(MemoryApiKeyStore::new());
    let issued = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live).expect("a key is minted");
    let prefix = issued.record.prefix.clone();
    keys.insert(&issued.record).await.expect("the store writes");
    assert!(
        keys.find_by_prefix(&prefix)
            .await
            .expect("the store answers")
            .is_some(),
        "a key that was just written must be findable by its prefix"
    );

    // `PasskeyStore` and `CaptchaVerifier` ship no in-tree implementation — an
    // application brings its own — so dyn compatibility is all there is to
    // check, and naming the type is what checks it. `PasskeyStore` only exists
    // when the off-by-default `passkeys` feature is on (ADR-0015).
    #[cfg(feature = "passkeys")]
    let no_passkeys: Option<Arc<dyn PasskeyStore>> = None;
    #[cfg(feature = "passkeys")]
    assert!(no_passkeys.is_none());
    let no_captcha: Option<Arc<dyn CaptchaVerifier>> = None;
    let no_refresh: Option<Arc<dyn RefreshStore>> = None;
    assert!(no_captcha.is_none() && no_refresh.is_none());
}

/// The mountable route builder, as a prototype writes it: the chain returns a
/// router with routes in it, every one of them under `/auth` or at the
/// well-known JWKS path, and none of them conflicting.
#[test]
fn the_route_builder_chains_and_mounts_what_it_was_asked_for() {
    let builder = moso_auth::routes()
        .password()
        .sessions()
        .api_keys()
        .totp()
        .magic_link()
        .jwks()
        .redirect_allowlist(["https://app.example.com"]);
    #[cfg(feature = "passkeys")]
    let builder = builder.passkeys();
    let router = builder.build();

    assert!(!router.is_empty());
    assert!(router.conflicts().is_empty());

    for route in router.describe() {
        assert!(
            route.path.starts_with("/auth/") || route.path == "/.well-known/jwks.json",
            "{} is neither an auth route nor the JWKS document",
            route.path
        );
    }
}

/// Asking for nothing mounts nothing. Every flag is opt-in, so an application
/// that wants only the JWKS document does not also serve a registration form.
#[test]
fn a_builder_asked_for_nothing_mounts_nothing() {
    assert!(moso_auth::routes().build().is_empty());

    let only_jwks = moso_auth::routes().jwks().build();
    let paths: Vec<_> = only_jwks
        .describe()
        .into_iter()
        .map(|route| route.path)
        .collect();
    assert_eq!(paths, vec!["/.well-known/jwks.json".to_owned()]);
}

// ---------------------------------------------------------------------------
// The defaults the documentation promises
// ---------------------------------------------------------------------------

/// `auth_hash` must change when the password changes **and** when the epoch
/// changes. If it did not, "log out everywhere" would silently do nothing —
/// the failure mode is invisible, which is why it is asserted rather than
/// assumed.
#[test]
fn auth_hash_changes_with_both_the_password_and_the_epoch() {
    let base = User {
        id: 1,
        email: "ada@example.com".to_owned(),
        password_hash: "$argon2id$v=19$…$aaa".to_owned(),
        session_epoch: 0,
        suspended: false,
    };

    let after_reset = User {
        password_hash: "$argon2id$v=19$…$bbb".to_owned(),
        ..base.clone()
    };
    assert_ne!(
        base.auth_hash(),
        after_reset.auth_hash(),
        "a password reset must invalidate every session"
    );

    let after_logout_all = User {
        session_epoch: 1,
        ..base.clone()
    };
    assert_ne!(
        base.auth_hash(),
        after_logout_all.auth_hash(),
        "an epoch bump must invalidate every session without a password change"
    );

    assert_eq!(
        base.auth_hash(),
        base.clone().auth_hash(),
        "an unchanged user must keep its sessions"
    );
}

/// A suspended account is inactive, and the check is the application's to make
/// — the trait only provides the hook.
#[test]
fn suspension_flows_through_is_active() {
    let user = User {
        id: 1,
        email: "ada@example.com".to_owned(),
        password_hash: String::new(),
        session_epoch: 0,
        suspended: true,
    };
    assert!(!user.is_active());
}

/// The cookie defaults are the security posture the documentation promises. A
/// silent change here would be a production-only regression.
#[test]
fn the_cookie_defaults_hold_from_outside_the_crate() {
    let cookie = CookieConfig::default();
    assert!(cookie.http_only);
    assert!(cookie.secure);
    assert!(cookie.host_prefix);
    assert_eq!(cookie.same_site, SameSite::Lax);
    assert!(cookie.domain.is_none());
}

/// Rolling expiry needs the idle timeout strictly inside the absolute one, or
/// the cap never fires.
#[test]
fn the_idle_timeout_sits_inside_the_absolute_one() {
    let config = SessionConfig::default();
    assert!(config.idle_timeout < config.absolute_timeout);
    assert!(
        config.touch_interval < config.idle_timeout,
        "a touch interval above the idle timeout would expire live sessions"
    );
}

/// The password policy is the current NIST position: length and breach, not
/// composition.
#[test]
fn the_password_policy_is_length_and_breach() {
    let policy = PasswordPolicy::default();
    assert_eq!(policy.min_length, 12);
    assert!(policy.breach_check);
    assert!(policy.banned_words.is_empty());
}

/// The hashing floor is OWASP's, and `at_least` has to be conjunctive —
/// weaker in *any* dimension is weaker, or `needs_rehash` would miss an
/// upgrade.
#[test]
fn the_hash_floor_comparison_is_conjunctive() {
    let floor = HashParams::OWASP_MINIMUM;
    assert!(floor.at_least(floor));

    let stronger = HashParams::new(floor.memory_kib * 2, floor.iterations, floor.parallelism);
    assert!(stronger.at_least(floor));
    assert!(!floor.at_least(stronger));

    let mixed = HashParams::new(floor.memory_kib * 2, 1, floor.parallelism);
    assert!(
        !mixed.at_least(floor),
        "more memory does not compensate for fewer passes"
    );
}

/// Symmetric JWT signing is off by default, and the algorithm knows it is
/// symmetric — the two facts the boot warning is built on.
#[test]
fn symmetric_signing_is_opt_in() {
    assert!(!moso_auth::JwtConfig::default().allow_symmetric);
    assert!(JwtAlgorithm::HS256.is_symmetric());
    assert!(!JwtAlgorithm::EdDSA.is_symmetric());
    assert!(!JwtAlgorithm::ES256.is_symmetric());
    assert!(!JwtAlgorithm::RS256.is_symmetric());
}

/// Account linking refuses an unverified address by default. This is the one
/// default in the crate whose loosening is an account-takeover path.
#[test]
fn linking_refuses_unverified_addresses() {
    assert_eq!(LinkPolicy::default(), LinkPolicy::VerifiedEmailOrSession);
}

/// The key environment segment is what a production deployment refuses a test
/// key on, so its spelling is part of the wire format.
#[test]
fn key_environments_spell_themselves_stably() {
    assert_eq!(KeyEnvironment::Live.as_str(), "live");
    assert_eq!(KeyEnvironment::Test.as_str(), "test");
    assert_eq!(moso_auth::apikey::KEY_PREFIX, "mso");
}

/// `PrincipalKind` is what an audit record stores, so its strings are a wire
/// format too, and only `Anonymous` is unauthenticated.
#[test]
fn principal_kinds_are_a_stable_wire_format() {
    for kind in [
        PrincipalKind::Session,
        PrincipalKind::Token,
        PrincipalKind::ApiKey,
        PrincipalKind::Service,
    ] {
        assert!(kind.is_authenticated(), "{kind:?} authenticates something");
    }
    assert!(!PrincipalKind::Anonymous.is_authenticated());
    assert_eq!(PrincipalKind::ApiKey.as_str(), "api_key");
    assert_eq!(PrincipalKind::default(), PrincipalKind::Anonymous);

    let anonymous = Principal::anonymous();
    assert!(!anonymous.is_authenticated());
    assert!(anonymous.subject().is_none());
    assert!(anonymous.scopes.is_empty());
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

/// Every credential-shaped type has a hand-written `Debug`, because a derived
/// one would print the credential and a `Debug` in a log line is a credential in
/// a log aggregator.
///
/// This asserts the *content* of the redaction, not merely the bound: each type
/// is constructed and its rendering is checked for the secret it holds. The
/// bound alone would pass against a derived `Debug`, which is the failure this
/// is here to catch.
#[tokio::test]
async fn no_credential_type_prints_its_own_secret() {
    // Deliberately far below `HashParams::OWASP_MINIMUM`: what is under test is
    // the formatter, and a test that spent 40 ms per hash is a test nobody runs.
    // `with_params` is the documented escape hatch for exactly this.
    let plain = Password::new("a sufficiently long one").expect("the policy accepts it");
    let hash = PasswordHash::with_params(&plain, HashParams::new(8, 1, 1))
        .await
        .expect("argon2 accepts these parameters");
    assert_eq!(format!("{hash:?}"), "PasswordHash(<redacted>)");
    assert!(
        !format!("{hash:?}").contains(hash.as_str()),
        "the PHC string is offline-attackable and must not reach a log"
    );

    let session = SessionId::generate();
    assert_eq!(format!("{session:?}"), "SessionId(<redacted>)");
    assert!(
        !format!("{session:?}").contains(session.as_str()),
        "a session id in a log line is a credential in a place nobody audits"
    );

    let totp = TotpSecret::generate().expect("the system generator answers");
    assert_eq!(format!("{totp:?}"), "TotpSecret(<redacted>)");
    assert!(
        !format!("{totp:?}").contains(totp.as_secret().expose()),
        "a TOTP secret is the whole second factor"
    );

    let issued = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live).expect("a key is minted");
    assert!(
        !format!("{issued:?}").contains(issued.secret.expose()),
        "the one-time API key secret must not be recoverable from a debug line"
    );
}
