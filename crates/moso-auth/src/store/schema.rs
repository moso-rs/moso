//! The four tables this battery owns, stated twice on purpose, and a test that
//! stops the two statements from drifting apart.
//!
//! # Twice?
//!
//! Yes, and the two forms answer different questions:
//!
//! | Form | Who reads it | What it is for |
//! | --- | --- | --- |
//! | The `*_SCHEMA` constants | a human | paste into a reviewed migration; run in a test |
//! | [`descriptors`] | `moso-migrate` | diff against the committed snapshot, plan, emit |
//!
//! Non-negotiable N6 is that a migration is *read* before it is run, so the
//! constants exist for the operator who wants to see the `create table` and put
//! it in their own migration by hand. That is the pattern
//! [`SESSIONS_SCHEMA`](crate::store::SESSIONS_SCHEMA) has always followed and
//! the three new tables follow it too.
//!
//! But a hand-copied constant is invisible to `moso db check`, so the same
//! tables are also published as [`EntityDescriptor`]s. Hand them to
//! `moso db make-migration` with the application's own entities and the
//! generator writes the migration, the snapshot and the reverse statements; hand
//! them to `moso db check` and drift on an auth table fails CI like drift on any
//! other table.
//!
//! Two statements of one fact is exactly the redundancy this codebase refuses,
//! so the test `the_constants_and_the_descriptors_describe_the_same_tables` at
//! the bottom of this file reads every constant back and compares it, column by
//! column and index by index, with the descriptor beside it. Add a column to one
//! and the test names the column that is missing from the other.
//!
//! # Why every timestamp is `text`
//!
//! One statement has to run on PostgreSQL and on SQLite. RFC 3339 with a fixed
//! sub-second width sorts lexicographically, so `expires_at > $1` needs no
//! `timestamptz`/`datetime` divergence and no cast. `boolean` and `bigint` are
//! spelled identically on both backends and are used where a flag or a counter
//! is genuinely one, because writing a counter as text would make
//! `sign_count <= $1` a string comparison and `9` would sort after `10`.
//!
//! # Why there is no foreign key to your user table
//!
//! `user_id`, `owner` and `subject` are `text`, indexed, and reference nothing.
//! This crate cannot know what an application's user table is called or whether
//! its key is a `uuid`, a `bigint` or a `text` slug, and a foreign key it
//! guessed wrong would be a failed migration on somebody's production database.
//! The index is what makes the lookup one seek; the constraint is the
//! application's to add in its own migration if it wants one.

use std::sync::OnceLock;

use moso_orm::SqlType;
use moso_orm::descriptor::{ColumnDescriptor, EntityDescriptor, IndexDescriptor};
use moso_orm::prelude::{Ident, TableRef};

use crate::store::SESSIONS_TABLE;

// ---------------------------------------------------------------------------
// Refresh tokens
// ---------------------------------------------------------------------------

/// The table [`TableRefreshStore`](crate::store::TableRefreshStore) reads and
/// writes.
///
/// ```
/// assert_eq!(moso_auth::store::REFRESH_TOKENS_TABLE, "moso_auth_refresh_tokens");
/// ```
pub const REFRESH_TOKENS_TABLE: &str = "moso_auth_refresh_tokens";

/// The DDL for [`REFRESH_TOKENS_TABLE`], valid on PostgreSQL and SQLite alike.
///
/// `token_hash` is the primary key rather than a surrogate id, because the hash
/// *is* the lookup and *is* unique: a separate id column would buy a second
/// index and nothing else.
///
/// `used` is the single bit reuse detection rests on, and it is what makes
/// [`RefreshStore::exchange`](crate::RefreshStore::exchange) a compare-and-set
/// rather than a read-then-write.
///
/// ```
/// assert!(moso_auth::store::REFRESH_TOKENS_SCHEMA.contains("used"));
/// ```
pub const REFRESH_TOKENS_SCHEMA: &str = "\
create table if not exists moso_auth_refresh_tokens (
    token_hash text primary key,
    family     text not null,
    subject    text not null,
    issued_at  text not null,
    expires_at text not null,
    used       boolean not null
)";

