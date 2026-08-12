//! [`Select<E, J>`] — the shape-stable query builder.
//!
//! # N1, stated exactly
//!
//! Every combinator returns `Select<E, J>` with **both** parameters unchanged.
//! `.join(..)` does not change `J` — see [`crate::predicate`] for why the
//! joined-entity set is checked when the statement is built rather than encoded
//! in the type. `J` changes in exactly one place: `.scoped(..)` and
//! `.across_tenants()` discharge a tenant obligation, turning
//! `Select<Invoice, NeedsTenant>` into `Select<Invoice>`.
//!
//! The longest type a user sees is `moso_orm::Select<shop::models::User>`.
//!
//! ```
//! use moso_orm::Select;
//! # use moso_orm::{ColumnDef, DecodeError, Entity, Row};
//! # use moso_orm::descriptor::EntityDescriptor;
//! # use moso_sql::{TableRef, ValueKind};
//! # use std::sync::OnceLock;
//! # #[derive(Clone, Debug)] pub struct User { pub id: i64 }
//! # impl Entity for User {
//! #     type Pk = i64;
//! #     const TABLE: TableRef = TableRef::from_static("users");
//! #     const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
//! #     const NAME: &'static str = "User";
//! #     fn pk(&self) -> i64 { self.id }
//! #     fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
//! #     fn descriptor() -> &'static EntityDescriptor {
//! #         static D: OnceLock<EntityDescriptor> = OnceLock::new();
//! #         D.get_or_init(|| EntityDescriptor::builder("User", Self::TABLE).build())
//! #     }
//! # }
//! # const ID: moso_orm::Column<User, i64> = moso_orm::Column::new("id");
//! // Ten combinators, one type.
//! let query: Select<User> = Select::<User>::new()
//!     .filter(ID.gt(0))
//!     .filter_opt(None::<moso_orm::Predicate>)
//!     .filter_if(true, || ID.lt(100))
//!     .when(false, |q| q)
//!     .apply(|q| q)
//!     .order_by(ID.desc())
//!     .limit(20)
//!     .offset(0)
//!     .distinct()
//!     .with_deleted();
//!
//! assert_eq!(query.filters().len(), 2);
//! ```

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, Ordering};

use moso_sql::{
    ColumnRef, Expr, FromItem, Ident, Join as SqlJoin, JoinKind as SqlJoinKind, Lock,
    LockBehavior as SqlLockBehavior, LockStrength, Order, OrderTerm, Select as SqlSelect,
    SelectItem, Statement, TableRef,
};

use crate::column::Column;
use crate::db::TenantId;
use crate::delete::Delete;
use crate::entity::{Entity, Ready};
use crate::error::{CallSite, Error, Result, Unjoined};
use crate::executor::Executor;
use crate::expr::count_star;
use crate::page::{OffsetPaginated, Paginated};
use crate::predicate::Predicate;
use crate::projection::{ColumnTuple, Projection};
use crate::relation::{Preload, Relation};
use crate::row::Row;
use crate::scope::{Scope, requires_tenant, soft_delete_predicate, tenant_predicate};
use crate::sqltype::SqlType;
use crate::update::Update;
use moso_schema::types::Cursor;

/// The row cap [`Select::fetch_all`] applies when a query asks for no limit.
///
/// Ten thousand rows is deliberate paternalism (`04-devex/45`, *Denial of
/// service*): an unbounded `SELECT *` that works on a dev database and exhausts
/// memory in production is a rite of passage Moso chooses to prevent. The cap
/// logs a `warn` naming the entity when it is reached, and
/// [`Select::unlimited`] opts out of it for the one query that genuinely wants
/// every row.
///
/// ```
/// assert_eq!(moso_orm::select::DEFAULT_ROW_LIMIT, 10_000);
/// ```
pub const DEFAULT_ROW_LIMIT: u64 = 10_000;

/// The process-wide row cap, which [`set_default_row_limit`] changes.
static ROW_LIMIT: AtomicU64 = AtomicU64::new(DEFAULT_ROW_LIMIT);

/// The row cap [`Select::fetch_all`] currently applies.
///
/// ```
/// assert_eq!(moso_orm::select::default_row_limit(), moso_orm::select::DEFAULT_ROW_LIMIT);
/// ```
#[must_use]
pub fn default_row_limit() -> u64 {
    ROW_LIMIT.load(Ordering::Relaxed)
}

/// Changes the row cap for this process.
///
/// `0` disables the cap entirely, which is the same thing as calling
/// [`Select::unlimited`] on every query — do it once at boot, from the
/// application's configuration, and never from a request handler.
///
/// ```
/// use moso_orm::select::{DEFAULT_ROW_LIMIT, default_row_limit, set_default_row_limit};
///
/// set_default_row_limit(500);
/// assert_eq!(default_row_limit(), 500);
/// set_default_row_limit(DEFAULT_ROW_LIMIT);
/// ```
pub fn set_default_row_limit(rows: u64) {
    ROW_LIMIT.store(rows, Ordering::Relaxed);
}

/// A `SELECT` over `E`, with an outstanding scope obligation `J`.
///
/// `J` defaults to `()`, which means "nothing outstanding", and is invisible in
/// every query on an entity that is not tenant-scoped.
///
/// ```
/// # use moso_orm::Select;
/// # use moso_orm::{ColumnDef, DecodeError, Entity, Row};
/// # use moso_orm::descriptor::EntityDescriptor;
/// # use moso_sql::{TableRef, ValueKind};
/// # use std::sync::OnceLock;
/// # #[derive(Clone, Debug)] pub struct Tag { pub id: i64 }
/// # impl Entity for Tag {
/// #     type Pk = i64;
/// #     const TABLE: TableRef = TableRef::from_static("tags");
/// #     const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
/// #     const NAME: &'static str = "Tag";
/// #     fn pk(&self) -> i64 { self.id }
/// #     fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
/// #     fn descriptor() -> &'static EntityDescriptor {
/// #         static D: OnceLock<EntityDescriptor> = OnceLock::new();
/// #         D.get_or_init(|| EntityDescriptor::builder("Tag", Self::TABLE).build())
/// #     }
/// # }
/// let all_tags = Select::<Tag>::new();
/// assert!(all_tags.filters().is_empty());
/// ```
pub struct Select<E, J = ()> {
    filters: Vec<Filter>,
    joins: Vec<Joined>,
    order: Vec<OrderTerm>,
    group_by: Vec<Expr>,
    having: Vec<Expr>,
    preloads: Vec<Preload>,
    limit: Option<u64>,
    offset: Option<u64>,
    lock: Option<LockMode>,
    lock_behavior: LockBehavior,
    distinct: bool,
    deleted: Deleted,
    tenant: Option<TenantId>,
    across_tenants: bool,
    unlimited: bool,
    entity: PhantomData<fn() -> E>,
    scope: PhantomData<fn() -> J>,
}

/// One accumulated filter, with the line that added it.
///
/// The call site is what lets an out-of-scope column blame the user's file
/// rather than a framework one.
///
/// ```
/// use moso_orm::{Filter, Predicate};
/// use moso_sql::Expr;
///
/// let filter = Filter::new(Predicate::unchecked(Expr::value(true)));
/// assert!(filter.predicate().entities().is_empty());
/// ```
#[derive(Clone, Debug)]
pub struct Filter {
    predicate: Predicate,
    at: CallSite,
}

impl Filter {
    /// Records `predicate`, capturing the caller's location.
    ///
    /// ```
    /// use moso_orm::{Filter, Predicate};
    /// use moso_sql::Expr;
    ///
    /// let filter = Filter::new(Predicate::unchecked(Expr::value(true)));
    /// assert!(filter.call_site().file().ends_with(".rs"));
    /// ```
    #[must_use]
    #[track_caller]
    pub fn new(predicate: Predicate) -> Self {
        Self {
            predicate,
            at: CallSite::caller(),
        }
    }

    /// The predicate.
    ///
    /// ```
    /// # use moso_orm::{Filter, Predicate};
    /// # use moso_sql::Expr;
    /// let filter = Filter::new(Predicate::of(["User"], Expr::value(true)));
    /// assert_eq!(filter.predicate().entities(), ["User"]);
    /// ```
    #[must_use]
    pub const fn predicate(&self) -> &Predicate {
        &self.predicate
    }

    /// Where it was added.
    ///
    /// ```
    /// # use moso_orm::{Filter, Predicate};
    /// # use moso_sql::Expr;
    /// let filter = Filter::new(Predicate::unchecked(Expr::value(true)));
    /// assert!(filter.call_site().line() > 0);
    /// ```
    #[must_use]
    pub const fn call_site(&self) -> CallSite {
        self.at
    }

    /// The predicate's expression, consuming the filter.
    ///
    /// ```
    /// # use moso_orm::{Filter, Predicate};
    /// # use moso_sql::Expr;
    /// let filter = Filter::new(Predicate::unchecked(Expr::value(true)));
    /// assert_eq!(filter.into_expr(), Expr::value(true));
    /// ```
    #[must_use]
    pub fn into_expr(self) -> Expr {
        self.predicate.into_expr()
    }
}

/// One entity brought into scope by a join.
///
/// ```
/// use moso_orm::{JoinKind, Joined};
/// use moso_sql::{Expr, TableRef};
///
/// let author = Joined::new(JoinKind::Inner, "User", TableRef::from_static("users"), Expr::value(true));
/// assert_eq!(author.entity(), "User");
/// assert_eq!(author.kind(), JoinKind::Inner);
/// ```
#[derive(Clone, Debug)]
pub struct Joined {
    kind: JoinKind,
    entity: &'static str,
    table: TableRef,
    on: Expr,
    alias: Option<Ident>,
}

impl Joined {
    /// A join of `entity`'s table on `on`.
    ///
    /// ```
    /// # use moso_orm::{JoinKind, Joined};
    /// # use moso_sql::{Expr, TableRef};
    /// let j = Joined::new(JoinKind::Left, "Tag", TableRef::from_static("tags"), Expr::value(true));
    /// assert_eq!(j.table().name().as_str(), "tags");
    /// ```
    #[must_use]
    pub const fn new(kind: JoinKind, entity: &'static str, table: TableRef, on: Expr) -> Self {
        Self {
            kind,
            entity,
            table,
            on,
            alias: None,
        }
    }

    /// Gives the joined table an alias, for a self-join.
    ///
    /// ```
    /// # use moso_orm::{JoinKind, Joined};
    /// # use moso_sql::{Expr, Ident, TableRef};
    /// let j = Joined::new(JoinKind::Inner, "C", TableRef::from_static("categories"), Expr::value(true))
    ///     .aliased(Ident::from_static("parent"));
    /// assert!(j.alias().is_some());
    /// ```
    #[must_use]
    pub fn aliased(mut self, alias: Ident) -> Self {
        self.alias = Some(alias);
        self
    }

    /// Which kind of join.
    ///
    /// ```
    /// # use moso_orm::{JoinKind, Joined};
    /// # use moso_sql::{Expr, TableRef};
    /// let j = Joined::new(JoinKind::Left, "T", TableRef::from_static("t"), Expr::value(true));
    /// assert_eq!(j.kind(), JoinKind::Left);
    /// ```
    #[must_use]
    pub const fn kind(&self) -> JoinKind {
        self.kind
    }

