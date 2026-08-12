//! Passkeys, in a map and in a table, with the signature counter and the
//! quarantine bit as columns of their own.
//!
//! [`MemoryPasskeyStore`] is complete rather than a test double: the unique
//! index on the credential id, the authoritative counter column and the
//! quarantine bit that outlives a lookup are all implemented there. It is also
//! one address space, and its `Debug` says so — a passkey registered against one
//! instance is unknown to the next, and, the part that matters, a credential
//! quarantined on one still works on the other, because quarantine is the
//! response to a *cloned* authenticator. [`TablePasskeyStore`] is the same six
//! operations with the rows somewhere every instance can see, and
//! [`conformance`](super::conformance) runs one suite against both so neither
//! can drift.
//!
//! # Two columns carry the state that changes after registration
//!
//! [`PasskeyCredential::record`] is the opaque ceremony state. Two things are
//! lifted out of it into columns of their own, because a store has to be able to
//! write them without reopening CBOR:
//!
//! | Column | Written by | Read by |
//! | --- | --- | --- |
//! | `sign_count` | [`PasskeyStore::update_counter`] | the credential rebuild, which overrides the record's copy |
//! | `disabled` | [`PasskeyStore::disable`] | `WebAuthn::assert`, which refuses a quarantined credential by name |
//!
//! There is exactly one authoritative counter and it is the column. That is what
//! makes clone detection possible at all: an authenticator increments its
//! counter on every assertion, a copy of it falls behind, and a counter that
//! arrives *lower* than the stored one is the only evidence a server ever gets
//! that two devices hold one private key.
//!
//! # Where the clone check lives, and where it does not
//!
//! Not in `update_counter`. `WebAuthn::assert` refuses a regressed counter
//! before a store is ever asked to write it and reports it distinctly
//! ([`is_clone_detected`](crate::webauthn::is_clone_detected)), so a store that
//! clamped here would be hiding a caller that skipped the verifier rather than
//! protecting anybody. `update_counter` writes what it is told, in both
//! directions.
//!
//! The response to the signal is [`PasskeyStore::disable`], and *that* is the
//! compare-and-set:
//!
//! ```text
//! update moso_auth_passkeys
//!    set disabled = $1              -- true
//!  where credential_id = $2
//!    and disabled     = $3          -- false
//! ```
//!
//! The affected row count is the answer, so two requests quarantining one
//! credential produce one `true` and one `false` rather than two alerts for one
//! event. The row is **kept**: an audit that cannot resolve a credential id is
//! not an audit, and the user has to be told which of their keys stopped working
//! and why.
//!
//! # Doctests
//!
//! The examples that need a database are `no_run`: they compile on every machine
//! and connect on none, because a doctest that needs a PostgreSQL server would
//! fail on a laptop rather than teach anything.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine as _;
use moso_core::BoxFuture;
use moso_orm::{Db, RawQuery, Row};

use crate::store::schema::{PASSKEYS_SCHEMA, PASSKEYS_TABLE, PASSKEYS_USER_INDEX};
use crate::store::sql::{
    create_objects, encode_strings, fetch, flag, integer, json, malformed, placeholder,
    placeholders, run, stamp, string_array, text, text_opt, unavailable, unstamp, unstamp_opt,
};
use crate::webauthn::{PasskeyCredential, PasskeyStore};
use crate::{Error, Result};

/// What this store is called when it cannot be reached.
const COMPONENT: &str = "passkey store";

/// The base64 alphabet the `public_key` column is written in.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The columns, in the order every statement here reads them.
const COLUMNS: &str = "credential_id, user_id, public_key, sign_count, aaguid, discoverable, \
                       label, created_at, last_used_at, user_handle, user_verified, \
                       backup_eligible, backup_state, algorithm, transports, disabled, record";

// ---------------------------------------------------------------------------
// MemoryPasskeyStore
// ---------------------------------------------------------------------------

/// Passkeys in a map.
///
/// Every rule the trait states is implemented here in the shape
/// [`TablePasskeyStore`] reproduces, and the two are held to it by one shared
/// suite. What it does not do is outlive the process or reach a second one.
///
/// ```
/// use moso_auth::store::MemoryPasskeyStore;
/// use moso_auth::PasskeyStore;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso_auth::Result<()> {
/// let store = MemoryPasskeyStore::new();
/// assert!(store.is_empty());
/// assert!(store.find("unknown").await?.is_none());
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct MemoryPasskeyStore {
    /// Credentials by credential id — the same primary key the table has, and
    /// the same index the discoverable flow looks up.
    rows: Mutex<HashMap<String, PasskeyCredential>>,
}

