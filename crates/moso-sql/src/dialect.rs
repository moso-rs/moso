//! The [`Dialect`] trait and the two backends this build supports.
//!
//! ADR-0010 makes PostgreSQL the reference dialect and SQLite a fully
//! supported second. MySQL is not in this build. The trait is public so a third
//! party can add a backend, with the caveat from `docs/02-data/20`: it is not
//! stable before 1.0.

use core::fmt;

use crate::error::Error;
use crate::ident::Ident;
use crate::sql::Sql;
use crate::statement::StatementRef;
use crate::types::DataType;

/// Renders a statement as SQL for one database.
///
/// # Implementing one
///
/// [`Dialect::build`] is the whole job; everything else describes the target so
/// that the layers above can ask before they generate. Two rules make the
/// difference between a dialect people trust and one they work around:
///
/// 1. **Never emit an identifier unquoted**, and never build one from anything
///    but an [`Ident`]. That is what makes injection structurally impossible
///    rather than merely unlikely.
/// 2. **Never silently drop a clause.** If the database has no `ILIKE`, either
///    lower both sides — and say so in the documentation — or return
///    [`Error::Unsupported`]. A dropped `FOR UPDATE` is a data-loss bug that
///    only shows up under load.
///
/// ```
/// use moso_sql::{Dialect, Ident, Postgres, Sqlite};
///
/// assert_eq!(Postgres.quoted(&Ident::from_static("select")), r#""select""#);
/// assert!(Postgres.supports_returning());
/// assert!(!Sqlite.capabilities().distinct_on);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a Moso SQL dialect",
    label = "not a dialect",
    note = "a dialect must implement `name`, `quote_ident`, `placeholder`, `type_name`, \
            `capabilities` and `build`",
    note = "the built-in dialects are `moso_sql::Postgres` and `moso_sql::Sqlite`",
    note = "help: pass `&Postgres` or `&Sqlite`, or implement `Dialect for {Self}` if you are \
            adding a backend"
)]
pub trait Dialect: fmt::Debug + Send + Sync {
    /// The dialect's name, as error messages and log fields spell it.
    ///
    /// Used in [`Error::Unsupported`], so it should read as a product name:
    /// `"PostgreSQL"`, not `"pg"`.
    fn name(&self) -> &'static str;

    /// Appends `ident`, quoted, to `out`.
    ///
    /// Implementations must quote unconditionally. [`Ident`] already forbids
    /// the quote character, so no escaping is needed — but an implementation
    /// that quotes only "when necessary" will one day meet a column named
    /// `order` and produce a syntax error in production.
    fn quote_ident(&self, ident: &Ident, out: &mut String);

    /// Appends the placeholder for the parameter at `index` (zero-based) to
    /// `out`.
    ///
    /// PostgreSQL numbers its placeholders and SQLite does not, which is the
    /// entire reason this is a dialect method rather than a constant.
    fn placeholder(&self, index: usize, out: &mut String);

    /// Appends the dialect's spelling of `data_type` to `out`.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when the database has no such type and there is
    /// no honest substitute.
    fn type_name(&self, data_type: &DataType, out: &mut String) -> Result<(), Error>;

    /// What the dialect can do.
    fn capabilities(&self) -> Capabilities;