    /// The entity this join brings into scope.
    ///
    /// ```
    /// # use moso_orm::{JoinKind, Joined};
    /// # use moso_sql::{Expr, TableRef};
    /// let j = Joined::new(JoinKind::Inner, "Tag", TableRef::from_static("t"), Expr::value(true));
    /// assert_eq!(j.entity(), "Tag");
    /// ```
    #[must_use]
    pub const fn entity(&self) -> &'static str {
        self.entity
    }

    /// The joined table.
    ///
    /// ```
    /// # use moso_orm::{JoinKind, Joined};
    /// # use moso_sql::{Expr, TableRef};
    /// let j = Joined::new(JoinKind::Inner, "Tag", TableRef::from_static("tags"), Expr::value(true));
    /// assert_eq!(j.table().name().as_str(), "tags");
    /// ```
    #[must_use]
    pub const fn table(&self) -> &TableRef {
        &self.table
    }

    /// The `ON` condition.
    ///
    /// ```
    /// # use moso_orm::{JoinKind, Joined};
    /// # use moso_sql::{Expr, TableRef};
    /// let j = Joined::new(JoinKind::Inner, "T", TableRef::from_static("t"), Expr::value(true));
    /// assert_eq!(j.on(), &Expr::value(true));
    /// ```
    #[must_use]
    pub const fn on(&self) -> &Expr {
        &self.on
    }

    /// The alias, when the join has one.
    ///
    /// ```
    /// # use moso_orm::{JoinKind, Joined};
    /// # use moso_sql::{Expr, TableRef};
    /// let j = Joined::new(JoinKind::Inner, "T", TableRef::from_static("t"), Expr::value(true));
    /// assert!(j.alias().is_none());
    /// ```
    #[must_use]
    pub const fn alias(&self) -> Option<&Ident> {
        self.alias.as_ref()
    }
}

/// Which kind of join a relation was brought in with.
///
/// ```
/// use moso_orm::JoinKind;
///
/// assert!(JoinKind::Left.keeps_unmatched_rows());
/// assert!(!JoinKind::Inner.keeps_unmatched_rows());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JoinKind {
    /// Only rows with a match on both sides.
    Inner,
    /// Every row of this entity, matched or not.
    Left,
    /// Every row of the *joined* entity, matched or not.
    ///
    /// Almost always a sign that the query is the wrong way round; kept because
    /// the alternative is a raw query, and because a generated report sometimes
    /// really does want it.
    Right,
    /// Every row of both sides.
    Full,
}

impl JoinKind {
    /// Whether rows of the **selected** entity survive without a match.
    ///
    /// ```
    /// use moso_orm::JoinKind;
    ///
    /// assert!(JoinKind::Left.keeps_unmatched_rows());
    /// assert!(JoinKind::Full.keeps_unmatched_rows());
    /// assert!(!JoinKind::Right.keeps_unmatched_rows());
    /// ```
    #[must_use]
    pub const fn keeps_unmatched_rows(self) -> bool {
        matches!(self, Self::Left | Self::Full)
    }

    /// Whether rows of the **joined** entity survive without a match.
    ///
    /// ```
    /// use moso_orm::JoinKind;
    ///
    /// assert!(JoinKind::Right.keeps_unmatched_joined_rows());
    /// assert!(!JoinKind::Left.keeps_unmatched_joined_rows());
    /// ```
    #[must_use]
    pub const fn keeps_unmatched_joined_rows(self) -> bool {
        matches!(self, Self::Right | Self::Full)
    }

    /// The `moso-sql` join kind this renders as.
    const fn to_sql(self) -> SqlJoinKind {
        match self {
            Self::Inner => SqlJoinKind::Inner,
            Self::Left => SqlJoinKind::Left,
            Self::Right => SqlJoinKind::Right,
            Self::Full => SqlJoinKind::Full,
        }
    }
}

/// How a query treats soft-deleted rows.
///
/// ```
/// use moso_orm::Deleted;
///
/// assert_eq!(Deleted::default(), Deleted::Live);
/// assert!(Deleted::Live.adds_a_filter());
/// assert!(!Deleted::Any.adds_a_filter());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Deleted {
    /// Only rows that are not soft-deleted. The default.
    #[default]
    Live,
    /// Live and soft-deleted rows.
    Any,
    /// Only soft-deleted rows — for a restore screen or an audit.
    Only,
}

impl Deleted {
    /// Whether this mode adds a predicate on the soft-delete column.
    ///
    /// ```
    /// use moso_orm::Deleted;
    ///
    /// assert!(Deleted::Only.adds_a_filter());
    /// ```
    #[must_use]
    pub const fn adds_a_filter(self) -> bool {
        matches!(self, Self::Live | Self::Only)
    }
}

/// How strongly to lock the rows a query reads.
///
/// ```
/// use moso_orm::LockMode;
///
/// assert!(LockMode::ForUpdate.blocks_writers());
/// assert!(!LockMode::ForKeyShare.blocks_writers());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LockMode {
    /// `FOR UPDATE` — the strongest, for read-modify-write.
    ForUpdate,
    /// `FOR NO KEY UPDATE` — allows concurrent foreign-key references.
    ForNoKeyUpdate,
    /// `FOR SHARE` — blocks writers, allows readers.
    ForShare,
    /// `FOR KEY SHARE` — the weakest, blocks only key changes.
    ForKeyShare,
}

impl LockMode {
    /// Whether the lock blocks concurrent writers.
    ///
    /// ```
    /// use moso_orm::LockMode;
    ///
    /// assert!(LockMode::ForShare.blocks_writers());
    /// ```
    #[must_use]
    pub const fn blocks_writers(self) -> bool {
        !matches!(self, Self::ForKeyShare)
    }

    /// The `FOR …` clause.
    ///
    /// ```
    /// use moso_orm::LockMode;
    ///
    /// assert_eq!(LockMode::ForUpdate.as_sql(), "FOR UPDATE");
    /// ```
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::ForUpdate => "FOR UPDATE",
            Self::ForNoKeyUpdate => "FOR NO KEY UPDATE",
            Self::ForShare => "FOR SHARE",
            Self::ForKeyShare => "FOR KEY SHARE",
        }
    }

    /// The `moso-sql` lock strength this renders as.
    const fn to_sql(self) -> LockStrength {
        match self {
            Self::ForUpdate => LockStrength::Update,
            Self::ForNoKeyUpdate => LockStrength::NoKeyUpdate,
            Self::ForShare => LockStrength::Share,
            Self::ForKeyShare => LockStrength::KeyShare,
        }
    }
}

/// What a lock does when it cannot be taken.
///
/// ```
/// use moso_orm::LockBehavior;
///
/// assert_eq!(LockBehavior::default(), LockBehavior::Wait);
/// assert!(LockBehavior::SkipLocked.skips());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LockBehavior {
    /// Wait for the conflicting transaction.
    #[default]
    Wait,
    /// Leave conflicting rows out of the result — the queue-worker idiom.
    SkipLocked,
    /// Fail at once.
    NoWait,
}

impl LockBehavior {
    /// Whether conflicting rows are skipped rather than waited for.
    ///
    /// ```
    /// use moso_orm::LockBehavior;
    ///
    /// assert!(!LockBehavior::Wait.skips());
    /// ```
    #[must_use]
    pub const fn skips(self) -> bool {
        matches!(self, Self::SkipLocked)
    }

    /// The `moso-sql` lock behaviour this renders as.
    const fn to_sql(self) -> SqlLockBehavior {
        match self {
            Self::Wait => SqlLockBehavior::Wait,
            Self::SkipLocked => SqlLockBehavior::SkipLocked,
            Self::NoWait => SqlLockBehavior::NoWait,
        }
    }
}