impl core::fmt::Debug for MemoryPasskeyStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // A count, never the rows: a credential carries a user id and a public
        // key, and neither belongs in a log line. `single_process` is printed
        // because "why did my passkey stop working on the other instance" is
        // otherwise a very long afternoon.
        f.debug_struct("MemoryPasskeyStore")
            .field("credentials", &self.len())
            .field("single_process", &true)
            .finish_non_exhaustive()
    }
}

impl MemoryPasskeyStore {
    /// An empty store.
    ///
    /// ```
    /// use moso_auth::store::MemoryPasskeyStore;
    ///
    /// assert!(MemoryPasskeyStore::new().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty store behind an [`Arc`], which is the shape every caller wants.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_auth::store::MemoryPasskeyStore;
    /// use moso_auth::PasskeyStore;
    ///
    /// let store: Arc<dyn PasskeyStore> = MemoryPasskeyStore::shared();
    /// let _ = store;
    /// ```
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// How many credentials are on record, disabled ones included.
    ///
    /// ```
    /// use moso_auth::store::MemoryPasskeyStore;
    ///
    /// assert_eq!(MemoryPasskeyStore::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.lock().map(|rows| rows.len()).unwrap_or_default()
    }

    /// Whether nothing has been registered.
    ///
    /// ```
    /// use moso_auth::store::MemoryPasskeyStore;
    ///
    /// assert!(MemoryPasskeyStore::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take the lock, mapping poisoning onto an outage rather than a panic.
    fn lock(&self) -> Result<MutexGuard<'_, HashMap<String, PasskeyCredential>>> {
        self.rows.lock().map_err(|_| Error::Unavailable {
            component: COMPONENT,
            detail: "the in-memory store's lock was poisoned by a panic".to_owned(),
            source: None,
        })
    }
}

impl PasskeyStore for MemoryPasskeyStore {
    fn insert<'a>(&'a self, credential: &'a PasskeyCredential) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut rows = self.lock()?;
            if rows.contains_key(&credential.credential_id) {
                // The primary key the table carries. Overwriting instead would
                // let a second registration of a credential id somebody else
                // already holds move that credential onto a new account.
                return Err(malformed(
                    COMPONENT,
                    "a credential with this identifier is already registered",
                ));
            }
            rows.insert(credential.credential_id.clone(), credential.clone());
            Ok(())
        })
    }

    fn find<'a>(
        &'a self,
        credential_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<PasskeyCredential>>> {
        // Disabled credentials come back rather than being hidden: a caller that
        // could not see one would report a cloned key as an unknown key.
        Box::pin(async move { Ok(self.lock()?.get(credential_id).cloned()) })
    }

    fn list_for_user<'a>(
        &'a self,
        user_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<PasskeyCredential>>> {
        Box::pin(async move {
            let mut found: Vec<PasskeyCredential> = self
                .lock()?
                .values()
                .filter(|credential| credential.user_id == user_id)
                .cloned()
                .collect();
            found.sort_by_key(|credential| credential.created_at);
            Ok(found)
        })
    }

    fn update_counter<'a>(
        &'a self,
        credential_id: &'a str,
        sign_count: u32,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // What it is told, and nothing when there is no such row — the same
            // two outcomes the table's single `update` has.
            if let Some(credential) = self.lock()?.get_mut(credential_id) {
                credential.sign_count = sign_count;
            }
            Ok(())
        })
    }

    fn disable<'a>(&'a self, credential_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let mut rows = self.lock()?;
            let Some(credential) = rows.get_mut(credential_id) else {
                return Ok(false);
            };
            if credential.disabled {
                return Ok(false);
            }
            credential.disable();
            Ok(true)
        })
    }

    fn delete<'a>(&'a self, credential_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(self.lock()?.remove(credential_id).is_some()) })
    }
}

// ---------------------------------------------------------------------------
// TablePasskeyStore
// ---------------------------------------------------------------------------

