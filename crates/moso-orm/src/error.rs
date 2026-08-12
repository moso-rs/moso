//! The one error type the data layer returns, and the call sites it records.
//!
//! Non-negotiable N7 is *errors that name the problem*. A unique violation is
//! not "database error 23505"; it is "an account with this email already
//! exists", pointing at `/email`, rendered as a 409. That mapping lives here,
//! once, so that every entity gets it without writing anything.

use core::fmt;
use core::time::Duration;

use crate::db::Backend;
use crate::relation::NotLoaded;
use crate::row::DecodeError;

/// The data layer's result type.
///
/// ```
/// use moso_orm::{Error, Result};
///
/// fn find() -> Result<u32> {
///     Err(Error::not_found("User"))
/// }
/// assert!(find().is_err());
/// ```
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Everything that can go wrong between a Rust value and a row.
///
/// # Why one enum and not one per module
///
/// A service function that reads, writes and commits would otherwise return
/// three error types and spend its body converting between them. One enum with
/// honest variants costs a `match` arm in the rare place that cares and nothing
/// anywhere else — and it is what makes the single `From<Error>` conversion into
/// an HTTP problem response possible.
///
/// ```
/// use moso_orm::Error;
///
/// let error = Error::not_found("User");
/// assert!(error.to_string().contains("User"));
/// assert!(!error.is_retryable());
/// ```
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A `fetch_one` found no row.
    #[error("no `{entity}` matched the query")]
    NotFound {
        /// The entity that was queried, as its Rust type is spelled.
        entity: &'static str,
    },

    /// A `UNIQUE` constraint rejected the write.
    #[error("{0}")]
    UniqueViolation(Box<ConstraintViolation>),

    /// A `FOREIGN KEY` constraint rejected the write.
    #[error("{0}")]
    ForeignKeyViolation(Box<ConstraintViolation>),

    /// A `NOT NULL` constraint rejected the write.
    #[error("{0}")]
    NotNullViolation(Box<ConstraintViolation>),

    /// A `CHECK` constraint rejected the write.
    #[error("{0}")]
    CheckViolation(Box<ConstraintViolation>),

    /// An optimistic-locking update matched no row, because the version column
    /// had already moved.
    #[error(
        "`{entity}` was changed by someone else since it was read\n\
         help: re-read the row and re-apply the change, or take a `LockMode::ForUpdate` first"
    )]
    StaleWrite {
        /// The entity whose version column did not match.
        entity: &'static str,
    },

    /// A filter or an ordering term named a column of an entity the query does
    /// not select from and has not joined.
    ///
    /// This is the runtime half of the decision recorded in
    /// [`crate::select`]: the joined set is checked when the statement is
    /// built, before any SQL leaves the process.
    #[error("{0}")]
    Unjoined(Box<Unjoined>),

    /// An `update_all()` or `delete_all()` reached execution with no filter and
    /// no explicit `all_rows()`.
    #[error(
        "this `{operation}` would touch every row of `{table}`\n\
         help: add a `.filter(..)`, or say so on purpose with `.all_rows()`"
    )]
    UnfilteredWrite {
        /// `"UPDATE"` or `"DELETE"`.
        operation: &'static str,
        /// The table that would have been rewritten.
        table: &'static str,
    },

    /// A tenant-scoped entity reached execution without a tenant.
    ///
    /// The common case is a compile error (see [`crate::NeedsTenant`]);
    /// this exists for the paths that build a statement dynamically.
    #[error(
        "`{entity}` is tenant-scoped and this query has no tenant\n\
         help: `{entity}::query().scoped(tenant)`, or `db.for_tenant(tenant)`\n\
         help: to query across tenants deliberately: `{entity}::query().across_tenants()`"
    )]
    TenantMissing {
        /// The tenant-scoped entity.
        entity: &'static str,
    },

    /// A column could not be turned into the Rust type the entity declares.
    #[error(transparent)]
    Decode(#[from] DecodeError),

    /// The statement could not be rendered for this dialect.
    #[error(transparent)]
    Build(#[from] moso_sql::Error),

    /// A pagination cursor was absent, tampered with, or built for a different
    /// ordering.
    #[error(transparent)]
    Cursor(#[from] CursorError),

    /// A relation was read without having been loaded.
    #[error(transparent)]
    NotLoaded(#[from] NotLoaded),

    /// No connection became free within `acquire_timeout`.
    ///
    /// Rendered as `503` with a `Retry-After`, never as a hang: a pool that is
    /// exhausted is a capacity problem, and a request that waits forever turns
    /// it into an outage.
    #[error(
        "no database connection became free within {}ms (pool size {size})\n\
         help: raise `database.max_connections`, lower the request rate, or find the query \
         holding connections — `application_name` names this process in `pg_stat_activity`",
        waited.as_millis()
    )]
    PoolTimeout {
        /// How long the acquire waited.
        waited: Duration,
        /// The configured maximum pool size.
        size: u32,
    },

    /// The connection could not be opened, or was lost mid-statement.
    #[error("the database connection failed: {detail}")]
    Connection {
        /// What the driver reported.
        detail: String,
    },

    /// A `SERIALIZABLE`/`REPEATABLE READ` transaction lost a race
    /// (`SQLSTATE 40001`). Retryable.
    #[error("the transaction could not be serialised against a concurrent one (SQLSTATE {code})")]
    Serialization {
        /// The reported SQLSTATE.
        code: String,
    },

    /// The server chose this transaction as a deadlock victim
    /// (`SQLSTATE 40P01`). Retryable.
    #[error("the transaction was chosen as a deadlock victim (SQLSTATE {code})")]
    Deadlock {
        /// The reported SQLSTATE.
        code: String,
    },

    /// The server cancelled the statement after `statement_timeout`.
    #[error(
        "the statement was cancelled after {}ms\n\
         help: raise `database.statement_timeout`, or run this work in a job",
        after.as_millis()
    )]
    StatementTimeout {
        /// The configured timeout the server enforced.
        after: Duration,
    },

    /// Any other error the server reported, with the statement that caused it.
    #[error("{0}")]
    Database(Box<DatabaseError>),

    /// The handle was built from a configuration the driver rejected.
    #[error("the database configuration is not usable: {detail}")]
    Configuration {
        /// What is wrong, and what to change.
        detail: String,
    },

    /// The construct is real, and this backend cannot express it.
    #[error(
        "{backend} cannot do {feature}\n\
         help: this works on PostgreSQL; see the divergence table in \
         `docs/02-data/20-orm-overview.md`"
    )]
    Unsupported {
        /// What was asked for.
        feature: &'static str,
        /// The backend that cannot do it.
        backend: Backend,
    },
}

