//! The upsert clause, and the machinery the three write builders share.
//!
//! # Why this module is reached through [`insert`](crate::insert)
//!
//! It is declared from `insert.rs` with `#[path = "upsert.rs"]`, so its items
//! live at `moso_orm::insert::upsert::…`. `src/lib.rs` is not mine to edit
//! during this build; promoting the declaration to `pub mod upsert;` there is a
//! one-line change that moves everything here to `moso_orm::upsert::…` and
//! breaks nothing, because [`Conflict`] and [`ConflictAction`] are already
//! re-exported from [`crate::insert`] and from the crate root.
//!
//! # What lives here
//!
//! | | |
//! | --- | --- |
//! | [`Conflict`], [`ConflictAction`] | the `ON CONFLICT` clause an upsert carries |
//! | [`write_error`] | non-negotiable N7: a SQLSTATE becomes an error that names a field |
//! | [`constraint_columns`](crate::insert::upsert::constraint_columns) | the constraint name → column derivation behind it |
//! | [`status_code`](crate::insert::upsert::status_code) | the HTTP status of `docs/01-http/16-errors.md`, in code |
//!
//! # The error mapping, and where it has to happen
//!
//! `docs/01-http/16-errors.md` requires `23505` to become a 409 whose problem
//! document points at the offending field. Deriving that pointer needs the
//! entity — its columns, and which Rust field each one came from — and the
//! execution layer does not have it: [`Handle`](crate::Handle) is deliberately
//! not generic (rule A2, erase early), so it can only report
//! [`Error::Database`].
//!
//! So the translation happens **in the write builders**, which do know `E`:
//! every `execute`/`fetch_*` in [`Insert`](crate::Insert),
//! [`Update`](crate::Update) and [`Delete`](crate::Delete) passes the driver's
//! error through [`write_error`]. It is idempotent — an already-classified
//! violation is re-seated onto the right entity rather than being wrapped twice
//! — so an execution layer that classifies eagerly does not break it.
//!
//! ```
//! use moso_orm::{Entity, Error};
//!
//! /// What every write path in this crate does with a driver error.
//! fn translate<E: Entity>(error: Error) -> Error {
//!     moso_orm::insert::upsert::write_error::<E>(error)
//! }
//! ```

use moso_sql::{ColumnRef, Expr, Function, Ident, Returning};

use crate::entity::Entity;
use crate::error::{ConstraintKind, ConstraintViolation, DatabaseError, Error};

/// What to do when an insert conflicts with an existing row.
///
/// ```
/// use moso_orm::{Conflict, ConflictAction};
/// use moso_sql::Ident;
///
/// let upsert = Conflict::new([Ident::from_static("email")])
///     .updating([Ident::from_static("name")]);
/// assert!(matches!(upsert.action(), ConflictAction::Update(_)));
/// ```
#[derive(Clone, Debug)]
pub struct Conflict {
    /// The columns of the unique index the insert may collide with. Empty
    /// means "any unique constraint", which SQL only allows with `DO NOTHING`.
    target: Vec<Ident>,
    /// What happens to the row that is already there.
    action: ConflictAction,
}

impl Conflict {
    /// A conflict on `target`, doing nothing.
    ///
    /// ```
    /// use moso_orm::Conflict;
    /// use moso_sql::Ident;
    ///
    /// assert_eq!(Conflict::new([Ident::from_static("email")]).target().len(), 1);
    /// ```
    #[must_use]
    pub fn new(target: impl IntoIterator<Item = Ident>) -> Self {
        Self {
            target: target.into_iter().collect(),
            action: ConflictAction::Nothing,
        }
    }

    /// Overwrites these columns instead.
    ///
    /// ```
    /// use moso_orm::{Conflict, ConflictAction};
    /// use moso_sql::Ident;
    ///
    /// let c = Conflict::new([Ident::from_static("a")]).updating([Ident::from_static("b")]);
    /// assert!(matches!(c.action(), ConflictAction::Update(_)));
    /// ```
    #[must_use]
    pub fn updating(mut self, columns: impl IntoIterator<Item = Ident>) -> Self {
        self.action = ConflictAction::Update(columns.into_iter().collect());
        self
    }

    /// The conflicting columns.
    ///
    /// ```
    /// # use moso_orm::Conflict;
    /// # use moso_sql::Ident;
    /// assert_eq!(Conflict::new([Ident::from_static("a")]).target().len(), 1);
    /// ```
    #[must_use]
    pub fn target(&self) -> &[Ident] {
        &self.target
    }

    /// What happens on a conflict.
    ///
    /// ```
    /// # use moso_orm::{Conflict, ConflictAction};
    /// # use moso_sql::Ident;
    /// assert!(matches!(Conflict::new([Ident::from_static("a")]).action(), ConflictAction::Nothing));
    /// ```
    #[must_use]
    pub const fn action(&self) -> &ConflictAction {
        &self.action
    }

    /// Replaces the action. The door [`Insert`](crate::Insert)'s combinators
    /// use, so that `.on_conflict(..).do_update(..)` can rewrite the clause it
    /// already built.
    pub(crate) fn set_action(&mut self, action: ConflictAction) {
        self.action = action;
    }
}

/// What an `ON CONFLICT` does.
///
/// ```
/// use moso_orm::ConflictAction;
/// use moso_sql::Ident;
///
/// assert!(ConflictAction::Update(vec![Ident::from_static("a")]).writes());
/// assert!(!ConflictAction::Nothing.writes());
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ConflictAction {
    /// Keep the existing row.
    Nothing,
    /// Overwrite these columns from the row being inserted.
    Update(Vec<Ident>),
}

impl ConflictAction {
    /// Whether a conflicting row is changed.
    ///
    /// ```
    /// use moso_orm::ConflictAction;
    ///
    /// assert!(!ConflictAction::Nothing.writes());
    /// ```
    #[must_use]
    pub const fn writes(&self) -> bool {
        matches!(self, Self::Update(_))
    }
}

