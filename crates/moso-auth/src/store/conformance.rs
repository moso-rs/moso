//! One suite per trait, run against every store that implements it.
//!
//! `moso-kv` proves its three backends agree by writing the suite once and
//! running it three times (`moso-kv/tests/conformance.rs`, acceptance criterion
//! 1 of `docs/02-data/25-kv-cache.md`). This module is that pattern for the
//! three authentication stores that now have both an in-memory and a
//! table-backed implementation:
//!
//! | Trait | In memory | In a table |
//! | --- | --- | --- |
//! | [`RefreshStore`] | [`MemoryRefreshStore`] | [`TableRefreshStore`] |
//! | [`ApiKeyStore`] | [`MemoryApiKeyStore`] | [`TableApiKeyStore`] |
//! | [`PasskeyStore`] | [`MemoryPasskeyStore`] | [`TablePasskeyStore`] |
//!
//! Writing the assertions once is the point. Two suites that started identical
//! drift within a release, and the drift is invisible until a deployment that
//! developed against the map behaves differently against the table — which is
//! the failure this whole module set exists to remove.
//!
//! # Why it is here and not in `tests/`
//!
//! The suite has to construct a [`PasskeyCredential`], which is
//! `#[non_exhaustive]` and therefore only constructible inside this crate, and
//! it reaches the table stores' private statement helpers. It lives beside the
//! code it holds together.
//!
//! # Three legs
//!
//! Every suite runs against the in-memory store, against SQLite, and against
//! PostgreSQL. The PostgreSQL leg **skips with a message** when `DATABASE_URL`
//! is unset — a tested property, not a courtesy: the macOS CI leg runs the whole
//! suite with it deliberately unset, so a test that failed without a database
//! would be a broken test.
//!
//! ```text
//! ./scripts/test-db.sh up
//! eval "$(./scripts/test-db.sh env)"
//! cargo nextest run -p moso-auth --all-features
//! ```
//!
//! Nothing here is mocked. The table legs run against a real PostgreSQL and a
//! real SQLite, because a mocked data-layer test proves nothing about SQL.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use moso_core::config::SecretBytes;
use moso_orm::Db;

use crate::apikey::{ApiKey, ApiKeyStore, KeyEnvironment, MemoryApiKeyStore};
use crate::jwt::{Claims, Jwt, JwtConfig, MemoryRefreshStore, RefreshOutcome, RefreshStore};
use crate::store::apikey::TableApiKeyStore;
#[cfg(feature = "passkeys")]
use crate::store::passkey::{MemoryPasskeyStore, TablePasskeyStore};
use crate::store::refresh::TableRefreshStore;
#[cfg(feature = "passkeys")]
use crate::webauthn::{PasskeyCredential, PasskeyStore};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An identifier nothing else in this process, or in a concurrent run, is using.
///
/// The PostgreSQL leg shares one server between test binaries and between runs,
/// so a fixed `"usr_1"` would have two tests deleting each other's rows.
pub(super) fn unique(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let sequence = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{prefix}_{nanos}_{sequence}")
}

/// A real Ed25519 issuer, for the stores that mint an access token.
///
/// Generated rather than committed: `ring` generates Ed25519 keys, so there is
/// no reason for a private key to exist in the source tree at all.
pub(super) fn jwt_issuer() -> Jwt<Claims> {
    let rng = ring::rand::SystemRandom::new();
    let document =
        ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).expect("the OS CSPRNG answered");
    Jwt::issuer(
        JwtConfig::default(),
        "conformance",
        SecretBytes::new(document.as_ref().to_vec()),
    )
    .expect("an Ed25519 issuer builds")
}

/// A SQLite database, which needs nothing installed.
///
/// `Db::connect` pins an in-memory pool to one connection, because every
/// connection to `:memory:` is a different database.
pub(super) async fn sqlite_db() -> Db {
    Db::connect_url("sqlite://:memory:")
        .await
        .expect("the bundled SQLite is always available")
}

/// A PostgreSQL database, or `None` when `DATABASE_URL` is not set.
pub(super) async fn postgres_db() -> Option<Db> {
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())?;
    Some(
        Db::connect_url(&url)
            .await
            .expect("DATABASE_URL is set but the server did not accept a connection"),
    )
}

/// Run `body` against SQLite, and against PostgreSQL when it is available.
pub(super) async fn on_every_backend<F, Fut>(body: F)
where
    F: Fn(Db) -> Fut,
    Fut: Future<Output = ()>,
{
    body(sqlite_db().await).await;

    match postgres_db().await {
        Some(db) => body(db).await,
        None => eprintln!(
            "skipping the PostgreSQL leg: DATABASE_URL is not set. Start the test server with \
             `./scripts/test-db.sh up` and `eval \"$(./scripts/test-db.sh env)\"`."
        ),
    }
}