impl<E: Entity, J> Select<E, J> {
    /// An unfiltered query over `E`.
    ///
    /// `#[derive(Entity)]` generates `E::query()`, which calls this with the
    /// right `J` for the entity.
    ///
    /// ```
    /// # use moso_orm::Select;
    /// # use moso_orm::Entity;
    /// fn everything<E: Entity>() -> Select<E> {
    ///     Select::new()
    /// }
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            filters: Vec::new(),
            joins: Vec::new(),
            order: Vec::new(),
            group_by: Vec::new(),
            having: Vec::new(),
            preloads: Vec::new(),
            limit: None,
            offset: None,
            lock: None,
            lock_behavior: LockBehavior::Wait,
            distinct: false,
            deleted: Deleted::Live,
            tenant: None,
            across_tenants: false,
            unlimited: false,
            entity: PhantomData,
            scope: PhantomData,
        }
    }

    /// Adds a filter. Repeated calls are `AND`ed.
    ///
    /// Captures the caller's location, so an out-of-scope column is reported
    /// against this line.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Select};
    /// fn positive<E: Entity>(query: Select<E>, id: Column<E, i64>) -> Select<E> {
    ///     query.filter(id.gt(0))
    /// }
    /// ```
    #[must_use]
    #[track_caller]
    pub fn filter(mut self, predicate: impl Into<Predicate>) -> Self {
        self.filters.push(Filter::new(predicate.into()));
        self
    }

    /// Adds a filter only when there is one.
    ///
    /// The single most-used helper in a search endpoint, and the reason
    /// dynamic queries need no type gymnastics (non-negotiable N4).
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Predicate, Select};
    /// fn maybe<E: Entity>(query: Select<E>, id: Column<E, i64>, wanted: Option<i64>) -> Select<E> {
    ///     query.filter_opt(wanted.map(|value| id.eq(value)))
    /// }
    /// ```
    #[must_use]
    #[track_caller]
    pub fn filter_opt(self, predicate: Option<impl Into<Predicate>>) -> Self {
        match predicate {
            Some(predicate) => self.filter(predicate),
            None => self,
        }
    }

    /// Adds a filter only when `condition` holds.
    ///
    /// The closure means the predicate is not built when it is not used.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Select};
    /// fn in_stock<E: Entity>(query: Select<E>, stock: Column<E, i64>, only: bool) -> Select<E> {
    ///     query.filter_if(only, || stock.gt(0))
    /// }
    /// ```
    #[must_use]
    #[track_caller]
    pub fn filter_if<P: Into<Predicate>>(
        self,
        condition: bool,
        predicate: impl FnOnce() -> P,
    ) -> Self {
        if condition {
            return self.filter(predicate());
        }
        self
    }

    /// Applies `transform` only when `condition` holds.
    ///
    /// For the conditional changes that are not filters — a join, an order, a
    /// limit.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn maybe_limited<E: Entity>(query: Select<E>, capped: bool) -> Select<E> {
    ///     query.when(capped, |q| q.limit(50))
    /// }
    /// ```
    #[must_use]
    pub fn when(self, condition: bool, transform: impl FnOnce(Self) -> Self) -> Self {
        if condition {
            return transform(self);
        }
        self
    }

    /// Applies `transform` unconditionally.
    ///
    /// The composition point for a reusable scope written as a free function.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn recent<E: Entity>(query: Select<E>) -> Select<E> {
    ///     query.apply(|q| q.limit(10))
    /// }
    /// ```
    #[must_use]
    pub fn apply(self, transform: impl FnOnce(Self) -> Self) -> Self {
        transform(self)
    }

    /// Adds an ordering term. Repeated calls order by each in turn.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Select};
    /// fn newest<E: Entity>(query: Select<E>, created: Column<E, i64>) -> Select<E> {
    ///     query.order_by(created.desc())
    /// }
    /// ```
    #[must_use]
    pub fn order_by(mut self, term: OrderTerm) -> Self {
        self.order.push(term);
        self
    }

    /// Adds an ordering term only when there is one.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// # use moso_sql::OrderTerm;
    /// fn sorted<E: Entity>(query: Select<E>, term: Option<OrderTerm>) -> Select<E> {
    ///     query.order_by_opt(term)
    /// }
    /// ```
    #[must_use]
    pub fn order_by_opt(self, term: Option<OrderTerm>) -> Self {
        match term {
            Some(term) => self.order_by(term),
            None => self,
        }
    }

    /// Adds an ordering term, putting `NULL`s last whichever direction it
    /// sorts.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Select};
    /// fn newest<E: Entity>(query: Select<E>, published: Column<E, Option<i64>>) -> Select<E> {
    ///     query.order_by_nulls_last(published.desc())
    /// }
    /// ```
    #[must_use]
    pub fn order_by_nulls_last(self, term: OrderTerm) -> Self {
        self.order_by(term.nulls_last())
    }

    /// Adds an ordering term, putting `NULL`s first whichever direction it
    /// sorts.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Select};
    /// fn unfinished_first<E: Entity>(q: Select<E>, done: Column<E, Option<i64>>) -> Select<E> {
    ///     q.order_by_nulls_first(done.asc())
    /// }
    /// ```
    #[must_use]
    pub fn order_by_nulls_first(self, term: OrderTerm) -> Self {
        self.order_by(term.nulls_first())
    }

    /// Forgets every ordering term added so far.
    ///
    /// What a `count(*)` over a sorted query needs, and what a scope that
    /// re-sorts uses.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn unsorted<E: Entity>(query: Select<E>) -> Select<E> {
    ///     query.clear_order()
    /// }
    /// ```
    #[must_use]
    pub fn clear_order(mut self) -> Self {
        self.order.clear();
        self
    }

    /// Groups by an expression.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Select};
    /// fn per_author<E: Entity>(query: Select<E>, author: Column<E, i64>) -> Select<E> {
    ///     query.group_by(author.expr())
    /// }
    /// ```
    #[must_use]
    pub fn group_by(mut self, expr: Expr) -> Self {
        self.group_by.push(expr);
        self
    }

    /// Filters groups.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Select};
    /// # use moso_sql::Expr;
    /// fn prolific<E: Entity>(query: Select<E>, id: Column<E, i64>) -> Select<E> {
    ///     query.having(id.count().gt(Expr::value(5_i64)))
    /// }
    /// ```
    #[must_use]
    pub fn having(mut self, expr: Expr) -> Self {
        self.having.push(expr);
        self
    }

    /// Caps the number of rows.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn first_ten<E: Entity>(query: Select<E>) -> Select<E> {
    ///     query.limit(10)
    /// }
    /// ```
    #[must_use]
    pub const fn limit(mut self, rows: u64) -> Self {
        self.limit = Some(rows);
        self
    }

    /// Skips rows.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn second_page<E: Entity>(query: Select<E>) -> Select<E> {
    ///     query.limit(10).offset(10)
    /// }
    /// ```
    #[must_use]
    pub const fn offset(mut self, rows: u64) -> Self {
        self.offset = Some(rows);
        self
    }

    /// Opts out of the default row cap on [`Select::fetch_all`].
    ///
    /// Without an explicit `.limit(..)`, `fetch_all` applies
    /// [`default_row_limit`] (10 000) and warns when it is reached. This says
    /// "I know, and I want every row" — an export, a migration backfill, a
    /// nightly job. It has no effect on a query that also sets `.limit(..)`,
    /// because an explicit limit already answers the question.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn every_single_row<E: Entity>(query: Select<E>) -> Select<E> {
    ///     query.unlimited()
    /// }
    /// ```
    #[must_use]
    pub const fn unlimited(mut self) -> Self {
        self.unlimited = true;
        self
    }

    /// Whether the default row cap has been opted out of.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn uncapped<E: Entity>(query: &Select<E>) -> bool {
    ///     query.is_unlimited()
    /// }
    /// ```
    #[must_use]
    pub const fn is_unlimited(&self) -> bool {
        self.unlimited
    }

    /// Applies a named, reusable [`Scope`].
    ///
    /// The value-carrying sibling of [`Select::apply`]: use it when the scope
    /// is chosen at run time, stored in a struct, or handed across an API
    /// boundary.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// # use moso_orm::scope::Scope;
    /// fn scoped<E: Entity>(query: Select<E>, scope: &Scope<E>) -> Select<E> {
    ///     query.with_scope(scope)
    /// }
    /// ```
    #[must_use]
    pub fn with_scope(self, scope: &Scope<E, J>) -> Self
    where
        J: 'static,
    {
        scope.apply(self)
    }

    /// Adds `DISTINCT`.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn unique<E: Entity>(query: Select<E>) -> Select<E> {
    ///     query.distinct()
    /// }
    /// ```
    #[must_use]
    pub const fn distinct(mut self) -> Self {
        self.distinct = true;
        self
    }

    /// Locks the rows this query reads.
    ///
    /// ```
    /// # use moso_orm::{Entity, LockMode, Select};
    /// fn locked<E: Entity>(query: Select<E>) -> Select<E> {
    ///     query.lock(LockMode::ForUpdate)
    /// }
    /// ```
    #[must_use]
    pub const fn lock(mut self, mode: LockMode) -> Self {
        self.lock = Some(mode);
        self
    }

    /// Locks the rows, saying what to do when a row is already locked.
    ///
    /// `LockBehavior::SkipLocked` is the queue-worker idiom: take the rows
    /// nobody else is working on, rather than waiting behind them.
    ///
    /// ```
    /// # use moso_orm::{Entity, LockBehavior, LockMode, Select};
    /// fn claim<E: Entity>(query: Select<E>) -> Select<E> {
    ///     query.lock_with(LockMode::ForUpdate, LockBehavior::SkipLocked)
    /// }
    /// ```
    #[must_use]
    pub const fn lock_with(mut self, mode: LockMode, behavior: LockBehavior) -> Self {
        self.lock = Some(mode);
        self.lock_behavior = behavior;
        self
    }

    /// Includes soft-deleted rows.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn everything<E: Entity>(query: Select<E>) -> Select<E> {
    ///     query.with_deleted()
    /// }
    /// ```
    #[must_use]
    pub const fn with_deleted(mut self) -> Self {
        self.deleted = Deleted::Any;
        self
    }

    /// Returns *only* soft-deleted rows.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn recycle_bin<E: Entity>(query: Select<E>) -> Select<E> {
    ///     query.only_deleted()
    /// }
    /// ```
    #[must_use]
    pub const fn only_deleted(mut self) -> Self {
        self.deleted = Deleted::Only;
        self
    }

    /// Joins a relation, bringing the related entity's columns into scope for
    /// `filter` and `order_by`.
    ///
    /// A join does **not** load the related rows. `.with(..)` does that, and
    /// keeping them separate is the distinction Rails conflates.
    ///
    /// The type does not change, so a conditional join is an ordinary `if`.
    ///
    /// ```
    /// # use moso_orm::{Entity, Relation, Select};
    /// fn joined<E: Entity, R: Relation<E>>(query: Select<E>, relation: R) -> Select<E> {
    ///     query.join(relation)
    /// }
    /// ```
    #[must_use]
    pub fn join<R: Relation<E>>(mut self, relation: R) -> Self {
        self.joins.push(relation.join(JoinKind::Inner));
        self
    }

    /// Joins a relation with a `LEFT JOIN`, keeping rows without a match.
    ///
    /// ```
    /// # use moso_orm::{Entity, Relation, Select};
    /// fn left<E: Entity, R: Relation<E>>(query: Select<E>, relation: R) -> Select<E> {
    ///     query.left_join(relation)
    /// }
    /// ```
    #[must_use]
    pub fn left_join<R: Relation<E>>(mut self, relation: R) -> Self {
        self.joins.push(relation.join(JoinKind::Left));
        self
    }

    /// Joins a relation with a `RIGHT JOIN`, keeping the *related* rows that
    /// have no match.
    ///
    /// SQLite has supported this since 3.39, below the 3.40 floor
    /// `20-orm-overview.md` sets, so it is portable.
    ///
    /// ```
    /// # use moso_orm::{Entity, Relation, Select};
    /// fn right<E: Entity, R: Relation<E>>(query: Select<E>, relation: R) -> Select<E> {
    ///     query.right_join(relation)
    /// }
    /// ```
    #[must_use]
    pub fn right_join<R: Relation<E>>(mut self, relation: R) -> Self {
        self.joins.push(relation.join(JoinKind::Right));
        self
    }

    /// Joins a relation with a `FULL OUTER JOIN`, keeping unmatched rows on
    /// both sides.
    ///
    /// ```
    /// # use moso_orm::{Entity, Relation, Select};
    /// fn full<E: Entity, R: Relation<E>>(query: Select<E>, relation: R) -> Select<E> {
    ///     query.full_join(relation)
    /// }
    /// ```
    #[must_use]
    pub fn full_join<R: Relation<E>>(mut self, relation: R) -> Self {
        self.joins.push(relation.join(JoinKind::Full));
        self
    }

    /// Joins a relation with an explicitly chosen kind.
    ///
    /// The shape a runtime decision needs: the kind is a value, so `match`ing
    /// on a request parameter to pick one is an ordinary `match`.
    ///
    /// ```
    /// # use moso_orm::{Entity, JoinKind, Relation, Select};
    /// fn either<E: Entity, R: Relation<E>>(q: Select<E>, r: R, outer: bool) -> Select<E> {
    ///     q.join_with(if outer { JoinKind::Left } else { JoinKind::Inner }, r)
    /// }
    /// ```
    #[must_use]
    pub fn join_with<R: Relation<E>>(mut self, kind: JoinKind, relation: R) -> Self {
        self.joins.push(relation.join(kind));
        self
    }

    /// Joins only when `condition` holds.
    ///
    /// This method is the reason the joined set is not a type parameter: with
    /// one, it could not exist.
    ///
    /// ```
    /// # use moso_orm::{Entity, Relation, Select};
    /// fn maybe<E: Entity, R: Relation<E>>(q: Select<E>, r: R, wanted: bool) -> Select<E> {
    ///     q.join_if(wanted, r)
    /// }
    /// ```
    #[must_use]
    pub fn join_if<R: Relation<E>>(self, condition: bool, relation: R) -> Self {
        if condition {
            return self.join(relation);
        }
        self
    }

    /// Joins only when there is a relation to join.
    ///
    /// ```
    /// # use moso_orm::{Entity, Relation, Select};
    /// fn maybe<E: Entity, R: Relation<E>>(q: Select<E>, r: Option<R>) -> Select<E> {
    ///     q.join_opt(r)
    /// }
    /// ```
    #[must_use]
    pub fn join_opt<R: Relation<E>>(self, relation: Option<R>) -> Self {
        match relation {
            Some(relation) => self.join(relation),
            None => self,
        }
    }

    /// Preloads a relation: one extra statement, whatever the row count
    /// (non-negotiable N3).
    ///
    /// ```
    /// # use moso_orm::{Entity, Preload, Select};
    /// fn eager<E: Entity>(query: Select<E>, preload: Preload) -> Select<E> {
    ///     query.with(preload)
    /// }
    /// ```
    #[must_use]
    pub fn with(mut self, preload: impl Into<Preload>) -> Self {
        self.preloads.push(preload.into());
        self
    }

    /// Preloads only when there is something to preload.
    ///
    /// ```
    /// # use moso_orm::{Entity, Preload, Select};
    /// fn maybe<E: Entity>(query: Select<E>, preload: Option<Preload>) -> Select<E> {
    ///     query.with_opt(preload)
    /// }
    /// ```
    #[must_use]
    pub fn with_opt(self, preload: Option<impl Into<Preload>>) -> Self {
        match preload {
            Some(preload) => self.with(preload),
            None => self,
        }
    }

    /// Adds a scalar count of a relation without loading its rows.
    ///
    /// ```
    /// # use moso_orm::{Entity, Relation, Select};
    /// fn counted<E: Entity, R: Relation<E>>(query: Select<E>, relation: R) -> Select<E> {
    ///     query.with_count(relation)
    /// }
    /// ```
    #[must_use]
    pub fn with_count<R: Relation<E>>(mut self, relation: R) -> Self {
        self.preloads.push(relation.count_preload());
        self
    }

    /// The accumulated filters.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn how_many<E: Entity>(query: &Select<E>) -> usize {
    ///     query.filters().len()
    /// }
    /// ```
    #[must_use]
    pub fn filters(&self) -> &[Filter] {
        &self.filters
    }

    /// The accumulated joins.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn how_many<E: Entity>(query: &Select<E>) -> usize {
    ///     query.joins().len()
    /// }
    /// ```
    #[must_use]
    pub fn joins(&self) -> &[Joined] {
        &self.joins
    }

    /// The accumulated ordering terms.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn how_many<E: Entity>(query: &Select<E>) -> usize {
    ///     query.order_terms().len()
    /// }
    /// ```
    #[must_use]
    pub fn order_terms(&self) -> &[OrderTerm] {
        &self.order
    }

    /// The accumulated preloads.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn how_many<E: Entity>(query: &Select<E>) -> usize {
    ///     query.preloads().len()
    /// }
    /// ```
    #[must_use]
    pub fn preloads(&self) -> &[Preload] {
        &self.preloads
    }

    /// How this query treats soft-deleted rows.
    ///
    /// ```
    /// # use moso_orm::{Deleted, Entity, Select};
    /// fn mode<E: Entity>(query: &Select<E>) -> Deleted {
    ///     query.deleted()
    /// }
    /// ```
    #[must_use]
    pub const fn deleted(&self) -> Deleted {
        self.deleted
    }

    /// The row limit, when one was set.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn capped<E: Entity>(query: &Select<E>) -> bool {
    ///     query.limit_value().is_some()
    /// }
    /// ```
    #[must_use]
    pub const fn limit_value(&self) -> Option<u64> {
        self.limit
    }

    /// The offset, when one was set.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn skipped<E: Entity>(query: &Select<E>) -> u64 {
    ///     query.offset_value().unwrap_or(0)
    /// }
    /// ```
    #[must_use]
    pub const fn offset_value(&self) -> Option<u64> {
        self.offset
    }

    /// The lock, when one was taken.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn locked<E: Entity>(query: &Select<E>) -> bool {
    ///     query.lock_mode().is_some()
    /// }
    /// ```
    #[must_use]
    pub const fn lock_mode(&self) -> Option<LockMode> {
        self.lock
    }

    /// What the lock does when a row is already locked.
    ///
    /// ```
    /// # use moso_orm::{Entity, LockBehavior, Select};
    /// fn behaviour<E: Entity>(query: &Select<E>) -> LockBehavior {
    ///     query.lock_behavior()
    /// }
    /// ```
    #[must_use]
    pub const fn lock_behavior(&self) -> LockBehavior {
        self.lock_behavior
    }

    /// Every entity whose columns this query may mention: `E`, plus each
    /// joined entity.
    ///
    /// This is the scope the build-time check compares a filter against.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn scope<E: Entity>(query: &Select<E>) -> Vec<&'static str> {
    ///     query.scope()
    /// }
    /// ```
    #[must_use]
    pub fn scope(&self) -> Vec<&'static str> {
        let mut scope = Vec::with_capacity(self.joins.len() + 1);
        scope.push(E::NAME);
        for join in &self.joins {
            if !scope.contains(&join.entity()) {
                scope.push(join.entity());
            }
        }
        scope
    }

    /// Checks every filter against the query's scope.
    ///
    /// **This is the joined-set check.** It runs before the statement is
    /// rendered, so an out-of-scope column never reaches the server, and the
    /// error names the entity, the offending line and both fixes.
    ///
    /// # Errors
    ///
    /// [`Error::Unjoined`] for the first filter that mentions an entity the
    /// query does not select from and has not joined.
    ///
    /// ```
    /// # use moso_orm::{Entity, Result, Select};
    /// fn ok<E: Entity>(query: &Select<E>) -> Result<()> {
    ///     query.check_scope()
    /// }
    /// ```
    pub fn check_scope(&self) -> Result<()> {
        let scope = self.scope();
        for filter in &self.filters {
            let Some(missing) = filter.predicate().missing_from(&scope) else {
                continue;
            };
            let mut error = Unjoined::new(E::NAME, table_name::<E>(), missing, missing.to_owned())
                .with_joined(scope.iter().skip(1).copied())
                .at(filter.call_site());
            if let Some(relation) = E::descriptor()
                .relations()
                .iter()
                .find(|relation| relation.target() == missing)
            {
                let _ = relation;
                error = error.with_relation("the relation constant for this entity");
            }
            return Err(Error::Unjoined(Box::new(error)));
        }
        Ok(())
    }

    /// Renders the query as a `moso-sql` statement.
    ///
    /// # Errors
    ///
    /// [`Error::Unjoined`] from [`Select::check_scope`], and
    /// [`Error::TenantMissing`] when a tenant-scoped entity has no tenant.
    ///
    /// ```
    /// # use moso_orm::{Entity, Result, Select};
    /// # use moso_sql::Statement;
    /// fn statement<E: Entity>(query: &Select<E>) -> Result<Statement> {
    ///     query.to_statement()
    /// }
    /// ```
    pub fn to_statement(&self) -> Result<Statement> {
        Ok(self.build(Shape::Entity)?.into_statement())
    }

    /// Checks that a tenant-scoped entity has a tenant.
    ///
    /// The compile-time half of this is [`Ready`]; this is the half that
    /// catches a query nothing ever gave a scope obligation to — one the admin
    /// assembled from a runtime description, or one written with an explicit
    /// `Select<Invoice, ()>`.
    ///
    /// # Errors
    ///
    /// [`Error::TenantMissing`].
    ///
    /// ```
    /// # use moso_orm::{Entity, Result, Select};
    /// fn ok<E: Entity>(query: &Select<E>) -> Result<()> {
    ///     query.check_tenant()
    /// }
    /// ```
    pub fn check_tenant(&self) -> Result<()> {
        if requires_tenant::<E>() && self.tenant.is_none() && !self.across_tenants {
            return Err(Error::TenantMissing { entity: E::NAME });
        }
        Ok(())
    }

    /// Renders the query as a `SELECT 1 …` suitable for an `EXISTS`.
    ///
    /// Used by [`crate::expr::exists`]; the ordering and the preloads are
    /// dropped, because neither changes whether a row is there.
    pub(crate) fn to_subquery(&self) -> Result<SqlSelect> {
        let mut base = self.clone();
        base.order.clear();
        base.preloads.clear();
        base.build(Shape::Expr(Expr::value(1_i32)))
    }

    /// Renders the query projecting exactly one column, for `IN (…)` and for a
    /// scalar subquery.
    pub(crate) fn to_column_subquery<T>(&self, column: Column<E, T>) -> Result<SqlSelect> {
        let mut base = self.clone();
        base.preloads.clear();
        base.build(Shape::Expr(column.expr()))
    }

    /// Assembles the `moso-sql` query, with `shape` deciding the projection.
    ///
    /// Everything that renders a `SELECT` goes through here — the entity fetch,
    /// the tuple and struct projections, `count`, `exists` and the subqueries —
    /// so the soft-delete predicate, the tenant predicate and the scope check
    /// cannot be forgotten by one of them.
    fn build(&self, shape: Shape) -> Result<SqlSelect> {
        self.check_scope()?;
        self.check_tenant()?;

        let mut query = SqlSelect::from_table(E::TABLE);

        query = match shape {
            Shape::Entity => {
                let table = E::TABLE.name().clone();
                query.select_items(E::COLUMNS.iter().map(|column| {
                    SelectItem::column(ColumnRef::qualified(table.clone(), column.ident()))
                }))
            }
            Shape::Items(items) => query.select_items(items),
            Shape::Expr(expr) => query.select_expr(expr),
        };

        if self.distinct {
            query = query.distinct();
        }

        for join in &self.joins {
            let source = match join.alias() {
                Some(alias) => FromItem::table_as(join.table().clone(), alias.clone()),
                None => FromItem::table(join.table().clone()),
            };
            query = query.join(SqlJoin::new(
                join.kind().to_sql(),
                source,
                join.on().clone(),
            ));
        }

        // The framework's predicates go first, so a rendered statement reads
        // `WHERE deleted_at IS NULL AND tenant_id = $1 AND …` and the two
        // invariants are visible at the front of every `EXPLAIN`.
        query =
            query.filter_opt(soft_delete_predicate::<E>(self.deleted).map(Predicate::into_expr));
        if let Some(tenant) = self.tenant.as_ref() {
            query = query.filter_opt(tenant_predicate::<E>(tenant).map(Predicate::into_expr));
        }
        for filter in &self.filters {
            query = query.filter(filter.predicate().expr().clone());
        }

        for expr in &self.group_by {
            query = query.group_by(expr.clone());
        }
        for expr in &self.having {
            query = query.having(expr.clone());
        }
        for term in &self.order {
            query = query.order_by(pin_null_order(term.clone()));
        }

        if let Some(rows) = self.limit {
            query = query.limit(rows);
        }
        if let Some(rows) = self.offset {
            query = query.offset(rows);
        }

        if let Some(mode) = self.lock {
            let lock = Lock::new(mode.to_sql());
            query = query.lock(match self.lock_behavior.to_sql() {
                SqlLockBehavior::SkipLocked => lock.skip_locked(),
                SqlLockBehavior::NoWait => lock.nowait(),
                _ => lock,
            });
        }

        Ok(query)
    }
}