/// Turns what the driver reported into an error that names the problem.
///
/// This is non-negotiable N7 in one function. `23505` stops being "database
/// error 23505" and becomes "a `User` with this email already exists", carrying
/// `/email` as its field pointer so the HTTP layer can render a 409 a client can
/// act on.
///
/// Idempotent, and safe to apply to any error: anything that is not a
/// constraint failure passes straight through, and an error that was already
/// classified without an entity is re-seated onto `E` rather than wrapped.
///
/// # What it reads, in order of precision
///
/// 1. PostgreSQL's `DETAIL: Key (email)=(…) already exists.` — the server
///    naming the columns itself, which is exact.
/// 2. `null value in column "email" of relation "users"` for `23502`.
/// 3. SQLite's `UNIQUE constraint failed: users.email`.
/// 4. The constraint name, looked up in the entity's indexes and foreign keys.
/// 5. The constraint name, parsed by convention — see [`constraint_columns`].
///
/// ```
/// use moso_orm::{Entity, Error};
///
/// fn translate<E: Entity>(error: Error) -> Error {
///     moso_orm::insert::upsert::write_error::<E>(error)
/// }
/// ```
#[must_use]
pub fn write_error<E: Entity>(error: Error) -> Error {
    match error {
        Error::Database(reported) => classify::<E>(*reported),
        Error::UniqueViolation(violation) => {
            Error::UniqueViolation(Box::new(reseat::<E>(*violation)))
        }
        Error::ForeignKeyViolation(violation) => {
            Error::ForeignKeyViolation(Box::new(reseat::<E>(*violation)))
        }
        Error::NotNullViolation(violation) => {
            Error::NotNullViolation(Box::new(reseat::<E>(*violation)))
        }
        Error::CheckViolation(violation) => {
            Error::CheckViolation(Box::new(reseat::<E>(*violation)))
        }
        other => other,
    }
}

/// The columns a constraint covers, derived from its name.
///
/// Two sources, in order:
///
/// 1. **The entity's own description.** An index or a foreign key the derive
///    emitted knows its columns, so a name it recognises needs no guessing.
/// 2. **The naming convention**, which is what PostgreSQL generates and what
///    `moso-migrate` emits: `{table}_{column}…_{key|pkey|fkey|check|excl}`. The
///    middle is matched greedily against the entity's column names, so
///    `users_tenant_id_email_key` resolves to `tenant_id` and `email` even
///    though both contain the separator.
///
/// An unrecognised name yields an empty vector rather than a guess: a wrong
/// field pointer sends a client to fix the wrong input.
///
/// ```
/// use moso_orm::Entity;
///
/// fn columns_of<E: Entity>(constraint: &str) -> Vec<&'static str> {
///     moso_orm::insert::upsert::constraint_columns::<E>(constraint)
/// }
/// ```
#[must_use]
pub fn constraint_columns<E: Entity>(constraint: &str) -> Vec<&'static str> {
    let descriptor = E::descriptor();

    if let Some(index) = descriptor
        .indexes()
        .iter()
        .find(|index| index.name().as_str() == constraint)
    {
        let columns: Vec<&'static str> = index
            .columns()
            .iter()
            .filter_map(|column| column.column_name().map(|name| name.as_str()))
            .collect();
        if !columns.is_empty() {
            return columns;
        }
    }

    if let Some(foreign_key) = descriptor
        .foreign_keys()
        .iter()
        .find(|foreign_key| foreign_key.name().as_str() == constraint)
    {
        let columns: Vec<&'static str> = foreign_key
            .columns()
            .iter()
            .map(moso_sql::Ident::as_str)
            .collect();
        if !columns.is_empty() {
            return columns;
        }
    }

    columns_by_convention::<E>(constraint)
}

/// The HTTP status `moso-core` renders this error as.
///
/// The table is `docs/01-http/16-errors.md`: a unique or foreign-key violation
/// is a 409, a check violation is a 422, a serialisation failure is a 409 once
/// [`Db::transaction`](crate::Db::transaction) has stopped retrying it, and
/// anything the client cannot act on is a 500.
///
/// It lives here, as a number, so that the mapping is testable without an HTTP
/// stack — and so that the day `moso-core` grows `From<moso_orm::Error>` there
/// is one place that decides, not two.
///
/// ```
/// use moso_orm::{ConstraintViolation, Error};
/// use moso_orm::insert::upsert::status_code;
///
/// let taken = ConstraintViolation::unique("User", "users_email_key").with_column("email");
/// assert_eq!(status_code(&Error::UniqueViolation(Box::new(taken))), 409);
/// assert_eq!(status_code(&Error::StaleWrite { entity: "Order" }), 409);
/// assert_eq!(status_code(&Error::not_found("User")), 404);
/// ```
#[must_use]
pub const fn status_code(error: &Error) -> u16 {
    match error {
        Error::NotFound { .. } => 404,
        Error::UniqueViolation(_)
        | Error::ForeignKeyViolation(_)
        | Error::StaleWrite { .. }
        | Error::Serialization { .. }
        | Error::Deadlock { .. } => 409,
        Error::CheckViolation(_) | Error::NotNullViolation(_) => 422,
        Error::Cursor(_) => 400,
        // Transient infrastructure, not the request's fault: a client that
        // retries has a real chance, which a 500 does not communicate.
        Error::PoolTimeout { .. } | Error::StatementTimeout { .. } | Error::Connection { .. } => {
            503
        }
        // Everything else — a build error, an unjoined column, a decode
        // failure, an unfiltered write — is the application's bug, and its
        // detail is suppressed by the problem renderer.
        _ => 500,
    }
}

/// `RETURNING` every column of `E`, **in `E::COLUMNS` order**.
///
/// Not `RETURNING *`: [`Entity::from_row`] decodes positionally, and the server
/// returns `*` in table order, which is the order the migrations happened to
/// create — not the order the struct declares. Listing the columns is what makes
/// the positional decode safe.
pub(crate) fn returning_entity<E: Entity>() -> Returning {
    Returning::columns(
        E::COLUMNS
            .iter()
            .map(|column| ColumnRef::new(column.ident())),
    )
}

/// `current_timestamp` — the value a soft delete and an `updated_at` bump write.
///
/// `now()` would be the PostgreSQL idiom; `current_timestamp` is the standard
/// spelling and is the same value on PostgreSQL while also existing on SQLite,
/// so the two dialects do not need a special case here.
pub(crate) fn current_timestamp() -> Expr {
    Function::CurrentTimestamp.into_expr()
}

/// The tenant column of `E`, when it is tenant-scoped.
pub(crate) fn tenant_column<E: Entity>() -> Option<Ident> {
    E::descriptor().tenant().cloned()
}

/// The soft-delete column of `E`, when it has one.
pub(crate) fn soft_delete_column<E: Entity>() -> Option<Ident> {
    E::descriptor().soft_delete().cloned()
}