/// The index that makes burning a family one seek instead of a scan.
///
/// Reuse detection revokes every token descended from the replayed one, so this
/// index is on the path of the operation that has to be *fast when it matters* —
/// the one running while an attacker holds a stolen token.
///
/// ```
/// assert!(moso_auth::store::REFRESH_TOKENS_FAMILY_INDEX.contains("family"));
/// ```
pub const REFRESH_TOKENS_FAMILY_INDEX: &str = "create index if not exists \
     moso_auth_refresh_tokens_family on moso_auth_refresh_tokens (family)";

/// The index behind "log out everywhere" for token-authenticated clients.
///
/// ```
/// assert!(moso_auth::store::REFRESH_TOKENS_SUBJECT_INDEX.contains("subject"));
/// ```
pub const REFRESH_TOKENS_SUBJECT_INDEX: &str = "create index if not exists \
     moso_auth_refresh_tokens_subject on moso_auth_refresh_tokens (subject)";

/// The index the expiry sweep uses.
///
/// ```
/// assert!(moso_auth::store::REFRESH_TOKENS_EXPIRY_INDEX.contains("expires_at"));
/// ```
pub const REFRESH_TOKENS_EXPIRY_INDEX: &str = "create index if not exists \
     moso_auth_refresh_tokens_expires_at on moso_auth_refresh_tokens (expires_at)";

// ---------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------

/// The table [`TableApiKeyStore`](crate::store::TableApiKeyStore) reads and
/// writes.
///
/// ```
/// assert_eq!(moso_auth::store::API_KEYS_TABLE, "moso_auth_api_keys");
/// ```
pub const API_KEYS_TABLE: &str = "moso_auth_api_keys";

/// The DDL for [`API_KEYS_TABLE`], valid on PostgreSQL and SQLite alike.
///
/// `prefix` carries the `unique` — which is an index on both backends, so there
/// is no separate index constant for it. That uniqueness is load-bearing twice:
/// it makes the lookup a single seek, and it turns a prefix collision into a
/// constraint violation the caller retries rather than two keys one lookup
/// cannot tell apart.
///
/// `hash` is deliberately *not* indexed and never appears in a `where` clause.
/// A query that filtered on it would make the database's own comparison the
/// timing oracle that [`ApiKey::verify_secret`](crate::ApiKey::verify_secret)
/// exists to avoid.
///
/// ```
/// assert!(moso_auth::store::API_KEYS_SCHEMA.contains("prefix"));
/// ```
pub const API_KEYS_SCHEMA: &str = "\
create table if not exists moso_auth_api_keys (
    id           text primary key,
    prefix       text not null unique,
    hash         text not null,
    environment  text not null,
    name         text not null,
    owner        text not null,
    scopes       text not null,
    created_at   text not null,
    expires_at   text,
    last_used_at text,
    revoked_at   text
)";

/// The index behind "show me my keys".
///
/// ```
/// assert!(moso_auth::store::API_KEYS_OWNER_INDEX.contains("owner"));
/// ```
pub const API_KEYS_OWNER_INDEX: &str = "create index if not exists \
     moso_auth_api_keys_owner on moso_auth_api_keys (owner)";

// ---------------------------------------------------------------------------
// Passkeys
// ---------------------------------------------------------------------------

/// The table `TablePasskeyStore` (behind the `passkeys` feature) reads and
/// writes.
///
/// ```
/// assert_eq!(moso_auth::store::PASSKEYS_TABLE, "moso_auth_passkeys");
/// ```
pub const PASSKEYS_TABLE: &str = "moso_auth_passkeys";