/// What the rendered `SELECT` puts between `SELECT` and `FROM`.
enum Shape {
    /// Every column of `E`, in `E::COLUMNS` order and qualified by the table.
    ///
    /// The order is what makes `Entity::from_row` positional: it decodes by
    /// index, never by name, so there is no per-column string hashing
    /// (`21-entities-queries.md`, acceptance criterion 8).
    Entity,
    /// A caller-supplied list — a tuple select or a `#[derive(Projection)]`.
    Items(Vec<SelectItem>),
    /// One expression: `count(*)`, the `1` of an `EXISTS`, or a subquery's
    /// single column.
    Expr(Expr),
}

/// Gives an ordering term an explicit `NULLS` placement when it has none.
///
/// PostgreSQL sorts `NULL`s last ascending and first descending; SQLite does
/// the opposite. A query that reads the same in both dialects therefore has to
/// say which it means, and the one Moso picks is PostgreSQL's — the reference
/// dialect (ADR-0010). Keyset pagination depends on this: a cursor built
/// against one null placement cannot be walked with the other.
fn pin_null_order(term: OrderTerm) -> OrderTerm {
    if term.nulls().is_some() {
        return term;
    }
    match term.order() {
        Order::Desc => term.nulls_first(),
        _ => term.nulls_last(),
    }
}

/// The table name of `E`, as a `&'static str`, for a diagnostic.
///
/// `TableRef` owns its identifier, so this leaks nothing: the entity's name is
/// used when the table name is not `'static`, which keeps the message useful
/// without an allocation that outlives the error.
fn table_name<E: Entity>() -> &'static str {
    E::NAME
}