/// The optimistic-locking column of `E`, when it has one.
pub(crate) fn version_column<E: Entity>() -> Option<Ident> {
    E::descriptor().version().cloned()
}

/// Turns a driver-reported error into a typed one.
fn classify<E: Entity>(reported: DatabaseError) -> Error {
    let sqlstate = reported.sqlstate().to_owned();

    let Some(kind) = constraint_kind(&sqlstate, reported.message()) else {
        return match sqlstate.as_str() {
            "40001" => Error::Serialization { code: sqlstate },
            "40P01" => Error::Deadlock { code: sqlstate },
            _ => Error::Database(Box::new(reported)),
        };
    };

    let constraint = constraint_name(reported.message())
        .map_or_else(|| default_constraint_label(kind).to_owned(), str::to_owned);

    let mut columns = reported_columns(reported.message(), reported.detail());
    if columns.is_empty() {
        columns = constraint_columns::<E>(&constraint)
            .into_iter()
            .map(str::to_owned)
            .collect();
    }

    let mut violation = ConstraintViolation::new(E::NAME, constraint, kind).with_sqlstate(sqlstate);
    if let Some(sql) = reported.sql() {
        violation = violation.with_sql(sql);
    }
    if let Some(at) = reported.call_site() {
        violation = violation.at(at);
    }
    violation = with_columns::<E>(violation, columns);
    wrap(kind, violation)
}

/// Fills in what an already-classified violation is missing: the entity it
/// belongs to, and the columns its constraint name implies.
fn reseat<E: Entity>(violation: ConstraintViolation) -> ConstraintViolation {
    if !violation.columns().is_empty() && violation.entity() == E::NAME {
        return violation;
    }

    let mut reseated = ConstraintViolation::new(E::NAME, violation.constraint(), violation.kind());
    if let Some(sqlstate) = violation.sqlstate() {
        reseated = reseated.with_sqlstate(sqlstate);
    }
    if let Some(sql) = violation.sql() {
        reseated = reseated.with_sql(sql);
    }
    if let Some(at) = violation.call_site() {
        reseated = reseated.at(at);
    }

    let mut columns: Vec<String> = violation.columns().to_vec();
    if columns.is_empty() {
        columns = constraint_columns::<E>(violation.constraint())
            .into_iter()
            .map(str::to_owned)
            .collect();
    }
    with_columns::<E>(reseated, columns)
}

/// Records the columns as the *fields* a client wrote, and writes the sentence
/// that names them.
fn with_columns<E: Entity>(
    mut violation: ConstraintViolation,
    columns: Vec<String>,
) -> ConstraintViolation {
    let mut first: Option<String> = None;
    for column in columns {
        let field = field_of::<E>(&column);
        if first.is_none() {
            first = Some(field.clone());
        }
        violation = violation.with_column(field);
    }
    match (violation.kind(), first) {
        (ConstraintKind::Unique, Some(field)) => {
            violation.with_message(format!("a {} with this {field} already exists", E::NAME))
        }
        (ConstraintKind::ForeignKey, Some(field)) => {
            violation.with_message(format!("`{field}` does not refer to a row that exists"))
        }
        (ConstraintKind::NotNull, Some(field)) => {
            violation.with_message(format!("`{field}` is required"))
        }
        _ => violation,
    }
}

/// The Rust field a column came from, so the JSON Pointer matches the request
/// body rather than the table. Falls back to the column name, which is what an
/// entity that never named its fields has.
fn field_of<E: Entity>(column: &str) -> String {
    E::descriptor()
        .column(column)
        .and_then(crate::descriptor::ColumnDescriptor::field)
        .map_or_else(|| column.to_owned(), str::to_owned)
}

/// Which error variant a constraint kind becomes.
///
/// `EXCLUDE` has no variant of its own and becomes a unique violation: it is
/// uniqueness over an operator rather than equality, a 409 either way, and the
/// [`ConstraintViolation::kind`] still says `Exclusion`, so nothing is lost.
fn wrap(kind: ConstraintKind, violation: ConstraintViolation) -> Error {
    let violation = Box::new(violation);
    match kind {
        ConstraintKind::Unique | ConstraintKind::Exclusion => Error::UniqueViolation(violation),
        ConstraintKind::ForeignKey => Error::ForeignKeyViolation(violation),
        ConstraintKind::NotNull => Error::NotNullViolation(violation),
        ConstraintKind::Check => Error::CheckViolation(violation),
    }
}

/// Which constraint refused, from the SQLSTATE — or, for SQLite's generic
/// `SQLITE_CONSTRAINT`, from the message it prints instead.
fn constraint_kind(sqlstate: &str, message: &str) -> Option<ConstraintKind> {
    match sqlstate {
        // PostgreSQL, class 23 — integrity constraint violation.
        "23505" => Some(ConstraintKind::Unique),
        "23503" => Some(ConstraintKind::ForeignKey),
        "23502" => Some(ConstraintKind::NotNull),
        "23514" => Some(ConstraintKind::Check),
        "23P01" => Some(ConstraintKind::Exclusion),
        // SQLite extended result codes, which sqlx reports as the "code".
        "1555" | "2067" => Some(ConstraintKind::Unique),
        "787" => Some(ConstraintKind::ForeignKey),
        "1299" => Some(ConstraintKind::NotNull),
        "275" => Some(ConstraintKind::Check),
        // SQLITE_CONSTRAINT with no extended code: the message is the only
        // thing that says which one.
        "19" => sqlite_kind(message),
        _ => None,
    }
}

/// SQLite's constraint messages, which name the kind in words.
fn sqlite_kind(message: &str) -> Option<ConstraintKind> {
    if message.starts_with("UNIQUE constraint failed") {
        Some(ConstraintKind::Unique)
    } else if message.starts_with("FOREIGN KEY constraint failed") {
        Some(ConstraintKind::ForeignKey)
    } else if message.starts_with("NOT NULL constraint failed") {
        Some(ConstraintKind::NotNull)
    } else if message.starts_with("CHECK constraint failed") {
        Some(ConstraintKind::Check)
    } else {
        None
    }
}

/// The label used when the server names no constraint, which is the case for
/// `NOT NULL` and for every SQLite failure.
const fn default_constraint_label(kind: ConstraintKind) -> &'static str {
    match kind {
        ConstraintKind::Unique => "unique",
        ConstraintKind::ForeignKey => "foreign-key",
        ConstraintKind::NotNull => "not-null",
        ConstraintKind::Check => "check",
        ConstraintKind::Exclusion => "exclusion",
    }
}