/// Passkeys in a table.
///
/// ```no_run
/// use moso_auth::store::TablePasskeyStore;
/// use moso_auth::PasskeyStore;
/// use moso_orm::Db;
///
/// # async fn example() -> moso_auth::Result<()> {
/// let db = Db::connect_url("postgres://moso:moso@localhost/moso").await.unwrap();
/// let store = TablePasskeyStore::new(db);
/// store.create_table().await?;
/// assert!(store.find("unknown").await?.is_none());
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct TablePasskeyStore {
    /// Where the rows are.
    db: Db,
}

impl TablePasskeyStore {
    /// A store over `db`.
    ///
    /// ```no_run
    /// # use moso_auth::store::TablePasskeyStore;
    /// # use moso_orm::Db;
    /// # fn f(db: Db) { let _ = TablePasskeyStore::new(db); }
    /// ```
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// The database this reads and writes.
    ///
    /// ```no_run
    /// # use moso_auth::store::TablePasskeyStore;
    /// # use moso_orm::{Backend, Db};
    /// # fn f(store: &TablePasskeyStore) -> Backend { store.db().backend() }
    /// ```
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Create the table and its index, if they are not there.
    ///
    /// For tests and for `moso dev`. A production deployment puts
    /// [`PASSKEYS_SCHEMA`](crate::store::PASSKEYS_SCHEMA) in a reviewed
    /// migration instead, or lets [`descriptors`](crate::store::descriptors)
    /// hand the table to `moso db make-migration`.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the statement cannot be executed.
    ///
    /// ```no_run
    /// # use moso_auth::store::TablePasskeyStore;
    /// # async fn f(store: &TablePasskeyStore) -> moso_auth::Result<()> {
    /// store.create_table().await
    /// # }
    /// ```
    pub async fn create_table(&self) -> Result<()> {
        create_objects(&self.db, COMPONENT, &[PASSKEYS_SCHEMA, PASSKEYS_USER_INDEX]).await
    }

    /// How many credentials are on record, disabled ones included.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::store::TablePasskeyStore;
    /// # async fn f(store: &TablePasskeyStore) -> moso_auth::Result<i64> {
    /// store.len().await
    /// # }
    /// ```
    pub async fn len(&self) -> Result<i64> {
        let rows = fetch(
            &self.db,
            COMPONENT,
            RawQuery::new(format!("select count(*) from {PASSKEYS_TABLE}")),
        )
        .await?;
        rows.first()
            .map(|row| row.get_i64(0).unwrap_or_default())
            .ok_or_else(|| Error::Unavailable {
                component: COMPONENT,
                detail: "count(*) returned no row".to_owned(),
                source: None,
            })
    }

    /// The `n`th bind placeholder in this backend's spelling.
    fn placeholder(&self, n: usize) -> String {
        placeholder(self.db.backend(), n)
    }

    /// Read one column list back.
    async fn select(&self, predicate: &str, value: &str, order: &str) -> Result<Vec<Row>> {
        let sql = format!(
            "select {COLUMNS} from {PASSKEYS_TABLE} where {predicate} = {}{order}",
            self.placeholder(1)
        );
        fetch(&self.db, COMPONENT, RawQuery::new(sql).bind_text(value)).await
    }
}