impl<E: Entity, J: Ready<E>> Select<E, J> {
    /// Runs the query and collects every row.
    ///
    /// # The row cap
    ///
    /// A query with no `.limit(..)` and no [`Select::unlimited`] is capped at
    /// [`default_row_limit`] rows, and logs a `warn` naming the entity when the
    /// cap is reached. This is deliberate paternalism, argued in
    /// `04-devex/45-security.md`: the unbounded `SELECT *` that works on a dev
    /// database and exhausts production memory is a failure mode worth
    /// designing out. Say `.unlimited()` when you mean it.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Result, Select};
    /// async fn all<E: Entity>(query: Select<E>, executor: impl Executor<'_>) -> Result<Vec<E>> {
    ///     query.fetch_all(executor).await
    /// }
    /// ```
    pub async fn fetch_all(mut self, executor: impl Executor<'_>) -> Result<Vec<E>> {
        let cap = self.apply_row_cap();
        let statement = self.to_statement()?;
        let handle = executor.handle();
        let rows = handle.fetch_all(&statement).await?;
        warn_if_capped::<E>(cap, rows.len(), self.filters.first().map(Filter::call_site));
        let mut entities = Vec::with_capacity(rows.len());
        for row in &rows {
            entities.push(E::from_row(row)?);
        }
        self.run_preloads(&mut entities, handle).await?;
        Ok(entities)
    }

    /// Runs the query and returns the single row it must produce.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when nothing matched, plus anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Result, Select};
    /// async fn one<E: Entity>(query: Select<E>, executor: impl Executor<'_>) -> Result<E> {
    ///     query.fetch_one(executor).await
    /// }
    /// ```
    pub async fn fetch_one(self, executor: impl Executor<'_>) -> Result<E> {
        self.fetch_optional(executor)
            .await?
            .ok_or(Error::NotFound { entity: E::NAME })
    }

    /// Runs the query and returns the first row, if there is one.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Result, Select};
    /// async fn maybe<E: Entity>(query: Select<E>, ex: impl Executor<'_>) -> Result<Option<E>> {
    ///     query.fetch_optional(ex).await
    /// }
    /// ```
    pub async fn fetch_optional(self, executor: impl Executor<'_>) -> Result<Option<E>> {
        let statement = self.to_statement()?;
        let handle = executor.handle();
        let Some(row) = handle.fetch_optional(&statement).await? else {
            return Ok(None);
        };
        let mut entities = vec![E::from_row(&row)?];
        self.run_preloads(&mut entities, handle).await?;
        Ok(entities.pop())
    }

    /// Runs the query with `LIMIT 1` and returns the first row.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Result, Select};
    /// async fn first<E: Entity>(query: Select<E>, ex: impl Executor<'_>) -> Result<Option<E>> {
    ///     query.fetch_first(ex).await
    /// }
    /// ```
    pub async fn fetch_first(self, executor: impl Executor<'_>) -> Result<Option<E>> {
        self.limit(1).fetch_optional(executor).await
    }

    /// Streams the rows without buffering them.
    ///
    /// Preloads are not applied to a stream: batching needs the whole parent
    /// set, and pretending otherwise would reintroduce N+1.
    ///
    /// # Errors
    ///
    /// [`Error::Unjoined`] or [`Error::Build`] from rendering the statement.
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, EntityStream, Executor, Result, Select};
    /// fn stream<'e, E: Entity>(q: Select<E>, ex: impl Executor<'e>) -> Result<EntityStream<'e, E>> {
    ///     q.fetch_stream(ex)
    /// }
    /// ```
    pub fn fetch_stream<'e>(self, executor: impl Executor<'e>) -> Result<EntityStream<'e, E>> {
        let statement = self.to_statement()?;
        Ok(EntityStream {
            rows: executor.handle().fetch_stream(statement),
            entity: PhantomData,
        })
    }

    /// Counts the matching rows.
    ///
    /// Drops the ordering, the limit and the preloads, because none of them
    /// changes a count and all of them cost.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Result, Select};
    /// async fn how_many<E: Entity>(query: Select<E>, ex: impl Executor<'_>) -> Result<u64> {
    ///     query.count(ex).await
    /// }
    /// ```
    pub async fn count(self, executor: impl Executor<'_>) -> Result<u64> {
        let statement = self.to_count_statement()?;
        let Some(row) = executor.handle().fetch_optional(&statement).await? else {
            return Ok(0);
        };
        Ok(u64::try_from(row.get_i64(0)?).unwrap_or(0))
    }

    /// Whether any row matches.
    ///
    /// Renders as `SELECT EXISTS (SELECT 1 FROM … LIMIT 1)`, which stops at the
    /// first match rather than counting them all.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Result, Select};
    /// async fn any<E: Entity>(query: Select<E>, ex: impl Executor<'_>) -> Result<bool> {
    ///     query.exists(ex).await
    /// }
    /// ```
    pub async fn exists(self, executor: impl Executor<'_>) -> Result<bool> {
        let statement = self.to_exists_statement()?;
        let Some(row) = executor.handle().fetch_optional(&statement).await? else {
            return Ok(false);
        };
        Ok(row.get_bool(0)?)
    }

    /// Runs every preload against the rows just fetched.
    ///
    /// One statement per relation, whatever the row count — the mechanism
    /// behind non-negotiable N3, which lives in
    /// [`relation::run_preloads`](crate::relation::run_preloads) because the
    /// key collection, the deduplication and the grouping are relation
    /// knowledge, not query-builder knowledge.
    async fn run_preloads(
        &self,
        entities: &mut [E],
        handle: crate::executor::Handle<'_>,
    ) -> Result<()> {
        if self.preloads.is_empty() || entities.is_empty() {
            return Ok(());
        }
        crate::relation::run_preloads(&self.preloads, entities, handle).await
    }

    /// The `count(*)` form of this query.
    ///
    /// Ordering and preloads are dropped, because neither changes a count and
    /// both cost. A `DISTINCT` or a `GROUP BY` counts the rows the query would
    /// have returned, which needs a subquery — `count(*)` over a grouped query
    /// counts rows *per group*, and that is not what anyone means by "how many
    /// results are there".
    fn to_count_statement(&self) -> Result<Statement> {
        let mut base = self.clone();
        base.order.clear();
        base.preloads.clear();

        if base.group_by.is_empty() && !base.distinct {
            base.limit = None;
            base.offset = None;
            return Ok(base.build(Shape::Expr(count_star()))?.into_statement());
        }

        let inner = base.build(Shape::Entity)?;
        Ok(SqlSelect::new()
            .from(FromItem::subquery(
                inner,
                Ident::from_static("moso_counted"),
            ))
            .select_expr(count_star())
            .into_statement())
    }

    /// The `select exists (select 1 from … limit 1)` form of this query.
    fn to_exists_statement(&self) -> Result<Statement> {
        let mut base = self.clone();
        base.limit = Some(base.limit.map_or(1, |rows| rows.min(1)));
        let inner = base.to_subquery()?;
        Ok(SqlSelect::new()
            .select_expr(Expr::exists(inner))
            .into_statement())
    }

    /// Applies the default row cap, returning it when one was applied.
    fn apply_row_cap(&mut self) -> Option<u64> {
        if self.limit.is_some() || self.unlimited {
            return None;
        }
        let cap = default_row_limit();
        if cap == 0 {
            return None;
        }
        self.limit = Some(cap);
        Some(cap)
    }
}

/// Warns when a `fetch_all` returned exactly as many rows as the cap allowed,
/// which is the only observable sign that rows were dropped.
fn warn_if_capped<E: Entity>(cap: Option<u64>, returned: usize, at: Option<CallSite>) {
    let Some(cap) = cap else {
        return;
    };
    if (returned as u64) < cap {
        return;
    }
    let site = at.map_or_else(|| String::from("unknown"), |site| site.to_string());
    let table = E::TABLE;
    tracing::warn!(
        entity = E::NAME,
        table = table.name().as_str(),
        cap,
        at = %site,
        "`fetch_all` hit the default row cap and may have dropped rows; add `.limit(..)` to say \
         how many you want, `.paginate(..)` to page through them, or `.unlimited()` to take all \
         of them on purpose"
    );
}

impl<E: Entity, J> Select<E, J> {
    /// Projects a tuple of columns, so the query decodes into a tuple of their
    /// Rust types (non-negotiable N5).
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Projected, Select};
    /// fn ids_and_names<E: Entity>(
    ///     query: Select<E>,
    ///     id: Column<E, i64>,
    ///     name: Column<E, String>,
    /// ) -> Projected<E, (i64, String)> {
    ///     query.select((id, name))
    /// }
    /// ```
    #[must_use]
    pub fn select<C: ColumnTuple>(self, columns: C) -> Projected<E, C::Output, J> {
        Projected {
            select: self,
            items: columns.items(),
            decode: C::decode,
        }
    }

    /// Projects into a `#[derive(Projection)]` struct.
    ///
    /// ```
    /// # use moso_orm::{Entity, Projected, Projection, Select};
    /// fn summarised<E: Entity, P: Projection>(query: Select<E>) -> Projected<E, P> {
    ///     query.project::<P>()
    /// }
    /// ```
    #[must_use]
    pub fn project<P: Projection>(self) -> Projected<E, P, J> {
        Projected {
            select: self,
            items: P::select_items(),
            decode: P::from_row,
        }
    }

    /// Turns the query into a bulk `UPDATE` with the same filters.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select, Update};
    /// fn as_update<E: Entity>(query: Select<E>) -> Update<E> {
    ///     query.update()
    /// }
    /// ```
    #[must_use]
    pub fn update(self) -> Update<E> {
        Update::from_filters(self.filters)
    }

    /// Turns the query into a bulk `DELETE` with the same filters.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity, Select};
    /// fn as_delete<E: Entity>(query: Select<E>) -> Delete<E> {
    ///     query.delete()
    /// }
    /// ```
    #[must_use]
    pub fn delete(self) -> Delete<E> {
        Delete::from_filters(self.filters)
    }

    /// Keyset pagination. The primary key is appended as a tiebreaker, so the
    /// order is always total.
    ///
    /// ```
    /// # use moso_orm::{Entity, Paginated, Select};
    /// fn page<E: Entity>(query: Select<E>, limit: u32) -> Paginated<E> {
    ///     query.paginate(None, limit)
    /// }
    /// ```
    #[must_use]
    pub fn paginate(self, cursor: Option<Cursor>, limit: u32) -> Paginated<E, J> {
        Paginated::new(self, cursor, limit)
    }

    /// Offset pagination, for an admin screen that needs page numbers.
    ///
    /// ```
    /// # use moso_orm::{Entity, OffsetPaginated, Select};
    /// fn page<E: Entity>(query: Select<E>) -> OffsetPaginated<E> {
    ///     query.paginate_offset(1, 25)
    /// }
    /// ```
    #[must_use]
    pub fn paginate_offset(self, page: u32, per_page: u32) -> OffsetPaginated<E, J> {
        OffsetPaginated::new(self, page, per_page)
    }
}

impl<E: Entity, J> Select<E, J> {
    /// Names the tenant, discharging the obligation.
    ///
    /// This is the one place `J` changes.
    ///
    /// ```
    /// # use moso_orm::{Entity, NeedsTenant, Select, TenantId};
    /// fn scoped<E: Entity>(query: Select<E, NeedsTenant>, tenant: TenantId) -> Select<E> {
    ///     query.scoped(tenant)
    /// }
    /// ```
    #[must_use]
    pub fn scoped(self, tenant: TenantId) -> Select<E, ()> {
        Select {
            tenant: Some(tenant),
            ..self.rescope()
        }
    }