/// The DDL for [`PASSKEYS_TABLE`], valid on PostgreSQL and SQLite alike.
///
/// `credential_id` is the primary key because the discoverable (usernameless)
/// flow looks a credential up with no user id in hand: the authenticator names
/// the credential and the server has to find it. Everything else on the row is
/// derived from `record` and exists so that an operator can answer "whose is
/// this, what kind of device, when was it last used" without deserialising CBOR.
///
/// `sign_count` is `bigint` and not `text`, because the clone check compares it
/// (`sign_count <= $1`) and a text comparison would put `9` after `10`.
///
/// `record` is `text` holding JSON, exactly as `moso_auth_sessions.data` is: a
/// `jsonb` column would need a `$1::jsonb` cast on PostgreSQL and no cast on
/// SQLite, which is the dialect divergence in the statement that this whole
/// module is shaped to avoid.
///
/// ```
/// assert!(moso_auth::store::PASSKEYS_SCHEMA.contains("credential_id"));
/// ```
pub const PASSKEYS_SCHEMA: &str = "\
create table if not exists moso_auth_passkeys (
    credential_id   text primary key,
    user_id         text not null,
    public_key      text not null,
    sign_count      bigint not null,
    aaguid          text,
    discoverable    boolean not null,
    label           text,
    created_at      text not null,
    last_used_at    text,
    user_handle     text not null,
    user_verified   boolean not null,
    backup_eligible boolean not null,
    backup_state    boolean not null,
    algorithm       bigint not null,
    transports      text not null,
    disabled        boolean not null,
    record          text not null
)";

/// The index behind "these are your passkeys", and behind the allow-list a
/// non-discoverable ceremony sends the browser.
///
/// ```
/// assert!(moso_auth::store::PASSKEYS_USER_INDEX.contains("user_id"));
/// ```
pub const PASSKEYS_USER_INDEX: &str = "create index if not exists \
     moso_auth_passkeys_user_id on moso_auth_passkeys (user_id)";

// ---------------------------------------------------------------------------
// The same four tables, as descriptors
// ---------------------------------------------------------------------------

/// Every table this battery owns, described for the migration generator.
///
/// There is no link-time registry (ADR-0004), so nothing walks a crate looking
/// for tables — an entity left off the list looks exactly like a table you want
/// dropped. That is why this is a function an application *calls*: adding it to
/// the entity list is the statement that says "these tables are mine too".
///
/// ```no_run
/// // `no_run`: `make_migration` writes files, and a doctest that writes into
/// // the source tree is a doctest that fails the second time it runs.
/// use moso_migrate::command::{self, MakeMigrationOptions};
/// use moso_orm::Backend;
/// use moso_orm::descriptor::EntityDescriptor;
///
/// # fn example(mine: &[&'static EntityDescriptor]) -> moso_migrate::Result<()> {
/// let mut entities: Vec<&EntityDescriptor> = mine.to_vec();
/// entities.extend(moso_auth::store::descriptors());
///
/// let report = command::make_migration(
///     "migrations",
///     Backend::Postgres,
///     &entities,
///     &MakeMigrationOptions::default().name("create auth tables"),
/// )?;
/// println!("{}", report.path().unwrap_or_default());
/// # Ok(())
/// # }
/// ```
///
/// From a shell, in a project whose `src/db.rs` passes these through:
///
/// ```text
/// moso db make-migration create_auth_tables
/// moso db migrate
/// ```
///
/// The passkeys table is in the list only when the `passkeys` feature is on, so
/// that `make_migration` on a default build does not create a table the disabled
/// feature would never use:
///
/// ```
/// let tables: Vec<&str> = moso_auth::store::descriptors()
///     .iter()
///     .map(|d| d.table().name().as_str())
///     .collect();
/// assert!(tables.starts_with(&[
///     "moso_auth_sessions",
///     "moso_auth_refresh_tokens",
///     "moso_auth_api_keys",
/// ]));
/// assert_eq!(
///     tables.contains(&"moso_auth_passkeys"),
///     cfg!(feature = "passkeys"),
/// );
/// ```
#[must_use]
pub fn descriptors() -> &'static [&'static EntityDescriptor] {
    static ALL: OnceLock<Vec<&'static EntityDescriptor>> = OnceLock::new();
    ALL.get_or_init(|| {
        #[cfg_attr(not(feature = "passkeys"), allow(unused_mut))]
        let mut all = vec![
            sessions_descriptor(),
            refresh_tokens_descriptor(),
            api_keys_descriptor(),
        ];
        #[cfg(feature = "passkeys")]
        all.push(passkeys_descriptor());
        all
    })
}