    /// Renders a statement.
    ///
    /// # DML binds, DDL does not
    ///
    /// A `SELECT`, `INSERT`, `UPDATE` or `DELETE` puts every [`Value`](crate::Value) in
    /// [`Sql::args`] and a placeholder in the text. A [`Ddl`](crate::ddl::Ddl)
    /// statement writes its values into the text as literals and leaves
    /// [`Sql::args`] empty, because the catalogue stores a `DEFAULT`, a `CHECK`
    /// and a partial index's predicate as parsed text and there is no parameter
    /// to bind them to.
    ///
    /// # A DDL result may be several statements
    ///
    /// Some schema changes are one statement on PostgreSQL and several on
    /// SQLite — an `ALTER TABLE` with two actions, a `DROP TABLE` naming two
    /// tables — and a `CREATE TABLE` that carries comments is a `CREATE TABLE`
    /// followed by its `COMMENT ON`s. Those results are separated by `;\n`.
    /// Since DDL binds nothing, the whole string can go over the simple query
    /// protocol.
    ///
    /// # Errors
    ///
    /// [`Error`] if the statement is incomplete, binds too many parameters, or
    /// uses a construct this dialect does not have.
    ///
    /// ```
    /// use moso_sql::{Dialect, Expr, Ident, Postgres, Select, Sqlite, TableRef};
    ///
    /// let query = Select::from_table(TableRef::from_static("users"))
    ///     .select_all()
    ///     .filter(Expr::col(Ident::from_static("email")).eq(Expr::value("ada@example.com")))
    ///     .into_statement();
    ///
    /// let postgres = Postgres.build(query.borrowed())?;
    /// assert_eq!(postgres.text, r#"SELECT * FROM "users" WHERE "email" = $1"#);
    ///
    /// let sqlite = Sqlite.build(query.borrowed())?;
    /// assert_eq!(sqlite.text, r#"SELECT * FROM "users" WHERE "email" = ?"#);
    ///
    /// // The address is a parameter on both, never text.
    /// assert_eq!(postgres.args, sqlite.args);
    /// assert!(!postgres.text.contains("ada@example.com"));
    /// # Ok::<(), moso_sql::Error>(())
    /// ```
    fn build(&self, statement: StatementRef<'_>) -> Result<Sql, Error>;

    /// How many parameters one statement may bind.
    ///
    /// A batched insert must chunk its rows to stay under this. The default is
    /// "no limit", which is wrong for every real database, so a dialect should
    /// override it.
    fn max_bind_params(&self) -> usize {
        usize::MAX
    }

    /// The longest identifier the server accepts, in bytes.
    ///
    /// Defaults to [`Ident::MAX_LEN`], which is PostgreSQL's.
    fn max_ident_len(&self) -> usize {
        Ident::MAX_LEN
    }

    /// `ident`, quoted, as a fresh `String`.
    ///
    /// The allocating convenience over [`Dialect::quote_ident`], for error
    /// messages and generated migration files.
    ///
    /// ```
    /// use moso_sql::{Dialect, Ident, Postgres};
    ///
    /// assert_eq!(Postgres.quoted(&Ident::from_static("users")), r#""users""#);
    /// ```
    fn quoted(&self, ident: &Ident) -> String {
        let mut out = String::with_capacity(ident.byte_len() + 2);
        self.quote_ident(ident, &mut out);
        out
    }

    /// Whether `INSERT`/`UPDATE`/`DELETE … RETURNING` works.
    ///
    /// ```
    /// use moso_sql::{Dialect, Postgres};
    ///
    /// assert!(Postgres.supports_returning());
    /// ```
    fn supports_returning(&self) -> bool {
        self.capabilities().returning
    }

    /// Whether window functions work.
    ///
    /// ```
    /// use moso_sql::{Dialect, Sqlite};
    ///
    /// assert!(Sqlite.supports_window_functions());
    /// ```
    fn supports_window_functions(&self) -> bool {
        self.capabilities().window_functions
    }

    /// Whether `LATERAL` subqueries work.
    ///
    /// ```
    /// use moso_sql::{Dialect, Sqlite};
    ///
    /// assert!(!Sqlite.supports_lateral());
    /// ```
    fn supports_lateral(&self) -> bool {
        self.capabilities().lateral_joins
    }

    /// Whether `FOR UPDATE … SKIP LOCKED` works — the job-queue primitive.
    ///
    /// ```
    /// use moso_sql::{Dialect, Postgres, Sqlite};
    ///
    /// assert!(Postgres.supports_skip_locked());
    /// assert!(!Sqlite.supports_skip_locked());
    /// ```
    fn supports_skip_locked(&self) -> bool {
        self.capabilities().skip_locked
    }
}

/// What a dialect can do.
///
/// Every field answers a question the layers above ask before generating.
/// Fields are public so a third-party dialect can start from
/// [`Capabilities::default`] — everything off — and turn on what it has;
/// the struct is `#[non_exhaustive]` so a new capability is not a breaking
/// change for implementors.
///
/// ```
/// use moso_sql::Capabilities;
///
/// let mut capabilities = Capabilities::default();
/// assert!(!capabilities.returning);
/// capabilities.returning = true;
/// assert!(capabilities.returning);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capabilities {
    /// `RETURNING` on a write.
    pub returning: bool,
    /// `INSERT … ON CONFLICT DO UPDATE`.
    pub on_conflict_do_update: bool,
    /// `RETURNING` on a statement that also has `ON CONFLICT`.
    pub returning_with_on_conflict: bool,
    /// Common table expressions.
    pub ctes: bool,
    /// `WITH RECURSIVE`.
    pub recursive_ctes: bool,
    /// `MATERIALIZED` / `NOT MATERIALIZED` on a CTE.
    pub materialized_ctes: bool,
    /// Data-modifying statements inside a CTE.
    pub data_modifying_ctes: bool,
    /// Window functions.
    pub window_functions: bool,
    /// `GROUPS` frame units and `EXCLUDE` on a window frame.
    pub advanced_window_frames: bool,
    /// `LATERAL` subqueries.
    pub lateral_joins: bool,
    /// `RIGHT OUTER JOIN`.
    pub right_join: bool,
    /// `FULL OUTER JOIN`.
    pub full_join: bool,
    /// `ILIKE`.
    pub ilike: bool,
    /// `DISTINCT ON (…)`.
    pub distinct_on: bool,
    /// `NULLS FIRST` / `NULLS LAST` in an `ORDER BY`.
    pub nulls_ordering: bool,
    /// `FOR UPDATE` and friends.
    pub row_locks: bool,
    /// `SKIP LOCKED`.
    pub skip_locked: bool,
    /// `NOWAIT`.
    pub nowait: bool,
    /// Array types and the array operators.
    pub arrays: bool,
    /// `jsonb` and its operators.
    pub jsonb: bool,
    /// `tsvector` full-text search.
    pub full_text_search: bool,
    /// `FILTER (WHERE …)` on an aggregate.
    pub aggregate_filter: bool,
    /// `IS DISTINCT FROM`.
    pub is_distinct_from: bool,
    /// `CREATE INDEX CONCURRENTLY`.
    pub concurrent_indexes: bool,
    /// Partial indexes — `CREATE INDEX … WHERE …`.
    pub partial_indexes: bool,
    /// An index method other than the default, such as `GIN`.
    pub index_methods: bool,
    /// `ALTER TABLE … DROP COLUMN`.
    pub drop_column: bool,
    /// `ALTER TABLE … ALTER COLUMN … TYPE …`.
    pub alter_column_type: bool,
    /// `ALTER TABLE … RENAME COLUMN`.
    pub rename_column: bool,
    /// User-defined enum types.
    pub enum_types: bool,
    /// Named schemas.
    pub schemas: bool,
    /// `NOT VALID` constraints and `VALIDATE CONSTRAINT`.
    pub deferred_constraint_validation: bool,
    /// Declarative partitioning.
    pub partitioning: bool,
}