impl Error {
    /// A `fetch_one` that found nothing.
    ///
    /// ```
    /// use moso_orm::Error;
    ///
    /// assert!(matches!(Error::not_found("Post"), Error::NotFound { entity: "Post" }));
    /// ```
    #[must_use]
    pub const fn not_found(entity: &'static str) -> Self {
        Self::NotFound { entity }
    }

    /// Whether re-running the same work has a real chance of succeeding.
    ///
    /// True for serialisation failures, deadlocks and pool timeouts; false for
    /// everything a retry would only repeat. [`crate::Db::transaction`] retries
    /// exactly this set.
    ///
    /// ```
    /// use moso_orm::Error;
    ///
    /// let lost = Error::Serialization { code: "40001".into() };
    /// assert!(lost.is_retryable());
    /// assert!(!Error::not_found("User").is_retryable());
    /// ```
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Serialization { .. } | Self::Deadlock { .. } | Self::PoolTimeout { .. }
        )
    }

    /// Whether this is the application's mistake rather than the database's
    /// state — a query that could never have worked.
    ///
    /// These are the ones worth failing a test over: they do not depend on
    /// data.
    ///
    /// ```
    /// use moso_orm::Error;
    ///
    /// assert!(Error::UnfilteredWrite { operation: "UPDATE", table: "users" }.is_programmer_error());
    /// assert!(!Error::not_found("User").is_programmer_error());
    /// ```
    #[must_use]
    pub const fn is_programmer_error(&self) -> bool {
        matches!(
            self,
            Self::Unjoined(_)
                | Self::UnfilteredWrite { .. }
                | Self::TenantMissing { .. }
                | Self::Build(_)
                | Self::Configuration { .. }
        )
    }

    /// The JSON Pointer of the field a client could fix, when there is one.
    ///
    /// This is what turns a `23505` into `409` with `"pointer": "/email"`
    /// rather than an opaque 500.
    ///
    /// ```
    /// use moso_orm::{ConstraintViolation, Error};
    ///
    /// let violation = ConstraintViolation::unique("User", "users_email_key").with_column("email");
    /// let error = Error::UniqueViolation(Box::new(violation));
    /// assert_eq!(error.field_pointer(), Some("/email".to_owned()));
    /// ```
    #[must_use]
    pub fn field_pointer(&self) -> Option<String> {
        match self {
            Self::UniqueViolation(violation)
            | Self::ForeignKeyViolation(violation)
            | Self::NotNullViolation(violation)
            | Self::CheckViolation(violation) => violation.pointer(),
            _ => None,
        }
    }

    /// The statement that produced the error, when one was sent.
    ///
    /// Parameterised, never interpolated: the text is safe to log in
    /// production, and the values are not in it.
    ///
    /// ```
    /// use moso_orm::{DatabaseError, Error};
    ///
    /// let inner = DatabaseError::new("22P02", "invalid input syntax for type uuid")
    ///     .with_sql("select * from users where id = $1");
    /// let error = Error::Database(Box::new(inner));
    /// assert_eq!(error.sql(), Some("select * from users where id = $1"));
    /// ```
    #[must_use]
    pub fn sql(&self) -> Option<&str> {
        match self {
            Self::Database(inner) => inner.sql(),
            Self::UniqueViolation(violation)
            | Self::ForeignKeyViolation(violation)
            | Self::NotNullViolation(violation)
            | Self::CheckViolation(violation) => violation.sql(),
            _ => None,
        }
    }

    /// The SQLSTATE the server reported, when it reported one.
    ///
    /// ```
    /// use moso_orm::Error;
    ///
    /// assert_eq!(Error::Deadlock { code: "40P01".into() }.sqlstate(), Some("40P01"));
    /// assert_eq!(Error::not_found("User").sqlstate(), None);
    /// ```
    #[must_use]
    pub fn sqlstate(&self) -> Option<&str> {
        match self {
            Self::Serialization { code } | Self::Deadlock { code } => Some(code),
            Self::Database(inner) => Some(inner.sqlstate()),
            Self::UniqueViolation(violation)
            | Self::ForeignKeyViolation(violation)
            | Self::NotNullViolation(violation)
            | Self::CheckViolation(violation) => violation.sqlstate(),
            _ => None,
        }
    }
}