// ---------------------------------------------------------------------------
// The refresh-token suite
// ---------------------------------------------------------------------------

/// Everything every [`RefreshStore`] must do.
///
/// `subject` is the caller's, so the PostgreSQL leg does not collide with a
/// concurrent test.
pub(super) async fn refresh_suite(store: &dyn RefreshStore, subject: &str) {
    let hour = Duration::from_secs(3600);

    // Issue: a token in a fresh family, and two issues never share one.
    let first = store.issue(subject, hour).await.expect("issued");
    assert_eq!(first.subject, subject);
    assert!(first.is_live(Utc::now()));
    let sibling = store.issue(subject, hour).await.expect("issued");
    assert_ne!(
        first.family(),
        sibling.family(),
        "each issue starts its own family"
    );

    // Exchange: a new token in the same family, and a working access token.
    let RefreshOutcome::Rotated { access, refresh } =
        store.exchange(first.expose()).await.expect("exchanged")
    else {
        panic!("a live token must rotate");
    };
    assert!(!access.is_empty(), "a rotation mints an access token");
    assert_eq!(refresh.family(), first.family(), "the family is inherited");
    assert_ne!(refresh.expose(), first.expose(), "and the token is new");
    assert_eq!(refresh.subject, subject);

    // Replay: reuse detection burns the family, descendants included.
    let replayed = store.exchange(first.expose()).await.expect("exchanged");
    match replayed {
        RefreshOutcome::ReuseDetected { ref family } => {
            assert_eq!(family, first.family());
        }
        other => panic!("a replayed token must be detected, got {other:?}"),
    }
    assert!(
        matches!(
            store.exchange(refresh.expose()).await.expect("exchanged"),
            RefreshOutcome::Invalid
        ),
        "the descendant died with its family"
    );

    // An unknown token is invalid and has no family to burn.
    assert!(matches!(
        store.exchange("not-a-token").await.expect("exchanged"),
        RefreshOutcome::Invalid
    ));

    // Revoking a family kills it.
    assert!(
        store
            .revoke_family(sibling.family())
            .await
            .expect("revoked")
            >= 1
    );
    assert!(matches!(
        store.exchange(sibling.expose()).await.expect("exchanged"),
        RefreshOutcome::Invalid
    ));

    // Revoking a subject is "log out everywhere" for token clients.
    let third = store.issue(subject, hour).await.expect("issued");
    let fourth = store.issue(subject, hour).await.expect("issued");
    assert!(store.revoke_subject(subject).await.expect("revoked") >= 2);
    for dead in [&third, &fourth] {
        assert!(matches!(
            store.exchange(dead.expose()).await.expect("exchanged"),
            RefreshOutcome::Invalid
        ));
    }

    // An expired token never rotates.
    let brief = store
        .issue(subject, Duration::from_millis(1))
        .await
        .expect("issued");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(matches!(
        store.exchange(brief.expose()).await.expect("exchanged"),
        RefreshOutcome::Invalid
    ));

    store.revoke_subject(subject).await.expect("cleaned up");
}

// ---------------------------------------------------------------------------
// The API-key suite
// ---------------------------------------------------------------------------