impl Capabilities {
    /// PostgreSQL 14 and later: everything in this table.
    ///
    /// ```
    /// use moso_sql::Capabilities;
    ///
    /// assert!(Capabilities::postgres().skip_locked);
    /// ```
    #[must_use]
    pub const fn postgres() -> Self {
        Self {
            returning: true,
            on_conflict_do_update: true,
            returning_with_on_conflict: true,
            ctes: true,
            recursive_ctes: true,
            materialized_ctes: true,
            data_modifying_ctes: true,
            window_functions: true,
            advanced_window_frames: true,
            lateral_joins: true,
            right_join: true,
            full_join: true,
            ilike: true,
            distinct_on: true,
            nulls_ordering: true,
            row_locks: true,
            skip_locked: true,
            nowait: true,
            arrays: true,
            jsonb: true,
            full_text_search: true,
            aggregate_filter: true,
            is_distinct_from: true,
            concurrent_indexes: true,
            partial_indexes: true,
            index_methods: true,
            drop_column: true,
            alter_column_type: true,
            rename_column: true,
            enum_types: true,
            schemas: true,
            deferred_constraint_validation: true,
            partitioning: true,
        }
    }

    /// SQLite 3.40 and later.
    ///
    /// The gaps are real and documented rather than papered over: no `ILIKE`,
    /// no `DISTINCT ON`, no row locks, no arrays, no `jsonb` operators, no
    /// enum types, no schemas, and `ALTER TABLE` that cannot change a column's
    /// type — which is why the migration generator emits a table rebuild
    /// instead.
    ///
    /// ```
    /// use moso_sql::Capabilities;
    ///
    /// let sqlite = Capabilities::sqlite();
    /// assert!(sqlite.window_functions);
    /// assert!(!sqlite.arrays);
    /// ```
    #[must_use]
    pub const fn sqlite() -> Self {
        Self {
            returning: true,
            on_conflict_do_update: true,
            returning_with_on_conflict: true,
            ctes: true,
            recursive_ctes: true,
            materialized_ctes: true,
            data_modifying_ctes: false,
            window_functions: true,
            advanced_window_frames: true,
            lateral_joins: false,
            right_join: true,
            full_join: true,
            ilike: false,
            distinct_on: false,
            nulls_ordering: true,
            row_locks: false,
            skip_locked: false,
            nowait: false,
            arrays: false,
            jsonb: false,
            full_text_search: false,
            aggregate_filter: true,
            is_distinct_from: true,
            concurrent_indexes: false,
            partial_indexes: true,
            index_methods: false,
            drop_column: true,
            alter_column_type: false,
            rename_column: true,
            enum_types: false,
            schemas: false,
            deferred_constraint_validation: false,
            partitioning: false,
        }
    }
}