/// Where in the user's source a statement was built.
///
/// Captured with `#[track_caller]` at every combinator that can be blamed, so
/// that the error names `src/routes/posts.rs:18` rather than a file inside the
/// framework — rule 1 of the diagnostics style guide.
///
/// ```
/// use moso_orm::CallSite;
///
/// #[track_caller]
/// fn here() -> CallSite {
///     CallSite::caller()
/// }
/// assert!(here().file().ends_with(".rs"));
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CallSite {
    file: &'static str,
    line: u32,
    column: u32,
}

impl CallSite {
    /// The location of the caller of the `#[track_caller]` function this is
    /// called from.
    ///
    /// ```
    /// use moso_orm::CallSite;
    ///
    /// let site = CallSite::caller();
    /// assert!(site.line() > 0);
    /// ```
    #[must_use]
    #[track_caller]
    pub fn caller() -> Self {
        let location = core::panic::Location::caller();
        Self {
            file: location.file(),
            line: location.line(),
            column: location.column(),
        }
    }

    /// The source file, as rustc spells it.
    ///
    /// ```
    /// assert!(moso_orm::CallSite::caller().file().ends_with(".rs"));
    /// ```
    #[must_use]
    pub const fn file(&self) -> &'static str {
        self.file
    }

    /// The one-based line.
    ///
    /// ```
    /// assert!(moso_orm::CallSite::caller().line() >= 1);
    /// ```
    #[must_use]
    pub const fn line(&self) -> u32 {
        self.line
    }

    /// The one-based column.
    ///
    /// ```
    /// assert!(moso_orm::CallSite::caller().column() >= 1);
    /// ```
    #[must_use]
    pub const fn column(&self) -> u32 {
        self.column
    }
}

impl fmt::Display for CallSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

impl fmt::Debug for CallSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

/// A constraint the database refused to violate, translated into the words the
/// application uses.
///
/// ```
/// use moso_orm::ConstraintViolation;
///
/// let violation = ConstraintViolation::unique("User", "users_email_key")
///     .with_column("email")
///     .with_message("an account with this email already exists");
/// assert_eq!(violation.pointer().as_deref(), Some("/email"));
/// assert_eq!(violation.entity(), "User");
/// ```
#[derive(Clone, Debug, thiserror::Error)]
pub struct ConstraintViolation {
    entity: &'static str,
    constraint: String,
    kind: ConstraintKind,
    columns: Vec<String>,
    message: Option<String>,
    sqlstate: Option<String>,
    sql: Option<String>,
    at: Option<CallSite>,
}

impl ConstraintViolation {
    /// A `UNIQUE` violation on `constraint`.
    ///
    /// ```
    /// use moso_orm::{ConstraintKind, ConstraintViolation};
    ///
    /// let v = ConstraintViolation::unique("User", "users_email_key");
    /// assert_eq!(v.kind(), ConstraintKind::Unique);
    /// ```
    #[must_use]
    pub fn unique(entity: &'static str, constraint: impl Into<String>) -> Self {
        Self::new(entity, constraint, ConstraintKind::Unique)
    }