/// Everything every [`ApiKeyStore`] must do.
pub(super) async fn api_key_suite(store: &dyn ApiKeyStore, owner: &str) {
    let new = ApiKey::generate("ci", owner, KeyEnvironment::Live)
        .expect("generated")
        .with_scopes(["posts:read", "posts:write"])
        .expiring_in(Duration::from_secs(90 * 24 * 3600));
    let presented = new.secret.expose().to_owned();
    let record = new.record;

    // Unknown prefixes are absent, not an error.
    assert!(
        store
            .find_by_prefix("00000000")
            .await
            .expect("looked up")
            .is_none()
    );

    store.insert(&record).await.expect("inserted");

    // A round trip preserves every field the authenticator reads.
    let found = store
        .find_by_prefix(&record.prefix)
        .await
        .expect("looked up")
        .expect("the key is there");
    assert_eq!(found.id, record.id);
    assert_eq!(found.owner, owner);
    assert_eq!(found.name, "ci");
    assert_eq!(found.environment, KeyEnvironment::Live);
    assert_eq!(found.scopes, vec!["posts:read", "posts:write"]);
    assert!(found.expires_at.is_some());
    assert!(found.revoked_at.is_none());
    assert!(found.is_usable(Utc::now()));

    // And the secret still verifies against the stored hash, which is the only
    // thing the hash is for.
    let (environment, prefix, secret) = ApiKey::parse(&presented).expect("parsed");
    assert_eq!(environment, KeyEnvironment::Live);
    assert_eq!(prefix, record.prefix);
    assert!(
        found.verify_secret(&secret),
        "the stored hash still matches"
    );
    assert!(!found.verify_secret("not it"));

    // The prefix is unique: a collision is an error the caller retries, never
    // two rows one lookup cannot tell apart.
    assert!(
        store.insert(&record).await.is_err(),
        "a duplicate prefix must be refused"
    );

    // Listing is per owner, oldest first, and does not leak another owner's.
    let second = ApiKey::generate("deploy", owner, KeyEnvironment::Test)
        .expect("generated")
        .record;
    store.insert(&second).await.expect("inserted");
    let stranger = ApiKey::generate("theirs", unique("other"), KeyEnvironment::Live)
        .expect("generated")
        .record;
    store.insert(&stranger).await.expect("inserted");

    let listed = store.list_for_owner(owner).await.expect("listed");
    assert_eq!(listed.len(), 2, "only this owner's keys");
    assert!(listed[0].created_at <= listed[1].created_at, "oldest first");
    assert!(
        store
            .list_for_owner("nobody")
            .await
            .expect("listed")
            .is_empty()
    );

    // Last-used tracking is lossy by contract and must still be visible.
    let used_at = Utc::now();
    store.touch(record.id, used_at).await;
    let touched = store
        .find_by_prefix(&record.prefix)
        .await
        .expect("looked up")
        .expect("still there");
    assert!(
        touched
            .last_used_at
            .is_some_and(|at| (at - used_at).num_seconds().abs() <= 1),
        "last_used_at was not recorded"
    );

    // Revocation is a tombstone, not a delete: an audit trail that cannot
    // resolve a key id is not an audit trail.
    assert!(store.revoke(record.id).await.expect("revoked"));
    assert!(
        !store.revoke(record.id).await.expect("revoked"),
        "revoking twice changes nothing"
    );
    let revoked = store
        .find_by_prefix(&record.prefix)
        .await
        .expect("looked up")
        .expect("a revoked key is kept");
    assert!(revoked.revoked_at.is_some());
    assert!(!revoked.is_usable(Utc::now()));
    assert_eq!(
        store.list_for_owner(owner).await.expect("listed").len(),
        2,
        "and it still appears in the listing"
    );

    // An id nobody has is not an error either.
    assert!(
        !store
            .revoke(moso_schema::Id::new_v7())
            .await
            .expect("revoked")
    );
    store.touch(moso_schema::Id::new_v7(), used_at).await;
}

// ---------------------------------------------------------------------------
// The passkey suite
// ---------------------------------------------------------------------------

/// A credential with every field set to something distinguishable.
///
/// Not a ceremony output: `webauthn.rs` already proves a real registration round
/// trips through a store. What this suite proves is that *storage* keeps every
/// column, which needs a value in each of them.
#[cfg(feature = "passkeys")]
pub(super) fn sample_credential(credential_id: &str, user_id: &str) -> PasskeyCredential {
    PasskeyCredential {
        credential_id: credential_id.to_owned(),
        user_id: user_id.to_owned(),
        public_key: vec![0xa5, 0x01, 0x02, 0xff, 0x00],
        sign_count: 7,
        aaguid: Some("adce0002-35bc-c60a-648b-0b25f1f05503".to_owned()),
        discoverable: true,
        label: Some("MacBook Touch ID".to_owned()),
        created_at: Utc::now(),
        last_used_at: None,
        user_handle: "dXNlci1oYW5kbGU".to_owned(),
        user_verified: true,
        backup_eligible: true,
        backup_state: false,
        algorithm: -8,
        transports: vec!["internal".to_owned(), "hybrid".to_owned()],
        disabled: false,
        record: serde_json::json!({ "counter": 7, "cred_id": credential_id }),
    }
}