    /// Queries every tenant's rows, on purpose.
    ///
    /// Deliberately long to type and easy to grep for.
    ///
    /// ```
    /// # use moso_orm::{Entity, NeedsTenant, Select};
    /// fn everyone<E: Entity>(query: Select<E, NeedsTenant>) -> Select<E> {
    ///     query.across_tenants()
    /// }
    /// ```
    #[must_use]
    pub fn across_tenants(self) -> Select<E, ()> {
        Select {
            across_tenants: true,
            ..self.rescope()
        }
    }

    /// Moves every accumulated clause into a query with a different obligation.
    fn rescope<K>(self) -> Select<E, K> {
        Select {
            filters: self.filters,
            joins: self.joins,
            order: self.order,
            group_by: self.group_by,
            having: self.having,
            preloads: self.preloads,
            limit: self.limit,
            offset: self.offset,
            lock: self.lock,
            lock_behavior: self.lock_behavior,
            distinct: self.distinct,
            deleted: self.deleted,
            tenant: self.tenant,
            across_tenants: self.across_tenants,
            unlimited: self.unlimited,
            entity: PhantomData,
            scope: PhantomData,
        }
    }
}

impl<E: Entity, J> Select<E, J> {
    /// The query for one primary key.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn by_key<E: Entity>(key: E::Pk) -> Select<E> {
    ///     Select::find(key)
    /// }
    /// ```
    #[must_use]
    pub fn find(key: E::Pk) -> Self {
        let query = Self::new();
        let Some(primary) = E::COLUMNS.iter().find(|column| column.is_primary_key()) else {
            return query;
        };
        let column: Column<E, E::Pk> = Column::new(primary.name());
        query.filter(Predicate::of(
            [E::NAME],
            column.expr().eq(Expr::bound(key.into_value())),
        ))
    }
}

impl<E: Entity, J> Default for Select<E, J> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: Entity, J> Clone for Select<E, J> {
    fn clone(&self) -> Self {
        Self {
            filters: self.filters.clone(),
            joins: self.joins.clone(),
            order: self.order.clone(),
            group_by: self.group_by.clone(),
            having: self.having.clone(),
            preloads: self.preloads.clone(),
            limit: self.limit,
            offset: self.offset,
            lock: self.lock,
            lock_behavior: self.lock_behavior,
            distinct: self.distinct,
            deleted: self.deleted,
            tenant: self.tenant.clone(),
            across_tenants: self.across_tenants,
            unlimited: self.unlimited,
            entity: PhantomData,
            scope: PhantomData,
        }
    }
}

impl<E: Entity, J> core::fmt::Debug for Select<E, J> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Select")
            .field("entity", &E::NAME)
            .field("filters", &self.filters.len())
            .field("joins", &self.joins.len())
            .field("preloads", &self.preloads.len())
            .finish_non_exhaustive()
    }
}

/// A query that decodes into `T` rather than into the entity.
///
/// Produced by [`Select::select`] (a tuple of columns) and
/// [`Select::project`] (a `#[derive(Projection)]` struct).
///
/// ```
/// # use moso_orm::{Column, Entity, Projected, Select};
/// fn ids<E: Entity>(query: Select<E>, id: Column<E, i64>) -> Projected<E, (i64,)> {
///     query.select((id,))
/// }
/// ```
pub struct Projected<E, T, J = ()> {
    select: Select<E, J>,
    items: Vec<SelectItem>,
    decode: fn(&Row) -> core::result::Result<T, crate::row::DecodeError>,
}

impl<E: Entity, T, J> Projected<E, T, J> {
    /// The query underneath, for the combinators that are not repeated here.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Select};
    /// fn filters<E: Entity>(query: Select<E>, id: Column<E, i64>) -> usize {
    ///     query.select((id,)).query().filters().len()
    /// }
    /// ```
    #[must_use]
    pub const fn query(&self) -> &Select<E, J> {
        &self.select
    }

    /// The projected items.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Select};
    /// fn width<E: Entity>(query: Select<E>, id: Column<E, i64>) -> usize {
    ///     query.select((id,)).items().len()
    /// }
    /// ```
    #[must_use]
    pub fn items(&self) -> &[SelectItem] {
        &self.items
    }

    /// Adds a filter, as [`Select::filter`] does.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Projected, Select};
    /// fn positive<E: Entity>(q: Select<E>, id: Column<E, i64>) -> Projected<E, (i64,)> {
    ///     q.select((id,)).filter(id.gt(0))
    /// }
    /// ```
    #[must_use]
    #[track_caller]
    pub fn filter(mut self, predicate: impl Into<Predicate>) -> Self {
        self.select = self.select.filter(predicate);
        self
    }

    /// Caps the number of rows.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Projected, Select};
    /// fn ten<E: Entity>(q: Select<E>, id: Column<E, i64>) -> Projected<E, (i64,)> {
    ///     q.select((id,)).limit(10)
    /// }
    /// ```
    #[must_use]
    pub fn limit(mut self, rows: u64) -> Self {
        self.select = self.select.limit(rows);
        self
    }

    /// Adds an ordering term.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Projected, Select};
    /// fn sorted<E: Entity>(q: Select<E>, id: Column<E, i64>) -> Projected<E, (i64,)> {
    ///     q.select((id,)).order_by(id.desc())
    /// }
    /// ```
    #[must_use]
    pub fn order_by(mut self, term: OrderTerm) -> Self {
        self.select = self.select.order_by(term);
        self
    }

    /// Opts out of the default row cap, as [`Select::unlimited`] does.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Projected, Select};
    /// fn everything<E: Entity>(q: Select<E>, id: Column<E, i64>) -> Projected<E, (i64,)> {
    ///     q.select((id,)).unlimited()
    /// }
    /// ```
    #[must_use]
    pub fn unlimited(mut self) -> Self {
        self.select = self.select.unlimited();
        self
    }
}

impl<E: Entity, T, J: Ready<E>> Projected<E, T, J> {
    /// Runs the query and collects every projected row.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Projected, Result};
    /// async fn all<E: Entity, T>(q: Projected<E, T>, ex: impl Executor<'_>) -> Result<Vec<T>> {
    ///     q.fetch_all(ex).await
    /// }
    /// ```
    pub async fn fetch_all(mut self, executor: impl Executor<'_>) -> Result<Vec<T>> {
        let cap = self.select.apply_row_cap();
        let statement = self.to_statement()?;
        let rows = executor.handle().fetch_all(&statement).await?;
        warn_if_capped::<E>(
            cap,
            rows.len(),
            self.select.filters.first().map(Filter::call_site),
        );
        let mut projected = Vec::with_capacity(rows.len());
        for row in &rows {
            projected.push((self.decode)(row)?);
        }
        Ok(projected)
    }

    /// Runs the query and returns the single projected row it must produce.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when nothing matched.
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Projected, Result};
    /// async fn one<E: Entity, T>(q: Projected<E, T>, ex: impl Executor<'_>) -> Result<T> {
    ///     q.fetch_one(ex).await
    /// }
    /// ```
    pub async fn fetch_one(self, executor: impl Executor<'_>) -> Result<T> {
        self.fetch_optional(executor)
            .await?
            .ok_or(Error::NotFound { entity: E::NAME })
    }

    /// Runs the query and returns the first projected row, if there is one.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Projected, Result};
    /// async fn maybe<E: Entity, T>(q: Projected<E, T>, ex: impl Executor<'_>) -> Result<Option<T>> {
    ///     q.fetch_optional(ex).await
    /// }
    /// ```
    pub async fn fetch_optional(self, executor: impl Executor<'_>) -> Result<Option<T>> {
        let statement = self.to_statement()?;
        let Some(row) = executor.handle().fetch_optional(&statement).await? else {
            return Ok(None);
        };
        Ok(Some((self.decode)(&row)?))
    }

    /// Renders the projected query.
    ///
    /// # Errors
    ///
    /// As [`Select::to_statement`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Projected, Result};
    /// # use moso_sql::Statement;
    /// fn statement<E: Entity, T>(q: &Projected<E, T>) -> Result<Statement> {
    ///     q.to_statement()
    /// }
    /// ```
    pub fn to_statement(&self) -> Result<Statement> {
        Ok(self
            .select
            .build(Shape::Items(self.items.clone()))?
            .into_statement())
    }
}

impl<E: Entity, T, J> core::fmt::Debug for Projected<E, T, J> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Projected")
            .field("entity", &E::NAME)
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}

/// A stream of entities.
///
/// ```no_run
/// # use moso_orm::{Entity, EntityStream};
/// fn is_a_stream<E: Entity>(stream: EntityStream<'_, E>) -> impl futures_util::Stream {
///     stream
/// }
/// ```
pub struct EntityStream<'e, E> {
    rows: crate::executor::RowStream<'e>,
    entity: PhantomData<fn() -> E>,
}