    /// A `FOREIGN KEY` violation on `constraint`.
    ///
    /// ```
    /// use moso_orm::{ConstraintKind, ConstraintViolation};
    ///
    /// let v = ConstraintViolation::foreign_key("Post", "posts_author_id_fkey");
    /// assert_eq!(v.kind(), ConstraintKind::ForeignKey);
    /// ```
    #[must_use]
    pub fn foreign_key(entity: &'static str, constraint: impl Into<String>) -> Self {
        Self::new(entity, constraint, ConstraintKind::ForeignKey)
    }

    /// A violation of `kind` on `constraint`.
    ///
    /// ```
    /// use moso_orm::{ConstraintKind, ConstraintViolation};
    ///
    /// let v = ConstraintViolation::new("Order", "orders_total_positive", ConstraintKind::Check);
    /// assert_eq!(v.constraint(), "orders_total_positive");
    /// ```
    #[must_use]
    pub fn new(entity: &'static str, constraint: impl Into<String>, kind: ConstraintKind) -> Self {
        Self {
            entity,
            constraint: constraint.into(),
            kind,
            columns: Vec::new(),
            message: None,
            sqlstate: None,
            sql: None,
            at: None,
        }
    }

    /// Names a column the constraint covers.
    ///
    /// The first one becomes the JSON Pointer a client sees.
    ///
    /// ```
    /// use moso_orm::ConstraintViolation;
    ///
    /// let v = ConstraintViolation::unique("User", "u").with_column("email");
    /// assert_eq!(v.columns(), ["email"]);
    /// ```
    #[must_use]
    pub fn with_column(mut self, column: impl Into<String>) -> Self {
        self.columns.push(column.into());
        self
    }

    /// Replaces the sentence a client reads.
    ///
    /// ```
    /// use moso_orm::ConstraintViolation;
    ///
    /// let v = ConstraintViolation::unique("User", "u").with_message("email already taken");
    /// assert_eq!(v.message(), "email already taken");
    /// ```
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Records the SQLSTATE the server reported.
    ///
    /// ```
    /// use moso_orm::ConstraintViolation;
    ///
    /// let v = ConstraintViolation::unique("User", "u").with_sqlstate("23505");
    /// assert_eq!(v.sqlstate(), Some("23505"));
    /// ```
    #[must_use]
    pub fn with_sqlstate(mut self, sqlstate: impl Into<String>) -> Self {
        self.sqlstate = Some(sqlstate.into());
        self
    }

    /// Records the parameterised statement that was sent.
    ///
    /// ```
    /// use moso_orm::ConstraintViolation;
    ///
    /// let v = ConstraintViolation::unique("User", "u").with_sql("insert into users ..");
    /// assert!(v.sql().is_some());
    /// ```
    #[must_use]
    pub fn with_sql(mut self, sql: impl Into<String>) -> Self {
        self.sql = Some(sql.into());
        self
    }

    /// Records where the statement was built.
    ///
    /// ```
    /// use moso_orm::{CallSite, ConstraintViolation};
    ///
    /// let v = ConstraintViolation::unique("User", "u").at(CallSite::caller());
    /// assert!(v.call_site().is_some());
    /// ```
    #[must_use]
    pub fn at(mut self, site: CallSite) -> Self {
        self.at = Some(site);
        self
    }