/// The constraint the server named.
///
/// PostgreSQL always writes it as `constraint "name"`, which is why this looks
/// for that exact shape rather than for the last quoted token: a foreign-key
/// message ends in `… on table "other"`, and the table is not the constraint.
///
/// SQLite has no quoting and names only a `CHECK` it was given a name for,
/// after the colon — `CHECK constraint failed: orders_total_positive`.
pub(crate) fn constraint_name(message: &str) -> Option<&str> {
    if let Some((_, rest)) = message.split_once("constraint \"")
        && let Some((name, _)) = rest.split_once('"')
        && !name.is_empty()
    {
        return Some(name);
    }
    if let Some((_, named)) = message.split_once("constraint failed: ")
        && !named.is_empty()
        && !named.contains('.')
        && !named.contains(' ')
    {
        return Some(named);
    }
    None
}

/// The columns the server itself named, which beats every heuristic.
pub(crate) fn reported_columns(message: &str, detail: Option<&str>) -> Vec<String> {
    // PostgreSQL: `DETAIL: Key (tenant_id, email)=(1, ada@example.com) …`
    if let Some(detail) = detail
        && let Some(keys) = between(detail, "Key (", ")=")
    {
        let columns: Vec<String> = keys
            .split(',')
            .map(|column| column.trim().to_owned())
            .filter(|column| !column.is_empty())
            .collect();
        if !columns.is_empty() {
            return columns;
        }
    }

    // PostgreSQL 23502: `null value in column "email" of relation "users" …`
    if let Some(column) = between(message, "null value in column \"", "\"") {
        return vec![column.to_owned()];
    }

    // SQLite: `UNIQUE constraint failed: users.email, users.tenant_id`. Only
    // the qualified parts are columns — an unqualified one is the name of a
    // `CHECK`, or the expression itself, and neither is a field.
    if let Some((_, listed)) = message.split_once("constraint failed: ") {
        let columns: Vec<String> = listed
            .split(',')
            .map(str::trim)
            .filter(|qualified| qualified.contains('.'))
            .filter_map(|qualified| qualified.rsplit('.').next())
            .filter(|column| !column.is_empty())
            .map(str::to_owned)
            .collect();
        if !columns.is_empty() {
            return columns;
        }
    }

    Vec::new()
}

/// The text between two markers, if both are there in that order.
fn between<'a>(haystack: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let (_, rest) = haystack.split_once(open)?;
    let (inside, _) = rest.split_once(close)?;
    Some(inside)
}

/// `{table}_{column}…_{suffix}` — the name PostgreSQL generates and the one
/// `moso-migrate` emits.
fn columns_by_convention<E: Entity>(constraint: &str) -> Vec<&'static str> {
    /// Every suffix an auto-generated constraint name ends in.
    const SUFFIXES: &[&str] = &[
        "_key", "_pkey", "_fkey", "_check", "_excl", "_unique", "_idx",
    ];

    let table = E::TABLE;
    let mut middle = constraint;
    if let Some(stripped) = middle.strip_prefix(table.name().as_str()) {
        middle = stripped.strip_prefix('_').unwrap_or(stripped);
    }
    for suffix in SUFFIXES {
        if let Some(stripped) = middle.strip_suffix(suffix) {
            middle = stripped;
            break;
        }
    }

    let mut rest = middle;
    let mut columns = Vec::new();
    while !rest.is_empty() {
        let Some(column) = longest_column::<E>(rest) else {
            // A name that does not decompose into this entity's columns is not
            // this entity's convention, and half an answer is worse than none.
            return Vec::new();
        };
        columns.push(column);
        rest = rest[column.len()..].strip_prefix('_').unwrap_or("");
    }
    columns
}