impl PasskeyStore for TablePasskeyStore {
    fn insert<'a>(&'a self, credential: &'a PasskeyCredential) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let sql = format!(
                "insert into {PASSKEYS_TABLE} ({COLUMNS}) values ({})",
                placeholders(self.db.backend(), 17)
            );
            let query = RawQuery::new(sql)
                .bind_text(&credential.credential_id)
                .bind_text(&credential.user_id)
                .bind_text(&B64.encode(&credential.public_key))
                .bind(i64::from(credential.sign_count))
                .bind(credential.aaguid.clone())
                .bind(credential.discoverable)
                .bind(credential.label.clone())
                .bind_text(&stamp(credential.created_at))
                .bind(credential.last_used_at.map(stamp))
                .bind_text(&credential.user_handle)
                .bind(credential.user_verified)
                .bind(credential.backup_eligible)
                .bind(credential.backup_state)
                .bind(credential.algorithm)
                .bind_text(&encode_strings(COMPONENT, &credential.transports)?)
                .bind(credential.disabled)
                .bind_text(&credential.record.to_string());

            match query.execute(&self.db).await {
                Ok(_) => Ok(()),
                // The primary key, refused rather than overwritten: an upsert
                // here would let a second registration of a credential id
                // somebody else already holds move that credential onto a new
                // account. Same answer, same wording, as the in-memory store.
                Err(moso_orm::Error::UniqueViolation(_)) => Err(malformed(
                    COMPONENT,
                    "a credential with this identifier is already registered",
                )),
                Err(error) => Err(unavailable(COMPONENT, error)),
            }
        })
    }

    fn find<'a>(
        &'a self,
        credential_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<PasskeyCredential>>> {
        Box::pin(async move {
            // By credential id and nothing else: the discoverable flow has no
            // user id to narrow it with, which is exactly what makes it
            // usernameless. Disabled rows come back rather than being filtered —
            // `WebAuthn::assert` refuses them by name, and a caller that could
            // not see one would report a cloned key as an unknown key.
            let rows = self.select("credential_id", credential_id, "").await?;
            rows.first().map(decode_row).transpose()
        })
    }

    fn list_for_user<'a>(
        &'a self,
        user_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<PasskeyCredential>>> {
        Box::pin(async move {
            let rows = self
                .select("user_id", user_id, " order by created_at")
                .await?;
            rows.iter().map(decode_row).collect()
        })
    }

    fn update_counter<'a>(
        &'a self,
        credential_id: &'a str,
        sign_count: u32,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // Writes what it is told, in both directions, and says nothing when
            // there is no such row. A counter that went backwards never reaches
            // here from a verified ceremony, so clamping would hide a caller
            // that skipped `WebAuthn::assert` rather than protect anybody — and
            // the response to a real regression is `disable`, not a silent
            // refusal to write.
            let sql = format!(
                "update {PASSKEYS_TABLE} set sign_count = {} where credential_id = {}",
                self.placeholder(1),
                self.placeholder(2)
            );
            run(
                &self.db,
                COMPONENT,
                RawQuery::new(sql)
                    .bind(i64::from(sign_count))
                    .bind_text(credential_id),
            )
            .await?;
            Ok(())
        })
    }

    fn disable<'a>(&'a self, credential_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            // A compare-and-set, so two requests quarantining one credential
            // produce one `true` and one `false` rather than two alerts for one
            // event. The row is kept, never deleted: deleting a cloned
            // credential destroys the evidence and leaves the user with no
            // explanation for a key that stopped working.
            let sql = format!(
                "update {PASSKEYS_TABLE} set disabled = {} \
                 where credential_id = {} and disabled = {}",
                self.placeholder(1),
                self.placeholder(2),
                self.placeholder(3)
            );
            let affected = run(
                &self.db,
                COMPONENT,
                RawQuery::new(sql)
                    .bind(true)
                    .bind_text(credential_id)
                    .bind(false),
            )
            .await?;
            Ok(affected > 0)
        })
    }

    fn delete<'a>(&'a self, credential_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let sql = format!(
                "delete from {PASSKEYS_TABLE} where credential_id = {}",
                self.placeholder(1)
            );
            let affected = run(
                &self.db,
                COMPONENT,
                RawQuery::new(sql).bind_text(credential_id),
            )
            .await?;
            Ok(affected > 0)
        })
    }
}