    /// The entity being written.
    ///
    /// ```
    /// assert_eq!(moso_orm::ConstraintViolation::unique("User", "u").entity(), "User");
    /// ```
    #[must_use]
    pub const fn entity(&self) -> &'static str {
        self.entity
    }

    /// The constraint's name, as the database knows it.
    ///
    /// ```
    /// assert_eq!(moso_orm::ConstraintViolation::unique("User", "u").constraint(), "u");
    /// ```
    #[must_use]
    pub fn constraint(&self) -> &str {
        &self.constraint
    }

    /// Which kind of constraint refused.
    ///
    /// ```
    /// use moso_orm::{ConstraintKind, ConstraintViolation};
    ///
    /// assert_eq!(ConstraintViolation::unique("U", "u").kind(), ConstraintKind::Unique);
    /// ```
    #[must_use]
    pub const fn kind(&self) -> ConstraintKind {
        self.kind
    }

    /// The columns the constraint covers, in declaration order.
    ///
    /// ```
    /// assert!(moso_orm::ConstraintViolation::unique("U", "u").columns().is_empty());
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// The sentence a client reads, generated from the kind when none was set.
    ///
    /// ```
    /// use moso_orm::ConstraintViolation;
    ///
    /// let generated = ConstraintViolation::unique("User", "u").with_column("email");
    /// assert!(generated.message().contains("already exists"));
    ///
    /// let written = generated.with_message("an account with this email already exists");
    /// assert!(written.message().contains("email"));
    /// ```
    #[must_use]
    pub fn message(&self) -> &str {
        match &self.message {
            Some(message) => message,
            None => self.kind.default_message(),
        }
    }

    /// The SQLSTATE, when the server reported one.
    ///
    /// ```
    /// assert!(moso_orm::ConstraintViolation::unique("U", "u").sqlstate().is_none());
    /// ```
    #[must_use]
    pub fn sqlstate(&self) -> Option<&str> {
        self.sqlstate.as_deref()
    }

    /// The parameterised statement, when one was recorded.
    ///
    /// ```
    /// assert!(moso_orm::ConstraintViolation::unique("U", "u").sql().is_none());
    /// ```
    #[must_use]
    pub fn sql(&self) -> Option<&str> {
        self.sql.as_deref()
    }

    /// Where the statement was built, when it was recorded.
    ///
    /// ```
    /// assert!(moso_orm::ConstraintViolation::unique("U", "u").call_site().is_none());
    /// ```
    #[must_use]
    pub const fn call_site(&self) -> Option<CallSite> {
        self.at
    }

    /// The JSON Pointer of the first covered column, for a problem response.
    ///
    /// ```
    /// use moso_orm::ConstraintViolation;
    ///
    /// let v = ConstraintViolation::unique("U", "u").with_column("email");
    /// assert_eq!(v.pointer().as_deref(), Some("/email"));
    /// ```
    #[must_use]
    pub fn pointer(&self) -> Option<String> {
        self.columns.first().map(|column| format!("/{column}"))
    }
}

impl fmt::Display for ConstraintViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.entity, self.message())?;
        if !self.columns.is_empty() {
            write!(f, " ({})", self.columns.join(", "))?;
        }
        write!(f, " [constraint {}]", self.constraint)
    }
}

/// Which kind of constraint refused a write.
///
/// ```
/// use moso_orm::ConstraintKind;
///
/// assert_eq!(ConstraintKind::Unique.sqlstate(), "23505");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConstraintKind {
    /// `UNIQUE` or a unique index.
    Unique,
    /// `FOREIGN KEY`.
    ForeignKey,
    /// `NOT NULL`.
    NotNull,
    /// `CHECK`.
    Check,
    /// `EXCLUDE`.
    Exclusion,
}

impl ConstraintKind {
    /// The PostgreSQL SQLSTATE this kind reports.
    ///
    /// ```
    /// use moso_orm::ConstraintKind;
    ///
    /// assert_eq!(ConstraintKind::ForeignKey.sqlstate(), "23503");
    /// ```
    #[must_use]
    pub const fn sqlstate(self) -> &'static str {
        match self {
            Self::Unique => "23505",
            Self::ForeignKey => "23503",
            Self::NotNull => "23502",
            Self::Check => "23514",
            Self::Exclusion => "23P01",
        }
    }

    /// The sentence used when the application did not write one.
    ///
    /// ```
    /// use moso_orm::ConstraintKind;
    ///
    /// assert!(ConstraintKind::Unique.default_message().contains("already"));
    /// ```
    #[must_use]
    pub const fn default_message(self) -> &'static str {
        match self {
            Self::Unique => "a row with this value already exists",
            Self::ForeignKey => "the referenced row does not exist",
            Self::NotNull => "this value is required",
            Self::Check => "this value is not allowed",
            Self::Exclusion => "this value overlaps an existing row",
        }
    }

    /// Whether a client could fix it by sending different input.
    ///
    /// ```
    /// use moso_orm::ConstraintKind;
    ///
    /// assert!(ConstraintKind::Unique.is_client_fixable());
    /// ```
    #[must_use]
    pub const fn is_client_fixable(self) -> bool {
        matches!(
            self,
            Self::Unique | Self::ForeignKey | Self::NotNull | Self::Check
        )
    }
}

/// A column reference the query has no table for.
///
/// This is the error the joined-set decision produces. It is raised when the
/// statement is *built*, so it never reaches the database, and it names the
/// call site of the combinator that introduced the reference.
///
/// ```
/// use moso_orm::Unjoined;
///
/// let error = Unjoined::new("Post", "posts", "User", "users.is_admin")
///     .with_relation("Post::AUTHOR")
///     .with_foreign_key("Post::AUTHOR_ID");
/// let text = error.to_string();
/// assert!(text.contains("`User` is not joined"));
/// assert!(text.contains(".join(Post::AUTHOR)"));
/// ```
#[derive(Clone, Debug, thiserror::Error)]
pub struct Unjoined {
    entity: &'static str,
    table: &'static str,
    referenced: &'static str,
    column: String,
    relation: Option<&'static str>,
    foreign_key: Option<&'static str>,
    joined: Vec<&'static str>,
    at: Option<CallSite>,
}