/// Everything every [`PasskeyStore`] must do.
#[cfg(feature = "passkeys")]
pub(super) async fn passkey_suite(store: &dyn PasskeyStore, user_id: &str) {
    let id = unique("cred");
    let credential = sample_credential(&id, user_id);

    assert!(store.find(&id).await.expect("looked up").is_none());
    assert!(!store.delete(&id).await.expect("nothing to delete"));
    assert!(!store.disable(&id).await.expect("nothing to disable"));
    store.insert(&credential).await.expect("inserted");

    // Lookup by credential id alone: no user id in hand, which is what makes
    // the discoverable flow possible.
    let found = store
        .find(&id)
        .await
        .expect("looked up")
        .expect("the credential is there");
    assert_eq!(found.user_id, user_id);
    assert_eq!(found.public_key, credential.public_key);
    assert_eq!(found.sign_count, 7);
    assert_eq!(found.aaguid, credential.aaguid);
    assert!(found.discoverable);
    assert_eq!(found.label.as_deref(), Some("MacBook Touch ID"));
    assert_eq!(found.user_handle, credential.user_handle);
    assert!(found.user_verified);
    assert!(found.backup_eligible);
    assert!(!found.backup_state);
    assert_eq!(found.algorithm, -8);
    assert_eq!(found.transports, vec!["internal", "hybrid"]);
    assert!(found.is_active());
    assert_eq!(found.record, credential.record);
    assert!(
        (found.created_at - credential.created_at)
            .num_milliseconds()
            .abs()
            < 1
    );

    // Listing is per user.
    assert_eq!(store.list_for_user(user_id).await.expect("listed").len(), 1);
    assert!(
        store
            .list_for_user(&unique("nobody"))
            .await
            .expect("listed")
            .is_empty()
    );

    // A credential id is a primary key: a second registration of one somebody
    // already holds is refused, never moved onto a new account.
    assert!(
        store
            .insert(&sample_credential(&id, &unique("attacker")))
            .await
            .is_err(),
        "a duplicate credential id must be refused"
    );

    // The counter column is authoritative, and it writes what it is told. A
    // regression never reaches a store from a verified ceremony — `WebAuthn`
    // refuses it first — so clamping here would only hide a caller that skipped
    // the verifier, and the response to a real one is `disable`.
    store.update_counter(&id, 9).await.expect("updated");
    assert_eq!(
        store
            .find(&id)
            .await
            .expect("looked up")
            .expect("still there")
            .sign_count,
        9
    );

    // A credential nobody has is not an error: the caller has already decided
    // the ceremony succeeded, and there is nothing to report.
    store
        .update_counter(&unique("absent"), 3)
        .await
        .expect("a missing credential is not a failure");

    // Quarantine: once, then never again, and the row stays resolvable.
    assert!(store.disable(&id).await.expect("disabled"));
    assert!(
        !store.disable(&id).await.expect("disabled"),
        "quarantining twice is one event, not two"
    );
    let after = store
        .find(&id)
        .await
        .expect("looked up")
        .expect("a disabled credential is kept, not hidden");
    assert!(!after.is_active());
    assert_eq!(
        store.list_for_user(user_id).await.expect("listed").len(),
        1,
        "and the user can still be told which key stopped working"
    );

    // Delete says whether there was anything to delete.
    assert!(store.delete(&id).await.expect("deleted"));
    assert!(!store.delete(&id).await.expect("deleted"));
    assert!(
        store
            .list_for_user(user_id)
            .await
            .expect("listed")
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// The legs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_memory_refresh_store_conforms() {
    let store = MemoryRefreshStore::new(Arc::new(jwt_issuer()));
    refresh_suite(&store, &unique("usr")).await;
}

#[tokio::test]
async fn the_table_refresh_store_conforms() {
    on_every_backend(|db| async move {
        let store = TableRefreshStore::new(db, Arc::new(jwt_issuer()));
        store.create_table().await.expect("the table is created");
        refresh_suite(&store, &unique("usr")).await;
    })
    .await;
}

#[tokio::test]
async fn the_memory_api_key_store_conforms() {
    let store = MemoryApiKeyStore::new();
    api_key_suite(&store, &unique("usr")).await;
}

#[tokio::test]
async fn the_table_api_key_store_conforms() {
    on_every_backend(|db| async move {
        let store = TableApiKeyStore::new(db);
        store.create_table().await.expect("the table is created");
        api_key_suite(&store, &unique("usr")).await;
    })
    .await;
}

#[cfg(feature = "passkeys")]
#[tokio::test]
async fn the_memory_passkey_store_conforms() {
    let store = MemoryPasskeyStore::new();
    passkey_suite(&store, &unique("usr")).await;
}

#[cfg(feature = "passkeys")]
#[tokio::test]
async fn the_table_passkey_store_conforms() {
    on_every_backend(|db| async move {
        let store = TablePasskeyStore::new(db);
        store.create_table().await.expect("the table is created");
        passkey_suite(&store, &unique("usr")).await;
    })
    .await;
}