/// The longest column name of `E` that `rest` starts with, on a `_` boundary.
///
/// Longest wins so that `tenant_id_email` reads `tenant_id` and not `tenant`
/// when the entity happens to have both.
fn longest_column<E: Entity>(rest: &str) -> Option<&'static str> {
    E::COLUMNS
        .iter()
        .map(crate::entity::ColumnDef::name)
        .filter(|name| {
            rest.strip_prefix(*name)
                .is_some_and(|tail| tail.is_empty() || tail.starts_with('_'))
        })
        .max_by_key(|name| name.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{ColumnDescriptor, EntityDescriptor, IndexDescriptor};
    use crate::entity::ColumnDef;
    use crate::row::{DecodeError, Row};
    use moso_sql::{DataType, TableRef, ValueKind};
    use std::sync::OnceLock;

    /// A user, with the two columns a composite unique index covers.
    #[derive(Clone, Debug)]
    struct User {
        id: i64,
    }

    impl Entity for User {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("users");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("email", ValueKind::Text).unique(),
            ColumnDef::new("tenant_id", ValueKind::I64),
            ColumnDef::new("tenant", ValueKind::Text),
            ColumnDef::new("password_hash", ValueKind::Text),
        ];
        const NAME: &'static str = "User";

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| {
                EntityDescriptor::builder("User", Self::TABLE)
                    .column(
                        ColumnDescriptor::builder(Ident::from_static("email"), DataType::Text)
                            .build(),
                    )
                    .column(
                        ColumnDescriptor::builder(
                            Ident::from_static("password_hash"),
                            DataType::Text,
                        )
                        .field("password")
                        .build(),
                    )
                    .index(
                        IndexDescriptor::builder("users_unique_login")
                            .column(Ident::from_static("tenant_id"))
                            .column(Ident::from_static("email"))
                            .unique()
                            .build(),
                    )
                    .build()
            })
        }
    }

    fn reported(sqlstate: &str, message: &str) -> Error {
        Error::Database(Box::new(DatabaseError::new(sqlstate, message)))
    }

    #[test]
    fn a_unique_violation_becomes_a_409_pointing_at_the_field() {
        let error = write_error::<User>(reported(
            "23505",
            "duplicate key value violates unique constraint \"users_email_key\"",
        ));
        assert_eq!(status_code(&error), 409);
        assert_eq!(error.field_pointer().as_deref(), Some("/email"));
        assert_eq!(error.sqlstate(), Some("23505"));
        assert!(error.to_string().contains("User"), "{error}");
        assert!(!error.is_retryable());
    }

    #[test]
    fn the_servers_own_detail_beats_the_naming_convention() {
        let inner = DatabaseError::new(
            "23505",
            "duplicate key value violates unique constraint \"idx_login\"",
        )
        .with_detail("Key (tenant_id, email)=(1, ada@example.com) already exists.");
        let error = write_error::<User>(Error::Database(Box::new(inner)));
        let Error::UniqueViolation(violation) = &error else {
            panic!("expected a unique violation, got {error:?}");
        };
        assert_eq!(violation.columns(), ["tenant_id", "email"]);
        assert_eq!(error.field_pointer().as_deref(), Some("/tenant_id"));
    }

    #[test]
    fn a_composite_constraint_name_decomposes_greedily() {
        // `tenant_id` wins over `tenant`, and the remainder still resolves.
        assert_eq!(
            constraint_columns::<User>("users_tenant_id_email_key"),
            ["tenant_id", "email"]
        );
        // A name the entity's own index knows is not guessed at all.
        assert_eq!(
            constraint_columns::<User>("users_unique_login"),
            ["tenant_id", "email"]
        );
        // And a name that does not decompose yields nothing rather than a
        // wrong field pointer.
        assert!(constraint_columns::<User>("legacy_constraint_v2").is_empty());
    }

    #[test]
    fn the_pointer_names_the_rust_field_not_the_column() {
        let inner = DatabaseError::new("23505", "duplicate key value violates unique constraint")
            .with_detail("Key (password_hash)=(x) already exists.");
        let error = write_error::<User>(Error::Database(Box::new(inner)));
        assert_eq!(
            error.field_pointer().as_deref(),
            Some("/password"),
            "the column is `password_hash`; the field a client sent is `password`"
        );
    }

    #[test]
    fn a_foreign_key_violation_is_a_409_and_a_check_violation_is_a_422() {
        let foreign = write_error::<User>(reported(
            "23503",
            "insert or update on table \"users\" violates foreign key constraint \
             \"users_tenant_id_fkey\"",
        ));
        assert!(matches!(foreign, Error::ForeignKeyViolation(_)));
        assert_eq!(status_code(&foreign), 409);
        assert_eq!(foreign.field_pointer().as_deref(), Some("/tenant_id"));

        let check = write_error::<User>(reported(
            "23514",
            "new row for relation \"users\" violates check constraint \"users_email_check\"",
        ));
        assert!(matches!(check, Error::CheckViolation(_)));
        assert_eq!(status_code(&check), 422);
    }

    #[test]
    fn a_not_null_violation_reads_its_column_out_of_the_message() {
        let error = write_error::<User>(reported(
            "23502",
            "null value in column \"email\" of relation \"users\" violates not-null constraint",
        ));
        assert!(matches!(error, Error::NotNullViolation(_)));
        assert_eq!(error.field_pointer().as_deref(), Some("/email"));
        assert_eq!(status_code(&error), 422);
    }

    #[test]
    fn a_serialisation_failure_is_retryable_and_a_deadlock_is_too() {
        let lost = write_error::<User>(reported("40001", "could not serialize access"));
        assert!(matches!(lost, Error::Serialization { .. }));
        assert!(lost.is_retryable());
        assert_eq!(status_code(&lost), 409);

        let victim = write_error::<User>(reported("40P01", "deadlock detected"));
        assert!(matches!(victim, Error::Deadlock { .. }));
        assert!(victim.is_retryable());
    }

    #[test]
    fn sqlite_reports_its_constraints_in_words_and_is_understood() {
        let error = write_error::<User>(reported(
            "2067",
            "UNIQUE constraint failed: users.tenant_id, users.email",
        ));
        assert!(matches!(error, Error::UniqueViolation(_)));
        assert_eq!(error.field_pointer().as_deref(), Some("/tenant_id"));

        let generic =
            write_error::<User>(reported("19", "NOT NULL constraint failed: users.email"));
        assert!(matches!(generic, Error::NotNullViolation(_)));
        assert_eq!(generic.field_pointer().as_deref(), Some("/email"));
    }

    #[test]
    fn anything_that_is_not_a_constraint_failure_passes_through() {
        let error = write_error::<User>(reported("42703", "column \"nam\" does not exist"));
        assert!(matches!(error, Error::Database(_)));
        assert_eq!(status_code(&error), 500);
        assert!(
            write_error::<User>(Error::not_found("User"))
                .field_pointer()
                .is_none()
        );
    }

    #[test]
    fn classifying_twice_does_not_wrap_twice() {
        let once = write_error::<User>(reported(
            "23505",
            "duplicate key value violates unique constraint \"users_email_key\"",
        ));
        let twice = write_error::<User>(once);
        assert!(matches!(twice, Error::UniqueViolation(_)));
        assert_eq!(twice.field_pointer().as_deref(), Some("/email"));
    }

    #[test]
    fn an_eagerly_classified_violation_is_reseated_onto_the_entity() {
        // What an execution layer that classifies before the entity is known
        // would produce: the right kind, the wrong entity, no columns.
        let anonymous = ConstraintViolation::unique("", "users_email_key").with_sqlstate("23505");
        let error = write_error::<User>(Error::UniqueViolation(Box::new(anonymous)));
        let Error::UniqueViolation(violation) = &error else {
            panic!("expected a unique violation, got {error:?}");
        };
        assert_eq!(violation.entity(), "User");
        assert_eq!(error.field_pointer().as_deref(), Some("/email"));
    }

    #[test]
    fn returning_lists_the_columns_in_decode_order() {
        let Returning::Items(items) = returning_entity::<User>() else {
            panic!("the entity's columns are listed, never `*`");
        };
        assert_eq!(
            items.len(),
            User::COLUMNS.len(),
            "a positional decode needs every column, in `COLUMNS` order"
        );
    }
}

/// The write path against a real database — PostgreSQL and SQLite, no mocks.
///
/// A statement that was only ever compared against another Rust value proves
/// that the builder does what the builder does. These tests render the
/// statements this crate builds, send them to a server, and check what the
/// server did with them — including the error codes, which is the only way to
/// know that `23505` really becomes a 409 that points at `/name`.
///
/// PostgreSQL is skipped, loudly, when `DATABASE_URL` is unset. SQLite runs
/// everywhere: it is bundled.
#[cfg(test)]
mod live {
    use super::*;
    use crate::delete::Delete;
    use crate::descriptor::EntityDescriptor;
    use crate::entity::ColumnDef;
    use crate::insert::Insert;
    use crate::row::{DecodeError, Row};
    use crate::update::Update;
    use crate::{Column, NewEntity, TenantId};
    use moso_sql::{Dialect, Postgres, Sqlite, Statement, TableRef, Value, ValueKind};
    use std::sync::OnceLock;