impl<E: Entity> futures_util::Stream for EntityStream<'_, E> {
    type Item = Result<E>;

    fn poll_next(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Option<Self::Item>> {
        use core::task::Poll;

        match core::pin::Pin::new(&mut self.rows).poll_next(cx) {
            Poll::Ready(Some(Ok(row))) => Poll::Ready(Some(E::from_row(&row).map_err(Error::from))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<E: Entity> core::fmt::Debug for EntityStream<'_, E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EntityStream")
            .field("entity", &E::NAME)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{EntityDescriptor, RelationDescriptor, RelationKind};
    use crate::entity::{ColumnDef, NeedsTenant};
    use crate::row::DecodeError;
    use moso_sql::{Nulls, ValueKind};
    use std::sync::OnceLock;

    /// A post, reduced to what a query test needs.
    #[derive(Clone, Debug)]
    struct Post {
        id: i64,
    }

    impl Entity for Post {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("posts");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("published", ValueKind::Bool),
        ];
        const NAME: &'static str = "Post";

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
                EntityDescriptor::builder("Post", Self::TABLE)
                    .relation(
                        RelationDescriptor::builder("author", RelationKind::BelongsTo, "User")
                            .build(),
                    )
                    .build()
            })
        }
    }

    const ID: Column<Post, i64> = Column::new("id");
    const PUBLISHED: Column<Post, bool> = Column::new("published");

    #[test]
    fn ten_combinators_leave_the_type_alone() {
        // N1, asserted rather than described: the annotation is the test.
        let query: Select<Post> = Select::<Post>::new()
            .filter(ID.gt(0))
            .filter_opt(Some(PUBLISHED.eq(true)))
            .filter_if(true, || ID.lt(1000))
            .filter_if(false, || ID.lt(1))
            .when(true, |q| q.limit(10))
            .apply(|q| q.offset(0))
            .order_by(ID.desc())
            .distinct()
            .with_deleted()
            .lock(LockMode::ForUpdate);

        assert_eq!(query.filters().len(), 3);
        assert_eq!(query.limit_value(), Some(10));
        assert_eq!(query.deleted(), Deleted::Any);
        assert_eq!(query.lock_mode(), Some(LockMode::ForUpdate));
    }

    #[test]
    fn the_user_visible_type_is_short() {
        // Diagnostics rule 2: never print a type over 80 characters.
        let printed = core::any::type_name::<Select<Post>>();
        let last = printed.rsplit("::").next().unwrap_or(printed);
        assert!(last.len() <= 80, "{last}");
    }

    #[test]
    fn a_conditional_join_is_an_ordinary_if() {
        // This is the test the type-level joined set could not pass.
        let mut query = Select::<Post>::new();
        for wanted in [true, false] {
            query = query.when(wanted, |q| q.limit(1));
        }
        assert_eq!(query.limit_value(), Some(1));
    }

    #[test]
    fn the_scope_is_the_entity_plus_the_joins() {
        let query = Select::<Post>::new();
        assert_eq!(query.scope(), ["Post"]);
    }

    #[test]
    fn a_filter_on_the_selected_entity_is_in_scope() {
        let query = Select::<Post>::new().filter(ID.eq(1));
        assert!(query.check_scope().is_ok());
    }

    #[test]
    fn a_filter_on_an_unjoined_entity_is_refused_before_any_sql_is_sent() {
        let stranger = Predicate::of(["User"], Expr::value(true));
        let query = Select::<Post>::new().filter(stranger);

        let error = query.check_scope().expect_err("`User` is not joined");
        let text = error.to_string();
        assert!(
            text.contains("`User` is not joined in this query"),
            "{text}"
        );
        assert!(text.contains("select.rs:"), "the user's line: {text}");
        assert!(error.is_programmer_error());
    }

    #[test]
    fn an_unchecked_expression_is_never_refused() {
        let raw: Predicate = Expr::value(true).into();
        let query = Select::<Post>::new().filter(raw);
        assert!(query.check_scope().is_ok());
    }

    #[test]
    fn lock_modes_carry_their_behaviour() {
        let claimed =
            Select::<Post>::new().lock_with(LockMode::ForUpdate, LockBehavior::SkipLocked);
        assert_eq!(claimed.lock_mode(), Some(LockMode::ForUpdate));
        assert!(claimed.lock_behavior().skips());
        assert_eq!(Select::<Post>::new().lock_behavior(), LockBehavior::Wait);
        assert!(LockMode::ForShare.blocks_writers());
        assert!(!LockMode::ForKeyShare.blocks_writers());
        assert_eq!(LockMode::ForUpdate.as_sql(), "FOR UPDATE");
    }

    #[test]
    fn soft_delete_modes_do_what_they_say() {
        assert_eq!(Select::<Post>::new().deleted(), Deleted::Live);
        assert_eq!(Select::<Post>::new().with_deleted().deleted(), Deleted::Any);
        assert_eq!(
            Select::<Post>::new().only_deleted().deleted(),
            Deleted::Only
        );
    }

    #[test]
    fn a_tenant_obligation_is_discharged_by_scoping() {
        let scoped: Select<Post, ()> =
            Select::<Post, NeedsTenant>::new().scoped(TenantId::of(1_i64));
        assert!(scoped.filters().is_empty());

        let across: Select<Post, ()> = Select::<Post, NeedsTenant>::new().across_tenants();
        assert!(across.filters().is_empty());
    }

    #[test]
    fn find_filters_on_the_primary_key() {
        let query: Select<Post> = Select::find(7);
        assert_eq!(query.filters().len(), 1);
        assert_eq!(query.filters()[0].predicate().entities(), ["Post"]);
    }

    /// A soft-deletable, tenant-scoped entity, so the framework predicates have
    /// somewhere to land.
    #[derive(Clone, Debug)]
    struct Invoice {
        id: i64,
    }

    impl Entity for Invoice {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("invoices");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("total", ValueKind::I64),
        ];
        const NAME: &'static str = "Invoice";

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
                EntityDescriptor::builder("Invoice", Self::TABLE)
                    .soft_delete("deleted_at")
                    .tenant("tenant_id")
                    .build()
            })
        }
    }

    /// The `moso_sql::Select` a query renders to, for the structural
    /// assertions. The dialect renderers turn this into text; these tests are
    /// about the tree, which is what this module owns.
    fn rendered<E: Entity, J>(query: &Select<E, J>) -> moso_sql::Select {
        match query.to_statement().expect("a renderable query") {
            Statement::Select(select) => select,
            other => panic!("a Select rendered as {:?}", other.kind()),
        }
    }

    /// The SQL a statement produces on both supported dialects.
    ///
    /// Acceptance criterion 4 of `21-entities-queries.md` asks for the SQL of
    /// every combinator on PostgreSQL *and* SQLite. Rendering both from one
    /// tree is also the cheapest way to catch a clause that only one of them
    /// can express.
    fn both_dialects(statement: &Statement) -> (moso_sql::Sql, moso_sql::Sql) {
        (
            statement
                .build(&moso_sql::Postgres)
                .expect("PostgreSQL renders it"),
            statement
                .build(&moso_sql::Sqlite)
                .expect("SQLite renders it"),
        )
    }

    #[test]
    fn ten_chained_combinators_are_the_same_type() {
        // Acceptance criterion 1 of `21-entities-queries.md`, as a type
        // equality rather than a description: `same_type` unifies its two
        // arguments, so this stops compiling the moment a combinator changes
        // the shape.
        fn same_type<T>(_: &T, _: &T) {}

        let plain = Select::<Post>::new();
        let chained = Select::<Post>::new()
            .filter(ID.gt(0))
            .filter_opt(Some(PUBLISHED.eq(true)))
            .filter_if(true, || ID.lt(1000))
            .when(true, |query| query.limit(10))
            .apply(|query| query.offset(5))
            .order_by(ID.desc())
            .group_by(PUBLISHED.expr())
            .having(ID.count().gt(Expr::value(1_i64)))
            .distinct()
            .with_deleted();

        same_type(&plain, &chained);
        assert_eq!(
            core::any::type_name_of_val(&plain),
            core::any::type_name_of_val(&chained)
        );
        // …and the type a user reads in an error stays inside the budget
        // `41-diagnostics.md` sets.
        assert!(core::any::type_name_of_val(&chained).len() <= 80);
    }

    #[test]
    fn the_projection_is_every_column_in_declaration_order() {
        let select = rendered(&Select::<Post>::new());
        assert_eq!(select.items().len(), Post::COLUMNS.len());
        assert_eq!(select.from_items().len(), 1);
        assert!(select.filters().is_empty());
    }

    #[test]
    fn every_clause_reaches_the_rendered_statement() {
        let query = Select::<Post>::new()
            .filter(ID.gt(0))
            .filter(PUBLISHED.eq(true))
            .group_by(PUBLISHED.expr())
            .having(ID.count().gt(Expr::value(1_i64)))
            .order_by(ID.desc())
            .limit(25)
            .offset(50)
            .distinct()
            .lock_with(LockMode::ForUpdate, LockBehavior::SkipLocked);

        let select = rendered(&query);
        assert_eq!(select.filters().len(), 2);
        assert_eq!(select.group_by_exprs().len(), 1);
        assert_eq!(select.having_exprs().len(), 1);
        assert_eq!(select.order_terms().len(), 1);
        assert_eq!(select.limit_value(), Some(25));
        assert_eq!(select.offset_value(), Some(50));
        assert_eq!(select.distinct_mode(), &moso_sql::Distinct::Distinct);
        let lock = select.lock_mode().expect("a lock");
        assert_eq!(lock.strength(), LockStrength::Update);
        assert_eq!(lock.behavior(), SqlLockBehavior::SkipLocked);
    }

    #[test]
    fn an_unset_null_placement_is_pinned_to_postgres_order() {
        // SQLite and PostgreSQL disagree about where NULLs go; a query that is
        // silent about it must still mean the same thing on both.
        let ascending = rendered(&Select::<Post>::new().order_by(ID.asc()));
        assert_eq!(ascending.order_terms()[0].nulls(), Some(Nulls::Last));

        let descending = rendered(&Select::<Post>::new().order_by(ID.desc()));
        assert_eq!(descending.order_terms()[0].nulls(), Some(Nulls::First));

        // An explicit placement is left exactly as the caller wrote it.
        let explicit = rendered(&Select::<Post>::new().order_by_nulls_first(ID.asc()));
        assert_eq!(explicit.order_terms()[0].nulls(), Some(Nulls::First));
        let last = rendered(&Select::<Post>::new().order_by_nulls_last(ID.desc()));
        assert_eq!(last.order_terms()[0].nulls(), Some(Nulls::Last));
    }

    #[test]
    fn a_soft_deletable_entity_filters_itself() {
        let live = rendered(&Select::<Invoice>::new().across_tenants());
        assert_eq!(live.filters().len(), 1);

        let any = rendered(&Select::<Invoice>::new().across_tenants().with_deleted());
        assert!(any.filters().is_empty());

        let only = rendered(&Select::<Invoice>::new().across_tenants().only_deleted());
        assert_eq!(only.filters().len(), 1);
        assert_ne!(only.filters()[0], live.filters()[0]);
    }

    #[test]
    fn a_tenant_scoped_entity_refuses_to_render_without_a_tenant() {
        let error = Select::<Invoice>::new()
            .to_statement()
            .expect_err("no tenant was named");
        let text = error.to_string();
        assert!(text.contains("`Invoice` is tenant-scoped"), "{text}");
        assert!(text.contains("across_tenants()"), "{text}");

        // Both discharges work, and only one of them adds a predicate.
        let scoped = rendered(&Select::<Invoice>::new().scoped(TenantId::of(7_i64)));
        assert_eq!(scoped.filters().len(), 2, "soft delete plus tenant");

        let across = rendered(&Select::<Invoice>::new().across_tenants());
        assert_eq!(across.filters().len(), 1, "soft delete only");
    }

    #[test]
    fn an_out_of_scope_filter_is_refused_before_rendering() {
        let stranger = Predicate::of(["User"], Expr::value(true));
        let error = Select::<Post>::new()
            .filter(stranger)
            .to_statement()
            .expect_err("`User` is not joined");
        assert!(error.to_string().contains("`User` is not joined"));
    }

    #[test]
    fn count_drops_the_ordering_and_the_paging() {
        let query = Select::<Post>::new()
            .filter(PUBLISHED.eq(true))
            .order_by(ID.desc())
            .limit(10)
            .offset(20);

        let Statement::Select(counted) = query.to_count_statement().expect("renderable") else {
            panic!("count rendered as something other than a SELECT");
        };
        assert_eq!(counted.items().len(), 1);
        assert!(counted.order_terms().is_empty());
        assert_eq!(counted.limit_value(), None);
        assert_eq!(counted.offset_value(), None);
        assert_eq!(counted.filters().len(), 1, "the filter survives");
    }

    #[test]
    fn counting_a_distinct_or_grouped_query_wraps_it() {
        // `count(*)` over a GROUP BY counts rows per group, which is not what
        // "how many results" means, so the query becomes a subquery.
        for query in [
            Select::<Post>::new().distinct(),
            Select::<Post>::new().group_by(PUBLISHED.expr()),
        ] {
            let Statement::Select(counted) = query.to_count_statement().expect("renderable") else {
                panic!("count rendered as something other than a SELECT");
            };
            assert_eq!(counted.items().len(), 1);
            assert!(
                matches!(counted.from_items()[0], moso_sql::FromItem::Subquery { .. }),
                "the counted query is wrapped"
            );
        }
    }

    #[test]
    fn exists_stops_at_the_first_row() {
        let Statement::Select(outer) = Select::<Post>::new()
            .filter(PUBLISHED.eq(true))
            .order_by(ID.desc())
            .to_exists_statement()
            .expect("renderable")
        else {
            panic!("exists rendered as something other than a SELECT");
        };
        assert_eq!(outer.items().len(), 1);
        assert!(outer.from_items().is_empty(), "`SELECT exists (…)`");

        // A caller-set limit of 0 means "no rows", and existence agrees.
        let zero = Select::<Post>::new().limit(0);
        assert!(zero.to_exists_statement().is_ok());
    }

    /// The cap is a process-wide global, so the test that changes it and the
    /// test that reads the default cannot be two tests: `cargo test` runs them
    /// on different threads at the same time.
    #[test]
    fn the_fetch_all_row_cap_is_applied_opted_out_of_and_configurable() {
        let mut capped = Select::<Post>::new();
        assert_eq!(capped.apply_row_cap(), Some(DEFAULT_ROW_LIMIT));
        assert_eq!(capped.limit_value(), Some(DEFAULT_ROW_LIMIT));

        // An explicit limit already answers the question the cap exists to ask.
        let mut explicit = Select::<Post>::new().limit(5);
        assert_eq!(explicit.apply_row_cap(), None);
        assert_eq!(explicit.limit_value(), Some(5));

        // `.unlimited()` is the documented opt-out.
        let mut unlimited = Select::<Post>::new().unlimited();
        assert!(unlimited.is_unlimited());
        assert_eq!(unlimited.apply_row_cap(), None);
        assert_eq!(unlimited.limit_value(), None);

        set_default_row_limit(3);
        assert_eq!(default_row_limit(), 3);
        let mut small = Select::<Post>::new();
        assert_eq!(small.apply_row_cap(), Some(3));

        // Zero turns the cap off for the whole process.
        set_default_row_limit(0);
        let mut off = Select::<Post>::new();
        assert_eq!(off.apply_row_cap(), None);
        assert_eq!(off.limit_value(), None);

        set_default_row_limit(DEFAULT_ROW_LIMIT);
        assert_eq!(default_row_limit(), DEFAULT_ROW_LIMIT);
    }

    #[test]
    fn a_projection_replaces_the_column_list_and_keeps_the_rest() {
        let projected = Select::<Post>::new()
            .filter(PUBLISHED.eq(true))
            .select((ID,))
            .order_by(ID.desc())
            .limit(7);

        let Statement::Select(select) = projected.to_statement().expect("renderable") else {
            panic!("a projection rendered as something other than a SELECT");
        };
        assert_eq!(select.items().len(), 1);
        assert_eq!(select.filters().len(), 1);
        assert_eq!(select.order_terms().len(), 1);
        assert_eq!(select.limit_value(), Some(7));
    }

    #[test]
    fn the_four_join_kinds_map_onto_sql_and_widen_the_scope() {
        for (ours, theirs) in [
            (JoinKind::Inner, moso_sql::JoinKind::Inner),
            (JoinKind::Left, moso_sql::JoinKind::Left),
            (JoinKind::Right, moso_sql::JoinKind::Right),
            (JoinKind::Full, moso_sql::JoinKind::Full),
        ] {
            assert_eq!(ours.to_sql(), theirs);
        }
        assert!(JoinKind::Full.keeps_unmatched_rows());
        assert!(JoinKind::Full.keeps_unmatched_joined_rows());
        assert!(!JoinKind::Right.keeps_unmatched_rows());
        assert!(JoinKind::Right.keeps_unmatched_joined_rows());

        // A join brings the target entity into scope, which is what makes a
        // filter on its columns legal.
        let joined = Joined::new(
            JoinKind::Left,
            "User",
            TableRef::from_static("users"),
            Expr::value(true),
        );
        let mut query = Select::<Post>::new();
        query.joins.push(joined);
        assert_eq!(query.scope(), ["Post", "User"]);

        let stranger = Predicate::of(["User"], Expr::value(true));
        let query = query.filter(stranger);
        assert!(query.check_scope().is_ok());
        let select = rendered(&query);
        assert_eq!(select.joins().len(), 1);
        assert_eq!(select.joins()[0].kind(), moso_sql::JoinKind::Left);
    }

    #[test]
    fn a_named_scope_composes_like_a_closure() {
        use crate::scope::Scope;

        let published = Scope::new("published", |query: Select<Post>| {
            query.filter(PUBLISHED.eq(true))
        });
        let query = Select::<Post>::new().with_scope(&published).limit(3);
        assert_eq!(query.filters().len(), 1);
        assert_eq!(query.limit_value(), Some(3));
    }

    #[test]
    fn every_combinator_renders_on_both_dialects() {
        // Acceptance criterion 4: each of these is one combinator, rendered on
        // PostgreSQL and on SQLite. A clause one dialect cannot express fails
        // here rather than in production.
        let cases: Vec<(&str, Select<Post>)> = vec![
            ("bare", Select::<Post>::new()),
            ("filter", Select::<Post>::new().filter(ID.gt(0))),
            (
                "two filters",
                Select::<Post>::new()
                    .filter(ID.gt(0))
                    .filter(PUBLISHED.eq(true)),
            ),
            ("order", Select::<Post>::new().order_by(ID.desc())),
            (
                "nulls first",
                Select::<Post>::new().order_by_nulls_first(ID.asc()),
            ),
            ("limit", Select::<Post>::new().limit(10)),
            ("offset", Select::<Post>::new().limit(10).offset(20)),
            ("distinct", Select::<Post>::new().distinct()),
            (
                "group by and having",
                Select::<Post>::new()
                    .group_by(PUBLISHED.expr())
                    .having(ID.count().gt(Expr::value(1_i64))),
            ),
            ("find", Select::find(7)),
            ("in list", Select::<Post>::new().filter(ID.is_in([1, 2, 3]))),
            (
                "empty in list",
                Select::<Post>::new().filter(ID.is_in(Vec::<i64>::new())),
            ),
        ];

        for (name, query) in cases {
            let statement = query.to_statement().expect("a renderable query");
            let (postgres, sqlite) = both_dialects(&statement);

            assert!(
                postgres.text.to_ascii_uppercase().starts_with("SELECT"),
                "{name}: {}",
                postgres.text
            );
            assert!(postgres.text.contains("posts"), "{name}: {}", postgres.text);
            assert!(sqlite.text.contains("posts"), "{name}: {}", sqlite.text);
            // The two dialects number their parameters differently and bind the
            // same values, which is the whole point of building a tree once.
            assert_eq!(
                postgres.args.len(),
                sqlite.args.len(),
                "{name}: parameter counts diverged"
            );
            if !postgres.args.is_empty() {
                assert!(postgres.text.contains("$1"), "{name}: {}", postgres.text);
                assert!(sqlite.text.contains('?'), "{name}: {}", sqlite.text);
            }
        }
    }

    #[test]
    fn the_rendered_sql_carries_the_clauses_it_was_given() {
        let statement = Select::<Post>::new()
            .filter(ID.gt(0))
            .order_by(ID.desc())
            .limit(25)
            .offset(50)
            .distinct()
            .to_statement()
            .expect("a renderable query");
        let (postgres, sqlite) = both_dialects(&statement);

        for sql in [&postgres, &sqlite] {
            let upper = sql.text.to_ascii_uppercase();
            assert!(upper.contains("DISTINCT"), "{}", sql.text);
            assert!(upper.contains("WHERE"), "{}", sql.text);
            assert!(upper.contains("ORDER BY"), "{}", sql.text);
            assert!(upper.contains("LIMIT"), "{}", sql.text);
            assert!(upper.contains("OFFSET"), "{}", sql.text);
            assert_eq!(sql.args.len(), 1, "one bound filter value: {}", sql.text);
        }
    }

    #[test]
    fn a_locking_read_renders_on_postgres_and_is_refused_on_sqlite() {
        // SQLite has no `FOR UPDATE`: it locks the whole database file. The
        // sealed facade says so rather than silently dropping the clause, which
        // would turn a read-modify-write into a race.
        let statement = Select::<Post>::new()
            .lock(LockMode::ForUpdate)
            .to_statement()
            .expect("a renderable query");
        let postgres = statement
            .build(&moso_sql::Postgres)
            .expect("PostgreSQL locks rows");
        assert!(
            postgres.text.to_ascii_uppercase().contains("FOR UPDATE"),
            "{}",
            postgres.text
        );

        match statement.build(&moso_sql::Sqlite) {
            Ok(sql) => assert!(
                !sql.text.to_ascii_uppercase().contains("FOR UPDATE"),
                "SQLite must not claim to lock a row: {}",
                sql.text
            ),
            Err(error) => assert!(
                error.to_string().to_ascii_lowercase().contains("sqlite"),
                "the refusal names the dialect: {error}"
            ),
        }
    }

    #[test]
    fn count_and_exists_render_on_both_dialects() {
        let query = Select::<Post>::new().filter(PUBLISHED.eq(true));

        let counted = query.clone().to_count_statement().expect("renderable");
        let (postgres, sqlite) = both_dialects(&counted);
        assert!(
            postgres.text.to_ascii_uppercase().contains("COUNT("),
            "{}",
            postgres.text
        );
        assert!(
            sqlite.text.to_ascii_uppercase().contains("COUNT("),
            "{}",
            sqlite.text
        );

        let wrapped = query
            .clone()
            .distinct()
            .to_count_statement()
            .expect("renderable");
        let (postgres, _) = both_dialects(&wrapped);
        assert!(
            postgres.text.to_ascii_uppercase().contains("DISTINCT"),
            "{}",
            postgres.text
        );

        let any = query.to_exists_statement().expect("renderable");
        let (postgres, sqlite) = both_dialects(&any);
        assert!(
            postgres.text.to_ascii_uppercase().contains("EXISTS"),
            "{}",
            postgres.text
        );
        assert!(
            sqlite.text.to_ascii_uppercase().contains("EXISTS"),
            "{}",
            sqlite.text
        );
    }

    #[test]
    fn the_framework_predicates_reach_the_rendered_text() {
        let statement = Select::<Invoice>::new()
            .scoped(TenantId::of(7_i64))
            .filter(Predicate::of(["Invoice"], Expr::value(true)))
            .to_statement()
            .expect("renderable");
        let (postgres, _) = both_dialects(&statement);
        let text = postgres.text.to_ascii_lowercase();
        assert!(text.contains("deleted_at"), "{}", postgres.text);
        assert!(text.contains("tenant_id"), "{}", postgres.text);
        // Both framework predicates land before the user's, so `EXPLAIN` shows
        // the two invariants first.
        let soft_delete = text.find("deleted_at").expect("the soft-delete column");
        let tenant = text.find("tenant_id").expect("the tenant column");
        assert!(soft_delete < tenant, "{}", postgres.text);

        // The tenant is bound, never interpolated: one parameter for it and one
        // for the user's filter, and no literal `7` anywhere in the text.
        assert_eq!(postgres.args.len(), 2, "{}", postgres.text);
        assert!(!postgres.text.contains('7'), "{}", postgres.text);
        assert_eq!(postgres.args[0], moso_sql::Value::I64(7));
    }

    #[test]
    fn a_preload_is_recorded_but_never_joined() {
        // N3's mechanism lives in `relation::run_preloads`; what `Select` owes
        // it is the node list, unchanged and un-joined — a preload must not
        // widen the parent query, because that is exactly the row-multiplying
        // join `.with(..)` exists to avoid.
        let query = Select::<Post>::new().with(Preload::new(
            "author",
            crate::descriptor::RelationKind::BelongsTo,
            "User",
        ));
        assert_eq!(query.preloads().len(), 1);
        assert!(query.joins().is_empty());
        assert_eq!(rendered(&query).joins().len(), 0);
        // …and the preload does not put `User` in scope, because nothing was
        // joined: a filter on the target still has to say so.
        assert_eq!(query.scope(), ["Post"]);
    }
}