/// Rebuild a credential from a row of [`PASSKEYS_TABLE`].
fn decode_row(row: &Row) -> Result<PasskeyCredential> {
    let public_key = B64
        .decode(text(COMPONENT, row, 2, "public_key")?)
        .map_err(|error| malformed(COMPONENT, format!("`public_key` is not base64url: {error}")))?;
    let sign_count = u32::try_from(integer(COMPONENT, row, 3, "sign_count")?).map_err(|_| {
        malformed(
            COMPONENT,
            "`sign_count` does not fit in the 32 bits WebAuthn defines it as",
        )
    })?;

    Ok(PasskeyCredential {
        credential_id: text(COMPONENT, row, 0, "credential_id")?,
        user_id: text(COMPONENT, row, 1, "user_id")?,
        public_key,
        sign_count,
        aaguid: text_opt(row, 4),
        discoverable: flag(COMPONENT, row, 5, "discoverable")?,
        label: text_opt(row, 6),
        created_at: unstamp(COMPONENT, &text(COMPONENT, row, 7, "created_at")?)?,
        last_used_at: unstamp_opt(COMPONENT, text_opt(row, 8))?,
        user_handle: text(COMPONENT, row, 9, "user_handle")?,
        user_verified: flag(COMPONENT, row, 10, "user_verified")?,
        backup_eligible: flag(COMPONENT, row, 11, "backup_eligible")?,
        backup_state: flag(COMPONENT, row, 12, "backup_state")?,
        algorithm: integer(COMPONENT, row, 13, "algorithm")?,
        transports: string_array(row, 14),
        disabled: flag(COMPONENT, row, 15, "disabled")?,
        record: json(row, 16, serde_json::Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::conformance::{on_every_backend, sample_credential, unique};

    /// A store over `db`, with its table created.
    async fn store(db: Db) -> TablePasskeyStore {
        let store = TablePasskeyStore::new(db);
        store.create_table().await.expect("the table is created");
        store
    }

    /// The counter column is the authoritative one, and `update_counter` writes
    /// it without reopening the ceremony record beside it.
    #[tokio::test]
    async fn the_counter_column_moves_without_touching_the_record() {
        on_every_backend(|db| async move {
            let store = store(db).await;
            let id = unique("cred");
            let credential = sample_credential(&id, &unique("usr"));
            store.insert(&credential).await.expect("inserted");

            store
                .update_counter(&id, 4_100_000_000)
                .await
                .expect("updated");
            let after = store
                .find(&id)
                .await
                .expect("looked up")
                .expect("still there");
            assert_eq!(
                after.sign_count, 4_100_000_000,
                "a counter above i32's range still round trips"
            );
            assert_eq!(
                after.record, credential.record,
                "the opaque record is untouched"
            );

            store.delete(&id).await.expect("cleaned up");
        })
        .await;
    }

    /// Quarantine is a compare-and-set, and the row survives it. A lookup that
    /// hid a disabled credential would turn "this key is cloned" into "this key
    /// does not exist", and the user would never be told which key stopped
    /// working.
    #[tokio::test]
    async fn disabling_is_idempotent_and_the_row_stays_resolvable() {
        on_every_backend(|db| async move {
            let store = store(db).await;
            let id = unique("cred");
            let user = unique("usr");
            store
                .insert(&sample_credential(&id, &user))
                .await
                .expect("inserted");

            assert!(store.disable(&id).await.expect("disabled"));
            assert!(
                !store.disable(&id).await.expect("disabled"),
                "quarantining twice is one event, not two"
            );

            let after = store
                .find(&id)
                .await
                .expect("looked up")
                .expect("a disabled credential is kept");
            assert!(!after.is_active());
            assert_eq!(
                store.list_for_user(&user).await.expect("listed").len(),
                1,
                "and the user can still be told which key stopped working"
            );

            store.delete(&id).await.expect("cleaned up");
        })
        .await;
    }

    /// A credential id is a primary key, not a suggestion: overwriting on a
    /// collision would let a second registration move somebody else's credential
    /// onto a new account.
    #[tokio::test]
    async fn a_duplicate_credential_id_is_refused_rather_than_overwritten() {
        on_every_backend(|db| async move {
            let store = store(db).await;
            let id = unique("cred");
            let mine = sample_credential(&id, &unique("usr"));
            store.insert(&mine).await.expect("inserted");

            let error = store
                .insert(&sample_credential(&id, &unique("attacker")))
                .await
                .expect_err("a duplicate identifier is refused");
            assert!(matches!(error, Error::Unavailable { .. }), "{error}");
            assert_eq!(
                store
                    .find(&id)
                    .await
                    .expect("looked up")
                    .expect("still there")
                    .user_id,
                mine.user_id,
                "and the original owner keeps it"
            );

            store.delete(&id).await.expect("cleaned up");
        })
        .await;
    }

    /// Bytes that are not UTF-8 and are not printable: a column that stored
    /// these as text would corrupt a public key silently.
    #[tokio::test]
    async fn a_binary_public_key_survives_the_round_trip_untouched() {
        on_every_backend(|db| async move {
            let store = store(db).await;
            let id = unique("cred");
            let mut credential = sample_credential(&id, &unique("usr"));
            credential.public_key = vec![0x00, 0xff, 0xfe, 0x80, 0x0a, 0x0d, 0x27, 0x22];
            store.insert(&credential).await.expect("inserted");

            let found = store
                .find(&id)
                .await
                .expect("looked up")
                .expect("still there");
            assert_eq!(found.public_key, credential.public_key);

            store.delete(&id).await.expect("cleaned up");
        })
        .await;
    }
}