impl Unjoined {
    /// The query selects from `entity` (`table`) and the expression mentions
    /// `column`, which belongs to `referenced`.
    ///
    /// ```
    /// use moso_orm::Unjoined;
    ///
    /// let error = Unjoined::new("Post", "posts", "User", "users.is_admin");
    /// assert_eq!(error.referenced(), "User");
    /// ```
    #[must_use]
    pub fn new(
        entity: &'static str,
        table: &'static str,
        referenced: &'static str,
        column: impl Into<String>,
    ) -> Self {
        Self {
            entity,
            table,
            referenced,
            column: column.into(),
            relation: None,
            foreign_key: None,
            joined: Vec::new(),
            at: None,
        }
    }

    /// The relation constant that would join it, for the `help:` line.
    ///
    /// ```
    /// use moso_orm::Unjoined;
    ///
    /// let error = Unjoined::new("Post", "posts", "User", "users.id")
    ///     .with_relation("Post::AUTHOR");
    /// assert!(error.to_string().contains("Post::AUTHOR"));
    /// ```
    #[must_use]
    pub const fn with_relation(mut self, relation: &'static str) -> Self {
        self.relation = Some(relation);
        self
    }

    /// The foreign-key column constant, for the second `help:` line.
    ///
    /// ```
    /// use moso_orm::Unjoined;
    ///
    /// let error = Unjoined::new("Post", "posts", "User", "users.id")
    ///     .with_foreign_key("Post::AUTHOR_ID");
    /// assert!(error.to_string().contains("Post::AUTHOR_ID"));
    /// ```
    #[must_use]
    pub const fn with_foreign_key(mut self, column: &'static str) -> Self {
        self.foreign_key = Some(column);
        self
    }

    /// Records what *is* joined, so the message can say so.
    ///
    /// ```
    /// use moso_orm::Unjoined;
    ///
    /// let error = Unjoined::new("Post", "posts", "Tag", "tags.slug").with_joined(["User"]);
    /// assert!(error.to_string().contains("User"));
    /// ```
    #[must_use]
    pub fn with_joined(mut self, joined: impl IntoIterator<Item = &'static str>) -> Self {
        self.joined.extend(joined);
        self
    }

    /// Records where the offending combinator was called.
    ///
    /// ```
    /// use moso_orm::{CallSite, Unjoined};
    ///
    /// let error = Unjoined::new("Post", "posts", "User", "users.id").at(CallSite::caller());
    /// assert!(error.call_site().is_some());
    /// ```
    #[must_use]
    pub const fn at(mut self, site: CallSite) -> Self {
        self.at = Some(site);
        self
    }

    /// The entity the query selects from.
    ///
    /// ```
    /// assert_eq!(moso_orm::Unjoined::new("Post", "posts", "User", "u.id").entity(), "Post");
    /// ```
    #[must_use]
    pub const fn entity(&self) -> &'static str {
        self.entity
    }

    /// The entity the column belongs to.
    ///
    /// ```
    /// assert_eq!(moso_orm::Unjoined::new("Post", "posts", "User", "u.id").referenced(), "User");
    /// ```
    #[must_use]
    pub const fn referenced(&self) -> &'static str {
        self.referenced
    }

    /// The table the query selects from.
    ///
    /// ```
    /// assert_eq!(moso_orm::Unjoined::new("Post", "posts", "User", "u.id").table(), "posts");
    /// ```
    #[must_use]
    pub const fn table(&self) -> &'static str {
        self.table
    }

    /// The qualified column, as it was written into the expression.
    ///
    /// ```
    /// assert_eq!(moso_orm::Unjoined::new("P", "p", "U", "u.id").column(), "u.id");
    /// ```
    #[must_use]
    pub fn column(&self) -> &str {
        &self.column
    }

    /// Where the offending combinator was called, when it was recorded.
    ///
    /// ```
    /// assert!(moso_orm::Unjoined::new("P", "p", "U", "u.id").call_site().is_none());
    /// ```
    #[must_use]
    pub const fn call_site(&self) -> Option<CallSite> {
        self.at
    }
}