/// The PostgreSQL dialect — the reference implementation (ADR-0010).
///
/// Every construct in this crate renders here; [`Sqlite`] is the one that has
/// to say no, and its documentation lists where. The two deliberate departures
/// from "whatever the server would print":
///
/// * **`LIMIT` and `OFFSET` are literals, not parameters.** A page size is a
///   `u64` this crate produced, so it cannot inject, and a literal lets the
///   planner see the row count — which for a `LIMIT` is the difference between
///   an index scan and a sort. It also keeps two of the 65 535 bind slots free
///   for the `WHERE` clause.
/// * **`RENAME`, `SET SCHEMA` and `ATTACH`/`DETACH PARTITION` are cut out of an
///   `ALTER TABLE` action list into their own statements**, because they are
///   separate statement forms in PostgreSQL's grammar and mixing them is a
///   syntax error. Every maximal run of list-able actions still shares a
///   statement, so the lock is taken as few times as the grammar allows.
///
/// ```
/// use moso_sql::{Dialect, Ident, Postgres};
///
/// let mut text = String::new();
/// Postgres.placeholder(0, &mut text);
/// assert_eq!(text, "$1");
/// assert_eq!(Postgres.quoted(&Ident::from_static("user")), r#""user""#);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Postgres;

impl Postgres {
    /// The name this dialect reports, also available through
    /// [`Dialect::name`].
    ///
    /// ```
    /// assert_eq!(moso_sql::Postgres::NAME, "PostgreSQL");
    /// ```
    pub const NAME: &'static str = "PostgreSQL";

    /// The extended-protocol limit on bound parameters: one `int16` counts
    /// them.
    ///
    /// ```
    /// assert_eq!(moso_sql::Postgres::MAX_BIND_PARAMS, 65_535);
    /// ```
    pub const MAX_BIND_PARAMS: usize = 65_535;
}

impl Dialect for Postgres {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn quote_ident(&self, ident: &Ident, out: &mut String) {
        out.push('"');
        out.push_str(ident.as_str());
        out.push('"');
    }

    fn placeholder(&self, index: usize, out: &mut String) {
        out.push('$');
        // Placeholders are one-based on the wire.
        out.push_str(&(index + 1).to_string());
    }