/// A `text not null` column.
fn text_column(name: &'static str) -> ColumnDescriptor {
    ColumnDescriptor::builder(Ident::from_static(name), <String as SqlType>::data_type()).build()
}

/// A `text` column that accepts `NULL`.
fn nullable_text_column(name: &'static str) -> ColumnDescriptor {
    ColumnDescriptor::builder(Ident::from_static(name), <String as SqlType>::data_type())
        .nullable()
        .build()
}

/// A `boolean not null` column.
fn flag_column(name: &'static str) -> ColumnDescriptor {
    ColumnDescriptor::builder(Ident::from_static(name), <bool as SqlType>::data_type()).build()
}

/// A `bigint not null` column.
#[cfg(feature = "passkeys")]
fn integer_column(name: &'static str) -> ColumnDescriptor {
    ColumnDescriptor::builder(Ident::from_static(name), <i64 as SqlType>::data_type()).build()
}

/// A single-column index.
fn index_on(name: &'static str, column: &'static str) -> IndexDescriptor {
    IndexDescriptor::builder(name)
        .column(Ident::from_static(column))
        .build()
}

/// [`SESSIONS_SCHEMA`](crate::store::SESSIONS_SCHEMA), described.
fn sessions_descriptor() -> &'static EntityDescriptor {
    static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
    DESCRIPTOR.get_or_init(|| {
        EntityDescriptor::builder("MosoAuthSession", TableRef::from_static(SESSIONS_TABLE))
            .comment("Moso's session store. Owned by moso-auth; do not hand-edit.")
            .column(
                ColumnDescriptor::builder(
                    Ident::from_static("id"),
                    <String as SqlType>::data_type(),
                )
                .primary_key()
                .build(),
            )
            .column(nullable_text_column("user_id"))
            .column(text_column("auth_hash"))
            .column(text_column("data"))
            .column(text_column("created_at"))
            .column(text_column("last_seen_at"))
            .column(text_column("expires_at"))
            .column(nullable_text_column("user_agent"))
            .column(nullable_text_column("ip"))
            .column(nullable_text_column("label"))
            .index(index_on("moso_auth_sessions_user_id", "user_id"))
            .index(index_on("moso_auth_sessions_expires_at", "expires_at"))
            .build()
    })
}

/// [`REFRESH_TOKENS_SCHEMA`], described.
fn refresh_tokens_descriptor() -> &'static EntityDescriptor {
    static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
    DESCRIPTOR.get_or_init(|| {
        EntityDescriptor::builder(
            "MosoAuthRefreshToken",
            TableRef::from_static(REFRESH_TOKENS_TABLE),
        )
        .comment("Moso's refresh-token families. Owned by moso-auth; do not hand-edit.")
        .column(
            ColumnDescriptor::builder(
                Ident::from_static("token_hash"),
                <String as SqlType>::data_type(),
            )
            .primary_key()
            .build(),
        )
        .column(text_column("family"))
        .column(text_column("subject"))
        .column(text_column("issued_at"))
        .column(text_column("expires_at"))
        .column(flag_column("used"))
        .index(index_on("moso_auth_refresh_tokens_family", "family"))
        .index(index_on("moso_auth_refresh_tokens_subject", "subject"))
        .index(index_on(
            "moso_auth_refresh_tokens_expires_at",
            "expires_at",
        ))
        .build()
    })
}

/// [`API_KEYS_SCHEMA`], described.
fn api_keys_descriptor() -> &'static EntityDescriptor {
    static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
    DESCRIPTOR.get_or_init(|| {
        EntityDescriptor::builder("MosoAuthApiKey", TableRef::from_static(API_KEYS_TABLE))
            .comment("Moso's API keys. Owned by moso-auth; do not hand-edit.")
            .column(
                ColumnDescriptor::builder(
                    Ident::from_static("id"),
                    <String as SqlType>::data_type(),
                )
                .primary_key()
                .build(),
            )
            .column(
                ColumnDescriptor::builder(
                    Ident::from_static("prefix"),
                    <String as SqlType>::data_type(),
                )
                .unique()
                .build(),
            )
            .column(text_column("hash"))
            .column(text_column("environment"))
            .column(text_column("name"))
            .column(text_column("owner"))
            .column(text_column("scopes"))
            .column(text_column("created_at"))
            .column(nullable_text_column("expires_at"))
            .column(nullable_text_column("last_used_at"))
            .column(nullable_text_column("revoked_at"))
            .index(index_on("moso_auth_api_keys_owner", "owner"))
            .build()
    })
}