impl fmt::Display for Unjoined {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "`{}` is not joined in this query", self.referenced)?;
        if let Some(at) = self.at {
            writeln!(f, "  at {at}")?;
        }
        writeln!(f, "  this expression mentions `{}`,", self.column)?;
        write!(f, "  but the query selects from `{}`", self.entity)?;
        match self.joined.as_slice() {
            [] => writeln!(f, " and joins nothing")?,
            joined => writeln!(f, " and joins {}", joined.join(", "))?,
        }
        if let Some(relation) = self.relation {
            writeln!(f, "  help: add `.join({relation})` before this filter")?;
        }
        if let Some(column) = self.foreign_key {
            writeln!(f, "  help: or filter on the foreign key: `{column}.eq(..)`")?;
        }
        Ok(())
    }
}

/// Any other error the server reported.
///
/// ```
/// use moso_orm::DatabaseError;
///
/// let error = DatabaseError::new("42703", "column \"nam\" does not exist")
///     .with_sql("select nam from users");
/// assert_eq!(error.sqlstate(), "42703");
/// ```
#[derive(Clone, Debug, thiserror::Error)]
pub struct DatabaseError {
    sqlstate: String,
    message: String,
    detail: Option<String>,
    hint: Option<String>,
    sql: Option<String>,
    at: Option<CallSite>,
}

impl DatabaseError {
    /// A server error with its SQLSTATE and primary message.
    ///
    /// ```
    /// use moso_orm::DatabaseError;
    ///
    /// assert_eq!(DatabaseError::new("22001", "value too long").sqlstate(), "22001");
    /// ```
    #[must_use]
    pub fn new(sqlstate: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            sqlstate: sqlstate.into(),
            message: message.into(),
            detail: None,
            hint: None,
            sql: None,
            at: None,
        }
    }

    /// Adds the server's `DETAIL` field.
    ///
    /// ```
    /// use moso_orm::DatabaseError;
    ///
    /// let e = DatabaseError::new("22001", "too long").with_detail("column `name`");
    /// assert_eq!(e.detail(), Some("column `name`"));
    /// ```
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Adds the server's `HINT` field, which is often the fix.
    ///
    /// ```
    /// use moso_orm::DatabaseError;
    ///
    /// let e = DatabaseError::new("42703", "no column").with_hint("perhaps you meant `name`");
    /// assert!(e.to_string().contains("perhaps"));
    /// ```
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Records the parameterised statement that was sent.
    ///
    /// ```
    /// use moso_orm::DatabaseError;
    ///
    /// let e = DatabaseError::new("42703", "no column").with_sql("select nam from users");
    /// assert!(e.sql().is_some());
    /// ```
    #[must_use]
    pub fn with_sql(mut self, sql: impl Into<String>) -> Self {
        self.sql = Some(sql.into());
        self
    }

    /// Records where the statement was built.
    ///
    /// ```
    /// use moso_orm::{CallSite, DatabaseError};
    ///
    /// let e = DatabaseError::new("42703", "no column").at(CallSite::caller());
    /// assert!(e.call_site().is_some());
    /// ```
    #[must_use]
    pub fn at(mut self, site: CallSite) -> Self {
        self.at = Some(site);
        self
    }

    /// The SQLSTATE.
    ///
    /// ```
    /// assert_eq!(moso_orm::DatabaseError::new("42703", "m").sqlstate(), "42703");
    /// ```
    #[must_use]
    pub fn sqlstate(&self) -> &str {
        &self.sqlstate
    }

    /// The server's primary message.
    ///
    /// ```
    /// assert_eq!(moso_orm::DatabaseError::new("42703", "m").message(), "m");
    /// ```
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The server's `DETAIL`, when there was one.
    ///
    /// ```
    /// assert!(moso_orm::DatabaseError::new("42703", "m").detail().is_none());
    /// ```
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// The server's `HINT`, when there was one.
    ///
    /// ```
    /// assert!(moso_orm::DatabaseError::new("42703", "m").hint().is_none());
    /// ```
    #[must_use]
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// The parameterised statement, when one was recorded.
    ///
    /// ```
    /// assert!(moso_orm::DatabaseError::new("42703", "m").sql().is_none());
    /// ```
    #[must_use]
    pub fn sql(&self) -> Option<&str> {
        self.sql.as_deref()
    }

    /// Where the statement was built, when it was recorded.
    ///
    /// ```
    /// assert!(moso_orm::DatabaseError::new("42703", "m").call_site().is_none());
    /// ```
    #[must_use]
    pub const fn call_site(&self) -> Option<CallSite> {
        self.at
    }
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (SQLSTATE {})", self.message, self.sqlstate)?;
        if let Some(detail) = &self.detail {
            write!(f, ": {detail}")?;
        }
        if let Some(hint) = &self.hint {
            write!(f, "\n  help: {hint}")?;
        }
        if let Some(at) = self.at {
            write!(f, "\n  at {at}")?;
        }
        Ok(())
    }
}