    fn type_name(&self, data_type: &DataType, out: &mut String) -> Result<(), Error> {
        match data_type {
            DataType::Boolean => out.push_str("boolean"),
            DataType::SmallInt => out.push_str("smallint"),
            DataType::Integer => out.push_str("integer"),
            DataType::BigInt => out.push_str("bigint"),
            DataType::SmallSerial => out.push_str("smallserial"),
            DataType::Serial => out.push_str("serial"),
            DataType::BigSerial => out.push_str("bigserial"),
            DataType::Real => out.push_str("real"),
            DataType::DoublePrecision => out.push_str("double precision"),
            DataType::Numeric { precision, scale } => match (precision, scale) {
                (Some(precision), Some(scale)) => {
                    out.push_str(&format!("numeric({precision}, {scale})"));
                }
                (Some(precision), None) => out.push_str(&format!("numeric({precision})")),
                _ => out.push_str("numeric"),
            },
            DataType::Text => out.push_str("text"),
            DataType::VarChar(Some(length)) => out.push_str(&format!("varchar({length})")),
            DataType::VarChar(None) => out.push_str("varchar"),
            DataType::Char(Some(length)) => out.push_str(&format!("char({length})")),
            DataType::Char(None) => out.push_str("char"),
            DataType::Bytea => out.push_str("bytea"),
            DataType::Uuid => out.push_str("uuid"),
            DataType::Json => out.push_str("json"),
            DataType::JsonB => out.push_str("jsonb"),
            DataType::Date => out.push_str("date"),
            DataType::Time { with_time_zone } => {
                out.push_str(if *with_time_zone { "timetz" } else { "time" });
            }
            DataType::Timestamp { with_time_zone } => {
                out.push_str(if *with_time_zone {
                    "timestamptz"
                } else {
                    "timestamp"
                });
            }
            DataType::Interval => out.push_str("interval"),
            DataType::Inet => out.push_str("inet"),
            DataType::Cidr => out.push_str("cidr"),
            DataType::MacAddr => out.push_str("macaddr"),
            DataType::TsVector => out.push_str("tsvector"),
            DataType::TsQuery => out.push_str("tsquery"),
            DataType::Array(element) => {
                self.type_name(element, out)?;
                out.push_str("[]");
            }
            DataType::Enum(name) | DataType::Custom(name) => {
                if let Some(schema) = name.schema() {
                    self.quote_ident(schema, out);
                    out.push('.');
                }
                self.quote_ident(name.name(), out);
            }
        }
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::postgres()
    }

    fn build(&self, statement: StatementRef<'_>) -> Result<Sql, Error> {
        crate::render::build(self, statement)
    }

    fn max_bind_params(&self) -> usize {
        Self::MAX_BIND_PARAMS
    }
}