/// [`PASSKEYS_SCHEMA`], described.
#[cfg(feature = "passkeys")]
fn passkeys_descriptor() -> &'static EntityDescriptor {
    static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
    DESCRIPTOR.get_or_init(|| {
        EntityDescriptor::builder("MosoAuthPasskey", TableRef::from_static(PASSKEYS_TABLE))
            .comment("Moso's passkey credentials. Owned by moso-auth; do not hand-edit.")
            .column(
                ColumnDescriptor::builder(
                    Ident::from_static("credential_id"),
                    <String as SqlType>::data_type(),
                )
                .primary_key()
                .build(),
            )
            .column(text_column("user_id"))
            .column(text_column("public_key"))
            .column(integer_column("sign_count"))
            .column(nullable_text_column("aaguid"))
            .column(flag_column("discoverable"))
            .column(nullable_text_column("label"))
            .column(text_column("created_at"))
            .column(nullable_text_column("last_used_at"))
            .column(text_column("user_handle"))
            .column(flag_column("user_verified"))
            .column(flag_column("backup_eligible"))
            .column(flag_column("backup_state"))
            .column(integer_column("algorithm"))
            .column(text_column("transports"))
            .column(flag_column("disabled"))
            .column(text_column("record"))
            .index(index_on("moso_auth_passkeys_user_id", "user_id"))
            .build()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SESSIONS_EXPIRY_INDEX, SESSIONS_SCHEMA, SESSIONS_USER_INDEX};

    // ── reading a `create table` back ─────────────────────────────────────

    /// One column, as the DDL constant declares it.
    #[derive(Debug)]
    struct Declared {
        /// The column name.
        name: String,
        /// Whether the column refuses `NULL`, a primary key counting as one.
        not_null: bool,
        /// Whether it carries `primary key`.
        primary_key: bool,
        /// Whether it carries `unique`.
        unique: bool,
    }

    /// Read a `create table` constant back into its columns.
    ///
    /// The constants in this file are machine-uniform on purpose — one column
    /// per line, name first — so this needs to be a splitter and not a parser.
    fn declared_columns(schema: &str) -> Vec<Declared> {
        let open = schema.find('(').expect("a create table has a column list");
        let close = schema.rfind(')').expect("and closes it");
        schema[open + 1..close]
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let mut words = entry.split_whitespace();
                let name = words.next().expect("a column name").to_owned();
                let modifiers = words.collect::<Vec<_>>().join(" ");
                let primary_key = modifiers.contains("primary key");
                Declared {
                    name,
                    not_null: primary_key || modifiers.contains("not null"),
                    primary_key,
                    unique: modifiers.contains("unique"),
                }
            })
            .collect()
    }

    /// The name a `create index` constant gives the index.
    fn declared_index_name(statement: &str) -> String {
        statement
            .split_whitespace()
            .skip_while(|word| *word != "exists")
            .nth(1)
            .expect("`create index if not exists <name>`")
            .to_owned()
    }

    /// Compare one constant with the descriptor beside it.
    fn assert_agrees(schema: &str, indexes: &[&str], descriptor: &EntityDescriptor, table: &str) {
        assert!(
            schema.contains(table),
            "{table}: the constant names a different table"
        );
        assert_eq!(descriptor.table().name().as_str(), table);

        let declared = declared_columns(schema);
        assert_eq!(
            declared.len(),
            descriptor.columns().len(),
            "{table}: {} columns in the DDL, {} in the descriptor",
            declared.len(),
            descriptor.columns().len()
        );

        for (column, described) in declared.iter().zip(descriptor.columns()) {
            let name = &column.name;
            assert_eq!(
                name.as_str(),
                described.name().as_str(),
                "{table}: the columns are in a different order"
            );
            assert_eq!(
                column.not_null,
                !described.is_nullable(),
                "{table}.{name}: nullability disagrees"
            );
            assert_eq!(
                column.primary_key,
                described.is_primary_key(),
                "{table}.{name}: primary key disagrees"
            );
            assert_eq!(
                column.unique,
                described.is_unique(),
                "{table}.{name}: uniqueness disagrees"
            );
        }

        let declared_names: Vec<String> =
            indexes.iter().copied().map(declared_index_name).collect();
        let described_names: Vec<String> = descriptor
            .indexes()
            .iter()
            .map(|index| index.name().as_str().to_owned())
            .collect();
        assert_eq!(
            declared_names, described_names,
            "{table}: the index constants and the descriptor's indexes disagree"
        );
    }

    // ── the drift gate ────────────────────────────────────────────────────

    /// Two statements of one fact, held together.
    ///
    /// The `*_SCHEMA` constants are what an operator copies into a migration;
    /// the descriptors are what `moso db make-migration` renders. If they ever
    /// describe different tables, an application that ran `create_table()` in
    /// development and the generated migration in production would have two
    /// different schemas and no way to notice.
    #[test]
    fn the_constants_and_the_descriptors_describe_the_same_tables() {
        assert_agrees(
            SESSIONS_SCHEMA,
            &[SESSIONS_USER_INDEX, SESSIONS_EXPIRY_INDEX],
            sessions_descriptor(),
            SESSIONS_TABLE,
        );
        assert_agrees(
            REFRESH_TOKENS_SCHEMA,
            &[
                REFRESH_TOKENS_FAMILY_INDEX,
                REFRESH_TOKENS_SUBJECT_INDEX,
                REFRESH_TOKENS_EXPIRY_INDEX,
            ],
            refresh_tokens_descriptor(),
            REFRESH_TOKENS_TABLE,
        );
        assert_agrees(
            API_KEYS_SCHEMA,
            &[API_KEYS_OWNER_INDEX],
            api_keys_descriptor(),
            API_KEYS_TABLE,
        );
        #[cfg(feature = "passkeys")]
        assert_agrees(
            PASSKEYS_SCHEMA,
            &[PASSKEYS_USER_INDEX],
            passkeys_descriptor(),
            PASSKEYS_TABLE,
        );
    }

    /// Every table is listed, and every index name is unique across the four —
    /// PostgreSQL's index names are per-schema, not per-table, so a collision
    /// between two of ours would be a failed migration.
    #[test]
    fn the_descriptor_list_is_complete_and_its_index_names_do_not_collide() {
        let expected = if cfg!(feature = "passkeys") { 4 } else { 3 };
        assert_eq!(descriptors().len(), expected);

        let mut names: Vec<&str> = descriptors()
            .iter()
            .flat_map(|descriptor| descriptor.indexes())
            .map(|index| index.name().as_str())
            .collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "two indexes share a name: {names:?}");
    }

    /// Every table this crate owns is prefixed, so it cannot collide with an
    /// application's own tables and an operator can find all of them with one
    /// `like`.
    #[test]
    fn every_table_this_battery_owns_is_prefixed() {
        for descriptor in descriptors() {
            let table = descriptor.table().name().as_str();
            assert!(table.starts_with("moso_auth_"), "{table} is not prefixed");
        }
    }

    /// The hash column must never be a lookup key: a `where hash = $1` would
    /// make the database's own comparison the timing oracle that
    /// `ApiKey::verify_secret` exists to avoid.
    #[test]
    fn the_api_key_hash_column_carries_no_index() {
        assert!(
            !api_keys_descriptor()
                .indexes()
                .iter()
                .any(|index| index.name().as_str().contains("hash")),
            "the secret hash must not be indexed"
        );
        assert!(
            !api_keys_descriptor()
                .column("hash")
                .expect("the column exists")
                .is_unique(),
            "a unique index on the hash is still an index on the hash"
        );
    }
}