/// Why a pagination cursor was refused.
///
/// Cursors are signed with the application secret and carry the ordering they
/// were produced for, so a tampered or reused cursor is rejected instead of
/// silently producing a page from the middle of a different query.
///
/// ```
/// use moso_orm::CursorError;
///
/// assert!(CursorError::Tampered.to_string().contains("cursor"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CursorError {
    /// The cursor was not valid base64url, or was truncated.
    #[error(
        "this pagination cursor is malformed\n\
         help: pass the `next` value from the previous page unchanged, or omit it for page one"
    )]
    Malformed,

    /// The signature did not verify.
    #[error(
        "this pagination cursor was not issued by this application\n\
         help: pass the `next` value from the previous page unchanged"
    )]
    Tampered,

    /// The cursor was issued for a different ordering, so resuming from it
    /// would skip or repeat rows.
    #[error(
        "this pagination cursor belongs to a differently ordered query\n\
         help: restart from page one when the sort changes — a cursor encodes the ordering key"
    )]
    OrderingChanged,

    /// The query has no deterministic order to paginate by.
    #[error(
        "keyset pagination needs a deterministic order\n\
         help: add `.order_by(..)`; the primary key is appended automatically as a tiebreaker"
    )]
    NoOrder,

    /// No signing key was configured, so a cursor could not be signed.
    #[error(
        "cursor pagination needs a signing key\n\
         help: set `app.secret_key`, or use `paginate_offset(page, per_page)`"
    )]
    NoSigningKey,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unique_violation_points_at_the_field() {
        let violation = ConstraintViolation::unique("User", "users_email_key")
            .with_column("email")
            .with_sqlstate("23505");
        let error = Error::UniqueViolation(Box::new(violation));
        assert_eq!(error.field_pointer().as_deref(), Some("/email"));
        assert_eq!(error.sqlstate(), Some("23505"));
        assert!(!error.is_retryable());
    }

    #[test]
    fn only_the_three_transient_failures_retry() {
        assert!(
            Error::Serialization {
                code: "40001".into()
            }
            .is_retryable()
        );
        assert!(
            Error::Deadlock {
                code: "40P01".into()
            }
            .is_retryable()
        );
        assert!(
            Error::PoolTimeout {
                waited: Duration::from_secs(10),
                size: 8
            }
            .is_retryable()
        );
        assert!(!Error::StaleWrite { entity: "Order" }.is_retryable());
        assert!(
            !Error::UnfilteredWrite {
                operation: "DELETE",
                table: "users"
            }
            .is_retryable()
        );
    }

    #[test]
    fn the_unjoined_message_names_the_entity_and_both_fixes() {
        let error = Unjoined::new("Post", "posts", "User", "users.is_admin")
            .with_relation("Post::AUTHOR")
            .with_foreign_key("Post::AUTHOR_ID")
            .at(CallSite::caller());
        let text = error.to_string();
        assert!(
            text.contains("`User` is not joined in this query"),
            "{text}"
        );
        assert!(text.contains("help: add `.join(Post::AUTHOR)`"), "{text}");
        assert!(text.contains("Post::AUTHOR_ID"), "{text}");
        assert!(text.contains(".rs:"), "{text}");
        // Style-guide rule 2: nothing printed is a long type.
        for line in text.lines() {
            assert!(line.len() <= 100, "line too long for a terminal: {line}");
        }
    }

    #[test]
    fn programmer_errors_are_separable_from_data_errors() {
        assert!(
            Error::Unjoined(Box::new(Unjoined::new("P", "p", "U", "u.id"))).is_programmer_error()
        );
        assert!(Error::TenantMissing { entity: "Invoice" }.is_programmer_error());
        assert!(!Error::not_found("User").is_programmer_error());
        assert!(
            !Error::Connection {
                detail: "reset".into()
            }
            .is_programmer_error()
        );
    }

    #[test]
    fn every_constraint_kind_has_a_sqlstate_and_a_sentence() {
        for kind in [
            ConstraintKind::Unique,
            ConstraintKind::ForeignKey,
            ConstraintKind::NotNull,
            ConstraintKind::Check,
            ConstraintKind::Exclusion,
        ] {
            assert_eq!(kind.sqlstate().len(), 5, "{kind:?}");
            assert!(!kind.default_message().is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn a_call_site_renders_as_file_line_column() {
        let site = CallSite::caller();
        let rendered = site.to_string();
        assert!(rendered.contains("error.rs"), "{rendered}");
        assert!(rendered.matches(':').count() >= 2, "{rendered}");
    }
}