/// The SQLite dialect — fully supported, with the divergences documented on
/// [`Capabilities::sqlite`].
///
/// # Every place the SQL differs, and why
///
/// ADR-0010 promises SQLite "full support … every feature works or has a
/// documented, tested equivalent". This is that list. Everything not on it
/// renders identically on both backends, modulo the placeholder spelling.
///
/// | Construct | SQLite |
/// | --- | --- |
/// | `ILIKE` | `lower(a) LIKE lower(b)`. The fold is ASCII-only, like `ILIKE`'s under a non-`C` collation on ASCII text and weaker on everything else. |
/// | `->` / `->>` on JSON | The same two operators. SQLite's are `json_extract` with PostgreSQL's key-or-index abbreviation. Every other `jsonb` operator is [`Error::Unsupported`]. |
/// | `now()` | `CURRENT_TIMESTAMP`. |
/// | `greatest` / `least` | `max` / `min`. A one-argument call renders as the argument, because SQLite's one-argument `max` is the *aggregate*. |
/// | `trim(LEADING c FROM s)` | `ltrim(s, c)`; `TRAILING` is `rtrim`, `BOTH` is `trim`. |
/// | `substring(s FROM a FOR b)` | `substr(s, a, b)`. An absent start becomes the standard's implied `1`, because `substr` has no "from the beginning" form. |
/// | `string_agg` | `group_concat`. |
/// | `json_agg` / `jsonb_agg` | `json_group_array`. |
/// | `json_object_agg` / `jsonb_object_agg` | `json_group_object`. |
/// | `OFFSET` with no `LIMIT` | `LIMIT -1 OFFSET n`. SQLite's grammar has no bare `OFFSET`. |
/// | `TRUNCATE t` | `DELETE FROM t`, which SQLite optimises into the same thing — and which already restarts a rowid counter, because SQLite derives the next rowid from `max(rowid)`. `RESTART IDENTITY` itself is [`Error::Unsupported`]: it would need `DELETE FROM sqlite_sequence`, and that catalogue table does not exist until the database has held an `AUTOINCREMENT` column. |
/// | `ALTER TABLE` with several actions | One statement per action, separated by `;\n`. SQLite takes one action per statement. |
/// | `DROP TABLE a, b` | One statement per table. |
/// | `CREATE INDEX i ON s.t (…)` | `CREATE INDEX s.i ON t (…)`. SQLite qualifies the *index* with the attached database. |
/// | `COMMENT ON` attached to a `CREATE TABLE` | Dropped. SQLite has no comment catalogue, and a comment carries no semantics — the one place in this crate where a clause disappears rather than erroring. A standalone [`CommentOn`](crate::ddl::CommentOn) is still [`Error::Unsupported`]. |
///
/// Everything else SQLite lacks — row locks, `DISTINCT ON`, `LATERAL`, arrays,
/// `ANY`/`ALL`, regular expressions, `^`, bitwise `#`, full-text search, enum
/// types, schemas, `DELETE … USING`, concurrent indexes, covering indexes,
/// operator classes, `NULLS NOT DISTINCT`, exclusion constraints, partitioning
/// and most of `ALTER TABLE` — is [`Error::Unsupported`] with a concrete
/// alternative, never silently different SQL.
///
/// # The refusals that are not obvious from the capability table
///
/// Four of them are server errors that arrive at *migration* time rather than
/// at parse time, so they are worth naming: `ALTER TABLE … ADD COLUMN` cannot
/// add a `UNIQUE` column, a `PRIMARY KEY` column, a `NOT NULL` column without a
/// constant default, a column whose `DEFAULT` is an expression, or a `STORED`
/// generated column. SQLite appends a column without rewriting the rows, so
/// anything that would have to be checked against or filled in for the existing
/// rows is refused. All five come back as [`Error::Unsupported`] pointing at
/// the table rebuild in `docs/02-data/23-migrations.md`. A fifth: `NULLS FIRST`
/// / `NULLS LAST` parses in an `ORDER BY` and answers
/// `unsupported use of NULLS LAST` in a `CREATE INDEX`, so it is refused there.
///
/// # Server version
///
/// The floor is SQLite 3.40 for most of the above. Three renderings need
/// 3.44: `concat`, `concat_ws` and an `ORDER BY` inside `group_concat`. Moso's
/// own test matrix bundles a much newer library through `sqlx`'s
/// `sqlite-bundled` feature.
///
/// ```
/// use moso_sql::{Dialect, Ident, Sqlite};
///
/// let mut text = String::new();
/// Sqlite.placeholder(0, &mut text);
/// assert_eq!(text, "?");
/// assert_eq!(Sqlite.quoted(&Ident::from_static("order")), r#""order""#);
/// ```
///
/// ```
/// use moso_sql::{Dialect, Expr, Ident, Select, Sqlite, TableRef};
///
/// // `ILIKE` has no SQLite spelling, so both sides are lowered — said out
/// // loud rather than dropped.
/// let query = Select::from_table(TableRef::from_static("users"))
///     .select_all()
///     .filter(Expr::col(Ident::from_static("email")).ilike(Expr::value("A%")))
///     .into_statement();
/// assert_eq!(
///     Sqlite.build(query.borrowed())?.text,
///     r#"SELECT * FROM "users" WHERE lower("email") LIKE lower(?)"#,
/// );
/// # Ok::<(), moso_sql::Error>(())
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sqlite;

impl Sqlite {
    /// The name this dialect reports.
    ///
    /// ```
    /// assert_eq!(moso_sql::Sqlite::NAME, "SQLite");
    /// ```
    pub const NAME: &'static str = "SQLite";

    /// `SQLITE_MAX_VARIABLE_NUMBER` as of 3.32, which is what any recent build
    /// ships with.
    ///
    /// ```
    /// assert_eq!(moso_sql::Sqlite::MAX_BIND_PARAMS, 32_766);
    /// ```
    pub const MAX_BIND_PARAMS: usize = 32_766;
}

impl Dialect for Sqlite {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn quote_ident(&self, ident: &Ident, out: &mut String) {
        out.push('"');
        out.push_str(ident.as_str());
        out.push('"');
    }

    fn placeholder(&self, index: usize, out: &mut String) {
        let _ = index;
        out.push('?');
    }