    /// The table these tests write to. Distinct enough not to collide with
    /// another crate's fixtures in the shared test database.
    const TABLE: &str = "moso_orm_writes_test";

    /// The child table, which exists so that a foreign key can refuse a delete.
    const CHILD_TABLE: &str = "moso_orm_writes_test_child";

    /// A widget: soft-deletable, versioned, tenant-scoped, with a managed
    /// `updated_at` — every write-path feature in one entity.
    #[derive(Clone, Debug)]
    struct Widget {
        id: i64,
    }

    impl Entity for Widget {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static(TABLE);
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64)
                .primary_key()
                .with_default(),
            ColumnDef::new("name", ValueKind::Text).unique(),
            ColumnDef::new("quantity", ValueKind::I64),
            ColumnDef::new("version", ValueKind::I32),
            ColumnDef::new("tenant_id", ValueKind::I64),
            ColumnDef::new("created_at", ValueKind::Timestamp),
            ColumnDef::new("updated_at", ValueKind::Timestamp),
            ColumnDef::new("deleted_at", ValueKind::Timestamp),
        ];
        const NAME: &'static str = "Widget";

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| {
                EntityDescriptor::builder("Widget", Self::TABLE)
                    .timestamps("created_at", "updated_at")
                    .soft_delete("deleted_at")
                    .versioned("version")
                    .tenant("tenant_id")
                    .build()
            })
        }
    }

    impl Widget {
        const NAME_COLUMN: Column<Self, String> = Column::new("name");
        const QUANTITY: Column<Self, i64> = Column::new("quantity");
    }

    /// What has to be supplied to create a widget.
    struct NewWidget {
        name: String,
        quantity: i64,
    }

    impl NewEntity for NewWidget {
        const COLUMNS: &'static [&'static str] = &["name", "quantity"];

        fn into_row(self) -> Vec<moso_sql::Expr> {
            vec![
                moso_sql::Expr::value(self.name),
                moso_sql::Expr::value(self.quantity),
            ]
        }
    }

    fn widget(name: &str, quantity: i64) -> NewWidget {
        NewWidget {
            name: name.to_owned(),
            quantity,
        }
    }

    fn tenant() -> TenantId {
        TenantId::of(1_i64)
    }

    /// Renders a statement and runs it, binding every parameter positionally.
    ///
    /// A macro rather than a function so that the two backends' query types
    /// never have to be named.
    macro_rules! run {
        ($pool:expr, $dialect:expr, $statement:expr) => {{
            let statement: Statement = $statement;
            let sql = statement.build(&$dialect).expect("the statement renders");
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.text.clone()));
            for value in &sql.args {
                query = match value {
                    Value::Bool(bound) => query.bind(*bound),
                    Value::I32(bound) => query.bind(*bound),
                    Value::I64(bound) => query.bind(*bound),
                    Value::Text(bound) => query.bind(bound.clone()),
                    other => panic!("these tests bind bool, i32, i64 and text only: {other:?}"),
                };
            }
            query.execute($pool).await.map(|done| done.rows_affected())
        }};
    }

    /// The driver's error, in the shape the execution layer reports it: a
    /// SQLSTATE and a message, and nothing this crate had to invent.
    fn reported(error: &sqlx::Error) -> Error {
        let sqlx::Error::Database(inner) = error else {
            panic!("expected a database error, got {error:?}");
        };
        let code = inner
            .code()
            .map_or_else(String::new, |code| code.into_owned());
        Error::Database(Box::new(DatabaseError::new(code, inner.message())))
    }

    #[tokio::test]
    async fn the_write_path_against_a_real_postgres() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skipping `the_write_path_against_a_real_postgres`: DATABASE_URL is not set, so \
                 there is no PostgreSQL to write to. Start one with `scripts/test-db.sh`."
            );
            return;
        };

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("DATABASE_URL is set, so it should be reachable");

        for statement in [
            format!("drop table if exists {CHILD_TABLE}"),
            format!("drop table if exists {TABLE}"),
            format!(
                "create table {TABLE} (
                     id          bigserial primary key,
                     name        text        not null unique,
                     quantity    bigint      not null default 0 check (quantity >= 0),
                     version     integer     not null default 1,
                     tenant_id   bigint      not null default 1,
                     created_at  timestamptz not null default current_timestamp,
                     updated_at  timestamptz not null default current_timestamp,
                     deleted_at  timestamptz
                 )"
            ),
            format!(
                "create table {CHILD_TABLE} (
                     id        bigserial primary key,
                     widget_id bigint not null references {TABLE} (id)
                 )"
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .execute(&pool)
                .await
                .expect("the fixture schema is created");
        }

        // ── insert: many rows, one statement ────────────────────────────────
        let bulk = Insert::<Widget>::rows([widget("bolt", 3), widget("nut", 5), widget("cog", 0)])
            .scoped(tenant());
        assert_eq!(
            bulk.statements(Postgres.max_bind_params())
                .expect("a valid insert")
                .len(),
            1,
            "N3-adjacent: a bulk insert is one statement, not one per row"
        );
        let written = run!(&pool, Postgres, bulk.to_statement().expect("renders"))
            .expect("three widgets are written");
        assert_eq!(written, 3);

        // ── 23505: the unique violation, with a field pointer ───────────────
        let duplicate = Insert::<Widget>::row(widget("bolt", 1))
            .scoped(tenant())
            .to_statement()
            .expect("renders");
        let failure = run!(&pool, Postgres, duplicate).expect_err("`bolt` is taken");
        let error = write_error::<Widget>(reported(&failure));
        assert_eq!(error.sqlstate(), Some("23505"), "{error}");
        assert!(matches!(error, Error::UniqueViolation(_)), "{error}");
        assert_eq!(
            error.field_pointer().as_deref(),
            Some("/name"),
            "derived from the constraint PostgreSQL itself named: {error}"
        );
        assert_eq!(status_code(&error), 409);
        assert!(error.to_string().contains("Widget"), "{error}");

        // ── the upsert: do nothing, then do update ──────────────────────────
        let ignored = Insert::<Widget>::row(widget("bolt", 99))
            .scoped(tenant())
            .on_conflict(Widget::NAME_COLUMN)
            .do_nothing()
            .to_statement()
            .expect("renders");
        assert_eq!(
            run!(&pool, Postgres, ignored).expect("an idempotent insert is not an error"),
            0
        );

        let upserted = Insert::<Widget>::row(widget("bolt", 42))
            .scoped(tenant())
            .on_conflict(Widget::NAME_COLUMN)
            .do_update([moso_sql::Ident::from_static("quantity")])
            .to_statement()
            .expect("renders");
        assert_eq!(run!(&pool, Postgres, upserted).expect("the upsert runs"), 1);
        assert_eq!(quantity_pg(&pool, "bolt").await, 42);

        // ── update by key, and the atomic increment ─────────────────────────
        let bolt = id_pg(&pool, "bolt").await;
        let bumped = Update::<Widget>::by_key(bolt)
            .scoped(tenant())
            .set_with(Widget::QUANTITY, |current| {
                current + moso_sql::Expr::value(8_i64)
            })
            .to_statement()
            .expect("renders");
        assert_eq!(run!(&pool, Postgres, bumped).expect("the bump runs"), 1);
        assert_eq!(
            quantity_pg(&pool, "bolt").await,
            50,
            "`quantity = quantity + 8` is one statement and cannot lose a race"
        );

        // ── optimistic locking ──────────────────────────────────────────────
        let version = version_pg(&pool, "bolt").await;
        let stale = Update::<Widget>::by_key(bolt)
            .scoped(tenant())
            .expecting_version(version - 1)
            .set(Widget::QUANTITY, 0_i64)
            .to_statement()
            .expect("renders");
        assert_eq!(
            run!(&pool, Postgres, stale).expect("a stale write is not a server error"),
            0,
            "the version moved, so the row no longer matches"
        );
        let fresh = Update::<Widget>::by_key(bolt)
            .scoped(tenant())
            .expecting_version(version)
            .set(Widget::QUANTITY, 7_i64)
            .to_statement()
            .expect("renders");
        assert_eq!(run!(&pool, Postgres, fresh).expect("the write runs"), 1);
        assert_eq!(
            version_pg(&pool, "bolt").await,
            version + 1,
            "every write moves the version, or nobody can detect staleness"
        );

        // ── 23514: the check violation ──────────────────────────────────────
        let negative = Update::<Widget>::by_key(bolt)
            .scoped(tenant())
            .set(Widget::QUANTITY, -1_i64)
            .to_statement()
            .expect("renders");
        let failure = run!(&pool, Postgres, negative).expect_err("quantity >= 0");
        let error = write_error::<Widget>(reported(&failure));
        assert_eq!(error.sqlstate(), Some("23514"), "{error}");
        assert!(matches!(error, Error::CheckViolation(_)), "{error}");
        assert_eq!(status_code(&error), 422);
        assert_eq!(error.field_pointer().as_deref(), Some("/quantity"));

        // ── the unfiltered-write guard, on a live connection ────────────────
        let refused = Update::<Widget>::all()
            .scoped(tenant())
            .set(Widget::QUANTITY, 0_i64)
            .to_statement()
            .expect_err("no filter, no `all_rows`");
        assert!(matches!(refused, Error::UnfilteredWrite { .. }));
        let deliberate = Update::<Widget>::all()
            .all_rows()
            .scoped(tenant())
            .set(Widget::QUANTITY, 1_i64)
            .to_statement()
            .expect("renders");
        assert_eq!(
            run!(&pool, Postgres, deliberate).expect("the mass update runs"),
            3,
            "`.all_rows()` really does mean every row"
        );

        // ── the soft delete, and the hard one ───────────────────────────────
        let soft = Delete::<Widget>::by_key(bolt)
            .scoped(tenant())
            .to_statement()
            .expect("renders");
        assert_eq!(run!(&pool, Postgres, soft).expect("the delete runs"), 1);
        assert_eq!(
            count_pg(&pool, "select count(*) from ").await,
            3,
            "a soft delete keeps the row"
        );
        assert!(deleted_at_pg(&pool, "bolt").await, "…and stamps it");

        let again = Delete::<Widget>::by_key(bolt)
            .scoped(tenant())
            .to_statement()
            .expect("renders");
        assert_eq!(
            run!(&pool, Postgres, again).expect("the delete runs"),
            0,
            "deleting a deleted row writes nothing, so the count stays honest"
        );

        // ── 23503: the foreign key refuses a hard delete ────────────────────
        let nut = id_pg(&pool, "nut").await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "insert into {CHILD_TABLE} (widget_id) values ($1)"
        )))
        .bind(nut)
        .execute(&pool)
        .await
        .expect("a child row");

        let referenced = Delete::<Widget>::by_key(nut)
            .scoped(tenant())
            .hard()
            .to_statement()
            .expect("renders");
        let failure = run!(&pool, Postgres, referenced).expect_err("a child still points at it");
        let error = write_error::<Widget>(reported(&failure));
        assert_eq!(error.sqlstate(), Some("23503"), "{error}");
        assert!(matches!(error, Error::ForeignKeyViolation(_)), "{error}");
        assert_eq!(status_code(&error), 409);

        // …and the hard delete of an unreferenced row removes it, including
        // one that was already soft-deleted.
        let purged = Delete::<Widget>::by_key(bolt)
            .scoped(tenant())
            .hard()
            .to_statement()
            .expect("renders");
        assert_eq!(
            run!(&pool, Postgres, purged).expect("the purge runs"),
            1,
            "`.hard()` must be able to reach an already soft-deleted row"
        );
        assert_eq!(count_pg(&pool, "select count(*) from ").await, 2);

        // ── the bulk-delete guard ───────────────────────────────────────────
        let refused = Delete::<Widget>::all()
            .scoped(tenant())
            .to_statement()
            .expect_err("no filter, no `all_rows`");
        assert!(matches!(refused, Error::UnfilteredWrite { .. }));

        // ── 40001: a serialisation failure is reported as retryable ─────────
        assert_serialisation_failure_is_retryable(&pool).await;

        for statement in [
            format!("drop table if exists {CHILD_TABLE}"),
            format!("drop table if exists {TABLE}"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .execute(&pool)
                .await
                .expect("the fixture is cleaned up");
        }
        pool.close().await;
    }

    /// Two `REPEATABLE READ` transactions writing the same row: the second gets
    /// `40001`, which [`write_error`] must report as retryable rather than as a
    /// 500.
    async fn assert_serialisation_failure_is_retryable(pool: &sqlx::PgPool) {
        let target = id_pg(pool, "cog").await;

        let mut first = pool.begin().await.expect("a transaction");
        sqlx::query("set transaction isolation level repeatable read")
            .execute(&mut *first)
            .await
            .expect("the isolation level is set");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "update {TABLE} set quantity = 11 where id = $1"
        )))
        .bind(target)
        .execute(&mut *first)
        .await
        .expect("the first write");

        let contender = pool.clone();
        let racing = tokio::spawn(async move {
            let mut second = contender.begin().await.expect("a transaction");
            sqlx::query("set transaction isolation level repeatable read")
                .execute(&mut *second)
                .await
                .expect("the isolation level is set");
            // Blocks on the first transaction's row lock until it commits.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "update {TABLE} set quantity = 22 where id = $1"
            )))
            .bind(target)
            .execute(&mut *second)
            .await
        });

        // Give the second transaction time to reach the lock before releasing
        // it; the assertion below does not depend on this being exact.
        tokio::time::sleep(core::time::Duration::from_millis(250)).await;
        first.commit().await.expect("the first transaction commits");

        let outcome = racing.await.expect("the racing task finishes");
        let failure = outcome.expect_err("the second write cannot be serialised");
        let error = write_error::<Widget>(reported(&failure));
        assert!(
            matches!(error, Error::Serialization { .. }),
            "40001 is a lost race, not a bug: {error}"
        );
        assert!(
            error.is_retryable(),
            "`Db::transaction` retries exactly this: {error}"
        );
        assert_eq!(status_code(&error), 409);
    }

    async fn id_pg(pool: &sqlx::PgPool, name: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select id from {TABLE} where name = $1"
        )))
        .bind(name.to_owned())
        .fetch_one(pool)
        .await
        .expect("the row exists")
    }

    async fn quantity_pg(pool: &sqlx::PgPool, name: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select quantity from {TABLE} where name = $1"
        )))
        .bind(name.to_owned())
        .fetch_one(pool)
        .await
        .expect("the row exists")
    }

    async fn version_pg(pool: &sqlx::PgPool, name: &str) -> i32 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select version from {TABLE} where name = $1"
        )))
        .bind(name.to_owned())
        .fetch_one(pool)
        .await
        .expect("the row exists")
    }

    async fn deleted_at_pg(pool: &sqlx::PgPool, name: &str) -> bool {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select deleted_at is not null from {TABLE} where name = $1"
        )))
        .bind(name.to_owned())
        .fetch_one(pool)
        .await
        .expect("the row exists")
    }

    async fn count_pg(pool: &sqlx::PgPool, prefix: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("{prefix}{TABLE}")))
            .fetch_one(pool)
            .await
            .expect("the table exists")
    }

    #[tokio::test]
    async fn the_write_path_against_a_real_sqlite() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite is bundled, so this cannot fail for want of a server");

        sqlx::query(sqlx::AssertSqlSafe(format!(
            "create table {TABLE} (
                 id         integer primary key autoincrement,
                 name       text    not null unique,
                 quantity   integer not null default 0
                            constraint {TABLE}_quantity_check check (quantity >= 0),
                 version    integer not null default 1,
                 tenant_id  integer not null default 1,
                 created_at text    not null default current_timestamp,
                 updated_at text    not null default current_timestamp,
                 deleted_at text
             )"
        )))
        .execute(&pool)
        .await
        .expect("the fixture schema is created");

        let bulk = Insert::<Widget>::rows([widget("bolt", 3), widget("nut", 5)]).scoped(tenant());
        assert_eq!(
            run!(&pool, Sqlite, bulk.to_statement().expect("renders")).expect("two widgets"),
            2
        );

        // The same unique violation, reported in SQLite's own words.
        let duplicate = Insert::<Widget>::row(widget("bolt", 1))
            .scoped(tenant())
            .to_statement()
            .expect("renders");
        let failure = run!(&pool, Sqlite, duplicate).expect_err("`bolt` is taken");
        let error = write_error::<Widget>(reported(&failure));
        assert!(matches!(error, Error::UniqueViolation(_)), "{error}");
        assert_eq!(error.field_pointer().as_deref(), Some("/name"), "{error}");
        assert_eq!(status_code(&error), 409);

        let bolt: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select id from {TABLE} where name = ?"
        )))
        .bind("bolt")
        .fetch_one(&pool)
        .await
        .expect("the row exists");

        // The atomic increment, on the second dialect.
        let bumped = Update::<Widget>::by_key(bolt)
            .scoped(tenant())
            .set_with(Widget::QUANTITY, |current| {
                current + moso_sql::Expr::value(4_i64)
            })
            .to_statement()
            .expect("renders");
        assert_eq!(run!(&pool, Sqlite, bumped).expect("the bump runs"), 1);
        let quantity: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select quantity from {TABLE} where id = ?"
        )))
        .bind(bolt)
        .fetch_one(&pool)
        .await
        .expect("the row exists");
        assert_eq!(quantity, 7);

        // The check violation, whose pointer comes from the constraint name.
        let negative = Update::<Widget>::by_key(bolt)
            .scoped(tenant())
            .set(Widget::QUANTITY, -1_i64)
            .to_statement()
            .expect("renders");
        let failure = run!(&pool, Sqlite, negative).expect_err("quantity >= 0");
        let error = write_error::<Widget>(reported(&failure));
        assert!(matches!(error, Error::CheckViolation(_)), "{error}");
        assert_eq!(status_code(&error), 422);
        assert_eq!(
            error.field_pointer().as_deref(),
            Some("/quantity"),
            "{error}"
        );

        // The soft delete keeps the row and stamps it.
        let soft = Delete::<Widget>::by_key(bolt)
            .scoped(tenant())
            .to_statement()
            .expect("renders");
        assert_eq!(run!(&pool, Sqlite, soft).expect("the delete runs"), 1);
        let stamped: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select deleted_at is not null from {TABLE} where id = ?"
        )))
        .bind(bolt)
        .fetch_one(&pool)
        .await
        .expect("the row is still there");
        assert!(stamped);

        // …and the hard one removes it.
        let purged = Delete::<Widget>::by_key(bolt)
            .scoped(tenant())
            .hard()
            .to_statement()
            .expect("renders");
        assert_eq!(run!(&pool, Sqlite, purged).expect("the purge runs"), 1);
        let remaining: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from {TABLE}")))
                .fetch_one(&pool)
                .await
                .expect("the table exists");
        assert_eq!(remaining, 1);

        pool.close().await;
    }
}