    fn type_name(&self, data_type: &DataType, out: &mut String) -> Result<(), Error> {
        // SQLite has five storage classes and assigns them by affinity rules
        // over the declared name, so the mapping below is chosen for the
        // affinity it produces, not for the spelling.
        match data_type {
            DataType::Boolean
            | DataType::SmallInt
            | DataType::Integer
            | DataType::BigInt
            | DataType::SmallSerial
            | DataType::Serial
            | DataType::BigSerial => out.push_str("integer"),
            DataType::Real | DataType::DoublePrecision => out.push_str("real"),
            DataType::Numeric { .. } => out.push_str("numeric"),
            DataType::Text
            | DataType::VarChar(_)
            | DataType::Char(_)
            | DataType::Uuid
            | DataType::Json
            | DataType::JsonB
            | DataType::Date
            | DataType::Time { .. }
            | DataType::Timestamp { .. }
            | DataType::Interval
            | DataType::Inet
            | DataType::Cidr
            | DataType::MacAddr
            | DataType::Enum(_) => out.push_str("text"),
            DataType::Bytea => out.push_str("blob"),
            DataType::TsVector | DataType::TsQuery => {
                return Err(Error::unsupported(
                    Self::NAME,
                    "full-text search types",
                    "use an FTS5 virtual table, or keep full-text search on PostgreSQL only",
                ));
            }
            DataType::Array(_) => {
                return Err(Error::unsupported(
                    Self::NAME,
                    "array types",
                    "store the list as a JSON `text` column, or normalise it into its own table",
                ));
            }
            DataType::Custom(name) => self.quote_ident(name.name(), out),
        }
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::sqlite()
    }

    fn build(&self, statement: StatementRef<'_>) -> Result<Sql, Error> {
        crate::render::build(self, statement)
    }

    fn max_bind_params(&self) -> usize {
        Self::MAX_BIND_PARAMS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::TypeRef;

    #[test]
    fn identifiers_are_always_quoted_even_when_they_are_keywords() {
        for dialect in [&Postgres as &dyn Dialect, &Sqlite as &dyn Dialect] {
            assert_eq!(dialect.quoted(&Ident::from_static("select")), r#""select""#);
            assert_eq!(dialect.quoted(&Ident::from_static("a b")), r#""a b""#);
        }
    }

    #[test]
    fn postgres_placeholders_are_one_based_and_sqlite_has_none() {
        let mut text = String::new();
        Postgres.placeholder(0, &mut text);
        Postgres.placeholder(9, &mut text);
        assert_eq!(text, "$1$10");

        let mut text = String::new();
        Sqlite.placeholder(0, &mut text);
        Sqlite.placeholder(9, &mut text);
        assert_eq!(text, "??");
    }

    #[test]
    fn postgres_renders_the_types_a_migration_needs() {
        let cases = [
            (
                DataType::Timestamp {
                    with_time_zone: true,
                },
                "timestamptz",
            ),
            (DataType::JsonB, "jsonb"),
            (DataType::array_of(DataType::Text), "text[]"),
            (
                DataType::Numeric {
                    precision: Some(10),
                    scale: Some(2),
                },
                "numeric(10, 2)",
            ),
            (DataType::Enum(TypeRef::from_static("mood")), r#""mood""#),
        ];
        for (data_type, expected) in cases {
            let mut out = String::new();
            Postgres
                .type_name(&data_type, &mut out)
                .expect("PostgreSQL has every type");
            assert_eq!(out, expected);
        }
    }

    #[test]
    fn sqlite_says_no_rather_than_guessing() {
        let mut out = String::new();
        let error = Sqlite
            .type_name(&DataType::array_of(DataType::Text), &mut out)
            .expect_err("SQLite has no arrays");
        assert!(error.is_dialect_gap());
        assert!(error.to_string().contains("help:"));
    }

    #[test]
    fn the_capability_tables_disagree_where_the_databases_do() {
        let postgres = Capabilities::postgres();
        let sqlite = Capabilities::sqlite();
        assert!(postgres.ilike && !sqlite.ilike);
        assert!(postgres.arrays && !sqlite.arrays);
        assert!(postgres.skip_locked && !sqlite.skip_locked);
        // And agree where they do: both have had these for years, and the
        // preload and pagination code paths depend on them.
        assert!(postgres.window_functions && sqlite.window_functions);
        assert!(postgres.returning && sqlite.returning);
        assert!(postgres.nulls_ordering && sqlite.nulls_ordering);
    }

    #[test]
    fn a_dialect_is_object_safe_and_shareable() {
        fn takes_dyn(dialect: &dyn Dialect) -> &'static str {
            dialect.name()
        }
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}

        assert_eq!(takes_dyn(&Postgres), "PostgreSQL");
        assert_eq!(takes_dyn(&Sqlite), "SQLite");
        assert_send_sync::<dyn Dialect>();
    }
}
