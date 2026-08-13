//! [`Preload`] — one relation, one statement, whatever the row count.
//!
//! # Non-negotiable N3, stated as an algorithm
//!
//! A preload never issues a statement per parent, and never widens the parent
//! query into a join that multiplies its rows. It does this instead:
//!
//! 1. collect the parents' keys — their primary keys, or the foreign key column
//!    for a `belongs_to`;
//! 2. **deduplicate** them, so a hundred posts by ten authors ask for ten;
//! 3. issue **one** statement, `WHERE key = ANY($1)` on a dialect with arrays
//!    and `WHERE key IN (…)` on one without;
//! 4. group the rows it comes back with by that key and hand each parent its
//!    own.
//!
//! Nested preloads run on the *flattened* children of every parent at once, so
//! a second level is one more statement, not one per parent. That is the whole
//! of [`Preload::statement_count`], and it is a number a test can assert
//! against rather than a claim in a document.
//!
//! # Two round trips beat one row-multiplying join
//!
//! `.with(..)` is deliberately not a `JOIN`. A hundred posts with ten comments
//! each is a thousand rows over the wire if joined — every post's title,
//! body and metadata repeated ten times — against a hundred plus a thousand
//! *narrow* rows in two statements. The join wins only for tiny result sets,
//! and it silently corrupts `LIMIT`. Joins are for *filtering*
//! ([`Select::join`](crate::Select::join)); preloads are for *fetching*. Rails
//! conflates the two and everybody gets it wrong.
//!
//! # What a node needs before it can run
//!
//! [`Preload::new`] describes a relation — a name, a kind, a target — and
//! cannot run one, because it does not know the target's table, its columns or
//! how to put the rows it loads into the parent's field. A node built from a
//! relation constant knows all three:
//!
//! ```
//! # use moso_orm::{Preload, RelationKind};
//! // Describes; cannot run.
//! let described = Preload::new("comments", RelationKind::HasMany, "Comment");
//! assert!(!described.is_runnable());
//! ```

use core::any::Any;
use core::fmt;
use core::future::Future;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use moso_sql::{
    Aggregate, Array, BinOp, Capabilities, Cte, Expr, FromItem, Ident, Order, OrderTerm,
    Select as SqlSelect, SelectItem, Statement, StatementRef, TableRef, Value, ValueKind,
    WindowExpr, WindowFunc, WindowSpec,
};

use crate::db::Backend;
use crate::descriptor::RelationKind;
use crate::entity::Entity;
use crate::error::{CallSite, Error, Result};
use crate::executor::Handle;
use crate::predicate::Predicate;
use crate::relation::related::{ForeignKeyFn, LinkFn, LoadedRows};
use crate::row::Row;
use crate::sqltype::SqlType;

/// The alias the grouping key is projected under.
///
/// Every preload statement ends its projection with this column, so the loader
/// finds the key at a fixed index instead of hunting for it by name.
const KEY_ALIAS: &str = "moso_key";

/// The alias `ROW_NUMBER()` is projected under by a per-parent limit.
const ROW_ALIAS: &str = "moso_row";

/// The alias the per-parent limit's subquery or CTE carries.
const SCOPE_ALIAS: &str = "moso_children";

/// The alias the correlated form gives the second reference to that CTE.
const PEER_ALIAS: &str = "moso_peer";

/// One node of a preload tree.
///
/// Built from a relation constant, and refined with filters, an order, a
/// per-parent limit, a column subset, or nested preloads. Each node costs
/// exactly one statement.
///
/// ```
/// use moso_orm::{Preload, Predicate, RelationKind};
/// use moso_sql::Expr;
///
/// let newest_three = Preload::new("comments", RelationKind::HasMany, "Comment")
///     .filter(Predicate::of(["Comment"], Expr::value(true)))
///     .limit_per_parent(3);
///
/// assert_eq!(newest_three.limit_per_parent_value(), Some(3));
/// assert_eq!(newest_three.statement_count(), 1);
/// ```
#[derive(Clone)]
pub struct Preload {
    relation: &'static str,
    kind: RelationKind,
    target: &'static str,
    filters: Vec<Predicate>,
    order: Vec<OrderTerm>,
    limit_per_parent: Option<u32>,
    columns: Vec<Ident>,
    counting: bool,
    nested: Vec<Preload>,
    /// The foreign-key column: on the target for a `has_many`/`has_one`, on the
    /// owner for a `belongs_to`, and unused for a `many_to_many`.
    key: Option<&'static str>,
    /// The join table of a `many_to_many`, with its two key columns.
    through: Option<(&'static str, &'static str, &'static str)>,
    /// The target's table, primary key and column list — everything the planner
    /// needs that depends on the related entity.
    target_table: Option<TableRef>,
    /// The target's first primary-key column.
    target_key: Option<Ident>,
    /// The target's columns, in `Entity::from_row` order.
    target_columns: Vec<Ident>,
    /// Decode, recurse, regroup: the one operation that must know the related
    /// entity's Rust type. See [`LoadFn`].
    load: Option<LoadFn>,
    /// `LinkFn<E>`, erased. Downcast by [`Preload::link_fn`], where the *owning*
    /// entity is known again.
    link: Option<Arc<dyn Any + Send + Sync>>,
    /// `ForeignKeyFn<E>`, erased. Only a `belongs_to` has one.
    parent_key: Option<Arc<dyn Any + Send + Sync>>,
    /// Where the user asked for this preload, for the N+1 report.
    at: Option<CallSite>,
}

/// Decode the rows, run the nested preloads on all of them at once, and hand
/// back one payload per parent.
///
/// This is the single place the related entity's type is needed, and the single
/// place it is erased. `groups[i]` holds the indices into `rows` that belong to
/// parent `i`; the result has one entry per group, in the same order.
type LoadFn = for<'a> fn(
    &'a Preload,
    &'a [Row],
    &'a [Vec<usize>],
    Handle<'a>,
) -> BoxFuture<'a, Result<Vec<LoadedRows>>>;

impl Preload {
    /// A preload of `relation`, with nothing refined.
    ///
    /// This constructor describes a relation without being able to execute one:
    /// it names the target entity but does not know its table or its columns.
    /// Use [`Preload::of`] to get a node that runs.
    ///
    /// ```
    /// use moso_orm::{Preload, RelationKind};
    ///
    /// let p = Preload::new("author", RelationKind::BelongsTo, "User");
    /// assert_eq!(p.relation(), "author");
    /// assert!(!p.is_runnable());
    /// ```
    #[must_use]
    pub const fn new(relation: &'static str, kind: RelationKind, target: &'static str) -> Self {
        Self {
            relation,
            kind,
            target,
            filters: Vec::new(),
            order: Vec::new(),
            limit_per_parent: None,
            columns: Vec::new(),
            counting: false,
            nested: Vec::new(),
            key: None,
            through: None,
            target_table: None,
            target_key: None,
            target_columns: Vec::new(),
            load: None,
            link: None,
            parent_key: None,
            at: None,
        }
    }

    /// A preload whose related entity is `T`, and which therefore knows how to
    /// build its statement and decode its rows.
    ///
    /// Relation constants call this; it is public so that a hand-written
    /// relation is not a second-class one.
    ///
    /// ```
    /// use moso_orm::{Preload, RelationKind};
    /// # use moso_orm::{ColumnDef, DecodeError, Entity, Row};
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::{TableRef, ValueKind};
    /// # use std::sync::OnceLock;
    /// # #[derive(Clone, Debug)] pub struct Comment { pub id: i64 }
    /// # impl Entity for Comment {
    /// #     type Pk = i64;
    /// #     const TABLE: TableRef = TableRef::from_static("comments");
    /// #     const COLUMNS: &'static [ColumnDef] = &[
    /// #         ColumnDef::new("id", ValueKind::I64).primary_key(),
    /// #         ColumnDef::new("post_id", ValueKind::I64),
    /// #     ];
    /// #     const NAME: &'static str = "Comment";
    /// #     fn pk(&self) -> i64 { self.id }
    /// #     fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
    /// #     fn descriptor() -> &'static EntityDescriptor {
    /// #         static D: OnceLock<EntityDescriptor> = OnceLock::new();
    /// #         D.get_or_init(|| EntityDescriptor::builder("Comment", Self::TABLE).build())
    /// #     }
    /// # }
    /// let p = Preload::of::<Comment>("comments", RelationKind::HasMany).keyed("post_id");
    /// assert!(p.is_runnable());
    /// assert_eq!(p.target(), "Comment");
    /// ```
    #[must_use]
    pub fn of<T: Entity>(relation: &'static str, kind: RelationKind) -> Self {
        Self {
            target_table: Some(T::TABLE),
            target_key: Some(primary_key_of::<T>()),
            target_columns: T::COLUMNS.iter().map(crate::ColumnDef::ident).collect(),
            load: Some(load_rows::<T> as LoadFn),
            ..Self::new(relation, kind, T::NAME)
        }
    }

    /// Names the foreign-key column: on the target for a `has_many` or a
    /// `has_one`, on the owner for a `belongs_to`.
    ///
    /// # Panics
    ///
    /// If `key` is not a valid SQL identifier, which the derive checks at
    /// compile time.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// let p = Preload::new("comments", RelationKind::HasMany, "Comment").keyed("post_id");
    /// assert_eq!(p.key_column(), Some("post_id"));
    /// ```
    #[must_use]
    pub const fn keyed(mut self, key: &'static str) -> Self {
        assert!(Ident::is_valid(key), "a foreign key must be an identifier");
        self.key = Some(key);
        self
    }

    /// Names the join table of a `many_to_many` and its two key columns.
    ///
    /// # Panics
    ///
    /// If any of the three is not a valid SQL identifier.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// let p = Preload::new("tags", RelationKind::ManyToMany, "Tag")
    ///     .through("post_tags", "post_id", "tag_id");
    /// assert_eq!(p.join_table(), Some("post_tags"));
    /// ```
    #[must_use]
    pub const fn through(
        mut self,
        table: &'static str,
        left: &'static str,
        right: &'static str,
    ) -> Self {
        assert!(
            Ident::is_valid(table) && Ident::is_valid(left) && Ident::is_valid(right),
            "a join table and its keys must be identifiers"
        );
        self.through = Some((table, left, right));
        self
    }

    /// Supplies the setter that puts the loaded rows into the owner's field.
    ///
    /// `#[derive(Entity)]` generates one per relation; see [`LinkFn`].
    ///
    /// ```
    /// use moso_orm::relation::{LinkFn, Related};
    /// use moso_orm::{Preload, RelationKind};
    /// # use moso_orm::{ColumnDef, DecodeError, Entity, Row};
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::{TableRef, ValueKind};
    /// # use std::sync::OnceLock;
    /// #[derive(Clone, Debug)]
    /// pub struct Post { pub id: i64, pub comments: Related<Vec<i64>> }
    /// # impl Entity for Post {
    /// #     type Pk = i64;
    /// #     const TABLE: TableRef = TableRef::from_static("posts");
    /// #     const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
    /// #     const NAME: &'static str = "Post";
    /// #     fn pk(&self) -> i64 { self.id }
    /// #     fn from_row(row: &Row) -> Result<Self, DecodeError> {
    /// #         Ok(Self { id: row.get_i64(0)?, comments: Related::NotLoaded })
    /// #     }
    /// #     fn descriptor() -> &'static EntityDescriptor {
    /// #         static D: OnceLock<EntityDescriptor> = OnceLock::new();
    /// #         D.get_or_init(|| EntityDescriptor::builder("Post", Self::TABLE).build())
    /// #     }
    /// # }
    /// const LINK: LinkFn<Post> = |post, rows| {
    ///     post.comments = Related::Loaded(rows.into_rows::<i64>()?);
    ///     Ok(())
    /// };
    ///
    /// let p = Preload::new("comments", RelationKind::HasMany, "Comment").linking(LINK);
    /// assert!(p.link_fn::<Post>().is_some());
    /// ```
    #[must_use]
    pub fn linking<E: Entity>(mut self, link: LinkFn<E>) -> Self {
        self.link = Some(Arc::new(link));
        self
    }

    /// Supplies the reader for a `belongs_to`'s foreign key on the **owner**.
    ///
    /// Without one the preloader batches on the owner's primary key, which is
    /// right for every other kind and wrong for this one.
    ///
    /// ```
    /// # use moso_orm::relation::ForeignKeyFn;
    /// # use moso_orm::{Preload, RelationKind};
    /// # use moso_sql::Value;
    /// # use moso_orm::{ColumnDef, DecodeError, Entity, Row};
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::{TableRef, ValueKind};
    /// # use std::sync::OnceLock;
    /// # #[derive(Clone, Debug)] pub struct Post { pub id: i64, pub author_id: i64 }
    /// # impl Entity for Post {
    /// #     type Pk = i64;
    /// #     const TABLE: TableRef = TableRef::from_static("posts");
    /// #     const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
    /// #     const NAME: &'static str = "Post";
    /// #     fn pk(&self) -> i64 { self.id }
    /// #     fn from_row(row: &Row) -> Result<Self, DecodeError> {
    /// #         Ok(Self { id: row.get_i64(0)?, author_id: row.get_i64(1)? })
    /// #     }
    /// #     fn descriptor() -> &'static EntityDescriptor {
    /// #         static D: OnceLock<EntityDescriptor> = OnceLock::new();
    /// #         D.get_or_init(|| EntityDescriptor::builder("Post", Self::TABLE).build())
    /// #     }
    /// # }
    /// const KEY: ForeignKeyFn<Post> = |post| Value::I64(post.author_id);
    ///
    /// let p = Preload::new("author", RelationKind::BelongsTo, "User").keyed_by(KEY);
    /// assert!(p.parent_key_fn::<Post>().is_some());
    /// ```
    #[must_use]
    pub fn keyed_by<E: Entity>(mut self, key: ForeignKeyFn<E>) -> Self {
        self.parent_key = Some(Arc::new(key));
        self
    }

    /// Records where the preload was asked for, so an N+1 warning can name the
    /// user's line.
    ///
    /// ```
    /// # use moso_orm::{CallSite, Preload, RelationKind};
    /// let p = Preload::new("c", RelationKind::HasMany, "C").at(CallSite::caller());
    /// assert!(p.call_site().is_some());
    /// ```
    #[must_use]
    pub const fn at(mut self, site: CallSite) -> Self {
        self.at = Some(site);
        self
    }

    /// Filters the related rows.
    ///
    /// ```
    /// # use moso_orm::{Preload, Predicate, RelationKind};
    /// # use moso_sql::Expr;
    /// let p = Preload::new("comments", RelationKind::HasMany, "Comment")
    ///     .filter(Predicate::of(["Comment"], Expr::value(true)));
    /// assert_eq!(p.filters().len(), 1);
    /// ```
    #[must_use]
    pub fn filter(mut self, predicate: impl Into<Predicate>) -> Self {
        self.filters.push(predicate.into());
        self
    }

    /// Orders the related rows.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// # use moso_sql::{Expr, Ident, OrderTerm};
    /// let p = Preload::new("comments", RelationKind::HasMany, "Comment")
    ///     .order_by(OrderTerm::desc(Expr::col(Ident::from_static("created_at"))));
    /// assert_eq!(p.order_terms().len(), 1);
    /// ```
    #[must_use]
    pub fn order_by(mut self, term: OrderTerm) -> Self {
        self.order.push(term);
        self
    }

    /// Keeps at most `rows` related rows **per parent**.
    ///
    /// A `ROW_NUMBER()` window where the dialect has one, and a correlated
    /// count over a CTE where it does not; the two are asserted to return the
    /// same rows in the same order. See [`LimitStrategy`].
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// let p = Preload::new("comments", RelationKind::HasMany, "C").limit_per_parent(3);
    /// assert_eq!(p.limit_per_parent_value(), Some(3));
    /// ```
    #[must_use]
    pub const fn limit_per_parent(mut self, rows: u32) -> Self {
        self.limit_per_parent = Some(rows);
        self
    }

    /// Fetches only these columns of the related rows.
    ///
    /// The statement narrows; the *decoder* does not, because
    /// [`Entity::from_row`] reads its columns positionally and a narrowed row
    /// would silently shift them. A subset that is not the entity's full column
    /// list is therefore rejected when the node runs, with a message pointing
    /// at the projection that does work. See the note on
    /// [`Preload::statement`].
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// # use moso_sql::Ident;
    /// let p = Preload::new("author", RelationKind::BelongsTo, "User")
    ///     .columns([Ident::from_static("id"), Ident::from_static("name")]);
    /// assert_eq!(p.column_names().len(), 2);
    /// ```
    #[must_use]
    pub fn columns(mut self, columns: impl IntoIterator<Item = Ident>) -> Self {
        self.columns.extend(columns);
        self
    }

    /// Nests another preload under this one, for one more statement.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// let nested = Preload::new("comments", RelationKind::HasMany, "Comment")
    ///     .with(Preload::new("author", RelationKind::BelongsTo, "User"));
    /// assert_eq!(nested.statement_count(), 2);
    /// ```
    #[must_use]
    pub fn with(mut self, preload: impl Into<Preload>) -> Self {
        self.nested.push(preload.into());
        self
    }

    /// Fetches a count instead of the rows.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert!(Preload::new("c", RelationKind::HasMany, "C").counting().is_counting());
    /// ```
    #[must_use]
    pub const fn counting(mut self) -> Self {
        self.counting = true;
        self
    }

    /// The relation's field name.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert_eq!(Preload::new("c", RelationKind::HasMany, "C").relation(), "c");
    /// ```
    #[must_use]
    pub const fn relation(&self) -> &'static str {
        self.relation
    }

    /// Which of the four shapes the relation is.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert_eq!(Preload::new("c", RelationKind::HasMany, "C").kind(), RelationKind::HasMany);
    /// ```
    #[must_use]
    pub const fn kind(&self) -> RelationKind {
        self.kind
    }

    /// The related entity's name.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert_eq!(Preload::new("c", RelationKind::HasMany, "Comment").target(), "Comment");
    /// ```
    #[must_use]
    pub const fn target(&self) -> &'static str {
        self.target
    }

    /// The filters on the related rows.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert!(Preload::new("c", RelationKind::HasMany, "C").filters().is_empty());
    /// ```
    #[must_use]
    pub fn filters(&self) -> &[Predicate] {
        &self.filters
    }

    /// The ordering of the related rows.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert!(Preload::new("c", RelationKind::HasMany, "C").order_terms().is_empty());
    /// ```
    #[must_use]
    pub fn order_terms(&self) -> &[OrderTerm] {
        &self.order
    }

    /// The per-parent limit, when one was set.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert!(Preload::new("c", RelationKind::HasMany, "C").limit_per_parent_value().is_none());
    /// ```
    #[must_use]
    pub const fn limit_per_parent_value(&self) -> Option<u32> {
        self.limit_per_parent
    }

    /// The column subset, when one was chosen.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert!(Preload::new("c", RelationKind::HasMany, "C").column_names().is_empty());
    /// ```
    #[must_use]
    pub fn column_names(&self) -> &[Ident] {
        &self.columns
    }

    /// Whether this node fetches a count rather than rows.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert!(!Preload::new("c", RelationKind::HasMany, "C").is_counting());
    /// ```
    #[must_use]
    pub const fn is_counting(&self) -> bool {
        self.counting
    }

    /// The nested preloads.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert!(Preload::new("c", RelationKind::HasMany, "C").nested().is_empty());
    /// ```
    #[must_use]
    pub fn nested(&self) -> &[Preload] {
        &self.nested
    }

    /// The foreign-key column, when one was named.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert!(Preload::new("c", RelationKind::HasMany, "C").key_column().is_none());
    /// ```
    #[must_use]
    pub const fn key_column(&self) -> Option<&'static str> {
        self.key
    }

    /// The join table of a `many_to_many`, when one was named.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert!(Preload::new("t", RelationKind::ManyToMany, "T").join_table().is_none());
    /// ```
    #[must_use]
    pub const fn join_table(&self) -> Option<&'static str> {
        match self.through {
            Some((table, _, _)) => Some(table),
            None => None,
        }
    }

    /// Where the preload was asked for, when the call site was recorded.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert!(Preload::new("c", RelationKind::HasMany, "C").call_site().is_none());
    /// ```
    #[must_use]
    pub const fn call_site(&self) -> Option<CallSite> {
        self.at
    }

    /// Whether the node knows enough to build a statement and decode its rows.
    ///
    /// False for a node from [`Preload::new`], true for one from
    /// [`Preload::of`] or a relation constant.
    ///
    /// ```
    /// # use moso_orm::{Preload, RelationKind};
    /// assert!(!Preload::new("c", RelationKind::HasMany, "C").is_runnable());
    /// ```
    #[must_use]
    pub const fn is_runnable(&self) -> bool {
        self.target_table.is_some() && self.load.is_some()
    }

    /// The setter for `E`'s field, when one was supplied and it is `E`'s.
    ///
    /// ```
    /// # use moso_orm::{Entity, Preload, RelationKind};
    /// fn setter<E: Entity>(p: &Preload) -> bool {
    ///     p.link_fn::<E>().is_some()
    /// }
    /// ```
    #[must_use]
    pub fn link_fn<E: Entity>(&self) -> Option<LinkFn<E>> {
        self.link.as_ref()?.downcast_ref::<LinkFn<E>>().copied()
    }

    /// The owner-side key reader, when one was supplied and it is `E`'s.
    ///
    /// ```
    /// # use moso_orm::{Entity, Preload, RelationKind};
    /// fn reader<E: Entity>(p: &Preload) -> bool {
    ///     p.parent_key_fn::<E>().is_some()
    /// }
    /// ```
    #[must_use]
    pub fn parent_key_fn<E: Entity>(&self) -> Option<ForeignKeyFn<E>> {
        self.parent_key
            .as_ref()?
            .downcast_ref::<ForeignKeyFn<E>>()
            .copied()
    }

    /// How many statements this subtree costs.
    ///
    /// One per node, whatever the row count. This is non-negotiable N3 as a
    /// number a test can assert against.
    ///
    /// ```
    /// use moso_orm::{Preload, RelationKind};
    ///
    /// let three_deep = Preload::new("a", RelationKind::HasMany, "A")
    ///     .with(Preload::new("b", RelationKind::HasMany, "B")
    ///         .with(Preload::new("c", RelationKind::BelongsTo, "C")));
    /// assert_eq!(three_deep.statement_count(), 3);
    /// ```
    #[must_use]
    pub fn statement_count(&self) -> usize {
        1 + self
            .nested
            .iter()
            .map(Preload::statement_count)
            .sum::<usize>()
    }

    /// The statement this node issues for `keys`, on `backend`.
    ///
    /// **One** statement, for any number of keys. The dialect decides two
    /// things: whether the keys go in as one array parameter (`= ANY($1)`) or
    /// as a list (`IN (?, ?, …)`), and how a per-parent limit is expressed.
    ///
    /// # Errors
    ///
    /// - [`Error::Build`] when the node came from [`Preload::new`] and has no
    ///   target table, or when a `many_to_many` has no join table.
    /// - [`Error::Unsupported`] when a per-parent limit needs an ordering the
    ///   dialect's strategy cannot express, or when a column subset would
    ///   shift the decoder's positions.
    ///
    /// ```
    /// # use moso_orm::{Backend, Preload, RelationKind};
    /// # use moso_sql::Value;
    /// // A node that only describes a relation cannot be planned.
    /// let described = Preload::new("comments", RelationKind::HasMany, "Comment");
    /// assert!(described.statement(&[Value::I64(1)], Backend::Postgres).is_err());
    /// ```
    pub fn statement(&self, keys: &[Value], backend: Backend) -> Result<Statement> {
        self.statement_using(keys, backend, LimitStrategy::for_backend(backend))
    }

    /// The statement this node issues, with the per-parent limit forced to a
    /// strategy.
    ///
    /// Exists so that both strategies can be run against the same database and
    /// asserted to return identical rows — which is the only honest way to
    /// claim that the dialect difference is invisible.
    ///
    /// # Errors
    ///
    /// As [`Preload::statement`].
    ///
    /// ```
    /// # use moso_orm::relation::LimitStrategy;
    /// # use moso_orm::{Backend, Preload, RelationKind};
    /// # use moso_sql::Value;
    /// let p = Preload::new("comments", RelationKind::HasMany, "Comment");
    /// let planned = p.statement_using(&[Value::I64(1)], Backend::Sqlite, LimitStrategy::Window);
    /// assert!(planned.is_err(), "still not runnable");
    /// ```
    pub fn statement_using(
        &self,
        keys: &[Value],
        backend: Backend,
        strategy: LimitStrategy,
    ) -> Result<Statement> {
        let capabilities = backend.dialect().capabilities();
        let table = self.target_table.clone().ok_or_else(Self::not_runnable)?;
        let target_key = self
            .target_key
            .clone()
            .unwrap_or_else(|| Ident::from_static("id"));

        if !self.columns.is_empty() && self.columns != self.target_columns {
            return Err(Error::Unsupported {
                feature: "a preload of a subset of the related entity's columns",
                backend,
            });
        }

        let (source, key_expr) = self.source_and_key(&table, &target_key, backend)?;

        if self.counting {
            return Ok(self
                .count_query(source, key_expr, keys, &capabilities)
                .into_statement());
        }

        let projection = self.projection(&table);
        let base = self
            .rows_query(source, &key_expr, keys, &capabilities)
            .select_items(projection)
            .select_expr_as(key_expr.clone(), Ident::from_static(KEY_ALIAS));

        let Some(limit) = self.limit_per_parent else {
            let ordered = self
                .order
                .iter()
                .fold(base, |query, term| query.order_by(term.clone()));
            return Ok(ordered.into_statement());
        };

        match strategy {
            LimitStrategy::Window => {
                self.window_limited(base, &key_expr, &target_key, limit, backend)
            }
            LimitStrategy::CorrelatedCount => {
                self.correlated_limited(base, &target_key, limit, backend)
            }
        }
    }

    /// The `FROM` clause and the expression the rows group by.
    fn source_and_key(
        &self,
        table: &TableRef,
        target_key: &Ident,
        backend: Backend,
    ) -> Result<(SqlSelect, Expr)> {
        match self.kind {
            RelationKind::BelongsTo => Ok((
                SqlSelect::from_table(table.clone()),
                Expr::column(table.column(target_key.clone())),
            )),
            RelationKind::HasMany | RelationKind::HasOne => {
                let key = self.foreign_key(backend)?;
                Ok((
                    SqlSelect::from_table(table.clone()),
                    Expr::column(table.column(key)),
                ))
            }
            RelationKind::ManyToMany => {
                let (join, left, right) = self.join_table_parts(backend)?;
                let query = SqlSelect::from_table(table.clone()).inner_join(
                    FromItem::table(join.clone()),
                    Expr::column(join.column(Ident::from_static(right)))
                        .eq(Expr::column(table.column(target_key.clone()))),
                );
                Ok((query, Expr::column(join.column(Ident::from_static(left)))))
            }
        }
    }

    /// The target's columns, qualified, in `Entity::from_row` order.
    fn projection(&self, table: &TableRef) -> Vec<SelectItem> {
        self.target_columns
            .iter()
            .map(|column| SelectItem::column(table.column(column.clone())))
            .collect()
    }

    /// The row-fetching query, before the projection and any per-parent limit.
    fn rows_query(
        &self,
        query: SqlSelect,
        key_expr: &Expr,
        keys: &[Value],
        capabilities: &Capabilities,
    ) -> SqlSelect {
        let query = query.filter(key_match(key_expr.clone(), keys, capabilities));
        self.filters
            .iter()
            .fold(query, |query, filter| query.filter(filter.expr().clone()))
    }

    /// `SELECT key, count(*) … GROUP BY key`.
    fn count_query(
        &self,
        query: SqlSelect,
        key_expr: Expr,
        keys: &[Value],
        capabilities: &Capabilities,
    ) -> SqlSelect {
        self.rows_query(query, &key_expr, keys, capabilities)
            .select_expr_as(key_expr.clone(), Ident::from_static(KEY_ALIAS))
            .select_expr(Aggregate::count_star().into_expr())
            .group_by(key_expr)
    }

    /// The `ROW_NUMBER()` form of a per-parent limit.
    fn window_limited(
        &self,
        base: SqlSelect,
        key_expr: &Expr,
        target_key: &Ident,
        limit: u32,
        backend: Backend,
    ) -> Result<Statement> {
        if !backend.dialect().capabilities().window_functions {
            return Err(Error::Unsupported {
                feature: "a per-parent preload limit by window function",
                backend,
            });
        }
        let mut window = WindowSpec::new().partition_by(key_expr.clone());
        for term in self.ordering(target_key) {
            window = window.order_by(term);
        }
        let ranked = base.select_expr_as(
            WindowExpr::new(WindowFunc::RowNumber, [], window).into_expr(),
            Ident::from_static(ROW_ALIAS),
        );
        Ok(SqlSelect::new()
            .from(FromItem::subquery(ranked, Ident::from_static(SCOPE_ALIAS)))
            .select_all()
            .filter(Expr::col(Ident::from_static(ROW_ALIAS)).le(Expr::value(i64::from(limit))))
            .order_by(OrderTerm::asc(Expr::col(Ident::from_static(KEY_ALIAS))))
            .order_by(OrderTerm::asc(Expr::col(Ident::from_static(ROW_ALIAS))))
            .into_statement())
    }

    /// The correlated-count form of a per-parent limit, for a dialect with no
    /// window functions.
    ///
    /// ```text
    /// WITH moso_children AS (…)
    /// SELECT * FROM moso_children
    /// WHERE (SELECT count(*) FROM moso_children AS moso_peer
    ///        WHERE moso_peer.moso_key = moso_children.moso_key
    ///          AND <peer sorts strictly before this row>) < n
    /// ```
    fn correlated_limited(
        &self,
        base: SqlSelect,
        target_key: &Ident,
        limit: u32,
        backend: Backend,
    ) -> Result<Statement> {
        if !backend.dialect().capabilities().ctes {
            return Err(Error::Unsupported {
                feature: "a per-parent preload limit without window functions",
                backend,
            });
        }
        let scope = TableRef::from_static(SCOPE_ALIAS);
        let peer = TableRef::from_static(PEER_ALIAS);
        let ordering = self.ordering(target_key);
        let before = precedes(&peer, &scope, &ordering, backend)?;

        let peers = SqlSelect::new()
            .from(FromItem::table_as(
                scope.clone(),
                Ident::from_static(PEER_ALIAS),
            ))
            .select_expr(Aggregate::count_star().into_expr())
            .filter(
                Expr::column(peer.column(Ident::from_static(KEY_ALIAS)))
                    .eq(Expr::column(scope.column(Ident::from_static(KEY_ALIAS)))),
            )
            .filter(before);

        let mut outer = SqlSelect::from_table(scope.clone())
            .with(Cte::new(Ident::from_static(SCOPE_ALIAS), base))
            .select_all()
            .filter(Expr::scalar(peers).lt(Expr::value(i64::from(limit))))
            .order_by(OrderTerm::asc(Expr::column(
                scope.column(Ident::from_static(KEY_ALIAS)),
            )));
        for term in ordering {
            let column = column_of(&term, backend)?;
            outer = outer.order_by(
                OrderTerm::new(Expr::column(scope.column(column)), term.order())
                    .with_nulls(term.nulls()),
            );
        }
        Ok(outer.into_statement())
    }

    /// The ordering a per-parent limit ranks by: what the user asked for, plus
    /// the target's primary key so that ties break the same way on every
    /// backend and on every run.
    fn ordering(&self, target_key: &Ident) -> Vec<OrderTerm> {
        let mut ordering = self.order.clone();
        let already = ordering.iter().any(|term| {
            term.expr()
                .as_column()
                .is_some_and(|column| column.name() == target_key)
        });
        if !already {
            ordering.push(OrderTerm::asc(Expr::col(target_key.clone())));
        }
        ordering
    }

    /// The foreign key a `has_many`/`has_one` batches on.
    fn foreign_key(&self, backend: Backend) -> Result<Ident> {
        self.key.map(Ident::from_static).ok_or(Error::Unsupported {
            feature: "a preload of a relation whose foreign key was never named",
            backend,
        })
    }

    /// The join table of a `many_to_many`, and its two key columns.
    fn join_table_parts(&self, backend: Backend) -> Result<(TableRef, &'static str, &'static str)> {
        let (table, left, right) = self.through.ok_or(Error::Unsupported {
            feature: "a preload of a many-to-many with no join table",
            backend,
        })?;
        Ok((TableRef::from_static(table), left, right))
    }

    /// One payload per key, in the order the keys were given.
    ///
    /// **One** statement, whatever `keys.len()` is: duplicates are collapsed
    /// before the statement is built and expanded again afterwards, so a
    /// hundred posts by ten authors fetch ten rows and every post still gets
    /// its own. Zero statements when no key is worth asking about — an empty
    /// batch, or one whose keys are all `NULL`.
    ///
    /// Nested preloads run inside, on every child of every parent at once,
    /// which is what keeps a second level at `+1` statement.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`]: the statement may not build, and the rows may not
    /// decode.
    ///
    /// ```no_run
    /// # use moso_orm::{Handle, Preload, Result};
    /// # use moso_orm::relation::LoadedRows;
    /// # use moso_sql::Value;
    /// async fn load(node: &Preload, keys: &[Value], handle: Handle<'_>)
    /// -> Result<Vec<LoadedRows>> {
    ///     node.payloads(keys, handle).await
    /// }
    /// ```
    pub async fn payloads(&self, keys: &[Value], handle: Handle<'_>) -> Result<Vec<LoadedRows>> {
        // The distinct, non-null keys are what the statement asks for.
        let mut seen = HashSet::new();
        let mut wanted = Vec::new();
        for key in keys {
            if !matches!(key, Value::Null(_)) && seen.insert(RelationKey::of(key)) {
                wanted.push(key.clone());
            }
        }

        // Every parent whose key matched nothing — and every parent at all,
        // when no key survived — is served from an empty group, which costs no
        // statement at all.
        let (rows, groups) = if wanted.is_empty() {
            (Vec::new(), vec![Vec::new(); keys.len()])
        } else {
            let statement = self.statement(&wanted, handle.backend())?;
            observe(&statement, self.at);
            let rows = handle.fetch_all(&statement).await?;
            let groups = group_rows(self, &rows, keys)?;
            (rows, groups)
        };

        if self.counting {
            return counted_payloads(self, &rows, &groups);
        }
        let load = self.load.ok_or_else(Self::not_runnable)?;
        load(self, &rows, &groups, handle).await
    }

    /// The error for planning a node that only describes a relation.
    fn not_runnable() -> Error {
        Error::Build(moso_sql::Error::incomplete(
            "preload",
            "the related entity's table",
            "build the node from a relation constant — `Post::COMMENTS` — or with \
             `Preload::of::<Comment>(..)`; `Preload::new` describes a relation without being able \
             to load one",
        ))
    }
}

impl fmt::Debug for Preload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Preload")
            .field("relation", &self.relation)
            .field("kind", &self.kind)
            .field("target", &self.target)
            .field("filters", &self.filters.len())
            .field("order", &self.order.len())
            .field("limit_per_parent", &self.limit_per_parent)
            .field("counting", &self.counting)
            .field("nested", &self.nested.len())
            .field("runnable", &self.is_runnable())
            .finish_non_exhaustive()
    }
}

/// How a per-parent limit is expressed.
///
/// The two forms return the same rows in the same order, which is asserted
/// against a real database rather than asserted in prose: the integration test
/// runs both against the same fixture and compares.
///
/// ```
/// use moso_orm::Backend;
/// use moso_orm::relation::LimitStrategy;
///
/// assert_eq!(LimitStrategy::for_backend(Backend::Postgres), LimitStrategy::Window);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LimitStrategy {
    /// `ROW_NUMBER() OVER (PARTITION BY key ORDER BY …)`, filtered to `<= n`.
    Window,
    /// A CTE plus a correlated `count(*)` of the rows that sort before this
    /// one, filtered to `< n`. For a dialect with no window functions.
    CorrelatedCount,
}

impl LimitStrategy {
    /// The strategy `backend` can run.
    ///
    /// ```
    /// use moso_orm::Backend;
    /// use moso_orm::relation::LimitStrategy;
    ///
    /// // SQLite has had window functions since 3.25; the correlated form is
    /// // the fallback for anything older, and is tested on both.
    /// assert_eq!(LimitStrategy::for_backend(Backend::Sqlite), LimitStrategy::Window);
    /// ```
    #[must_use]
    pub fn for_backend(backend: Backend) -> Self {
        if backend.dialect().capabilities().window_functions {
            Self::Window
        } else {
            Self::CorrelatedCount
        }
    }
}

/// `key = ANY($1)` where the dialect has arrays, `key IN (…)` where it does not.
///
/// The array form matters: it keeps the statement text identical for one key and
/// for a thousand, so the server's plan cache is not thrashed by a query whose
/// shape depends on how many parents happened to be on the page.
pub(crate) fn key_match(key: Expr, keys: &[Value], capabilities: &Capabilities) -> Expr {
    let element = keys.first().map_or(ValueKind::Unknown, Value::kind);
    if capabilities.arrays {
        key.any(
            BinOp::Eq,
            Expr::bound(Value::Array(Array::new(element, keys.to_vec()))),
        )
    } else {
        key.in_list(keys.iter().cloned().map(Expr::bound))
    }
}

/// The negation of [`key_match`]: `key <> ALL($1)`, or `key NOT IN (…)`.
///
/// What `sync` deletes: everything for this owner that is not in the new set.
pub(crate) fn key_mismatch(key: Expr, keys: &[Value], capabilities: &Capabilities) -> Expr {
    let element = keys.first().map_or(ValueKind::Unknown, Value::kind);
    if capabilities.arrays {
        key.all(
            BinOp::NotEq,
            Expr::bound(Value::Array(Array::new(element, keys.to_vec()))),
        )
    } else {
        key.not_in_list(keys.iter().cloned().map(Expr::bound))
    }
}

/// `peer sorts strictly before row`, for the correlated form of a per-parent
/// limit: lexicographic over the ordering terms.
fn precedes(
    peer: &TableRef,
    row: &TableRef,
    ordering: &[OrderTerm],
    backend: Backend,
) -> Result<Expr> {
    let Some((first, rest)) = ordering.split_first() else {
        return Ok(Expr::value(false));
    };
    let column = column_of(first, backend)?;
    let left = Expr::column(peer.column(column.clone()));
    let right = Expr::column(row.column(column));
    let strictly = match first.order() {
        // `Order` is `#[non_exhaustive]`; anything that is not descending sorts
        // ascending, which is also what the dialects render for a new variant
        // they do not know.
        Order::Desc => left.clone().gt(right.clone()),
        _ => left.clone().lt(right.clone()),
    };
    if rest.is_empty() {
        return Ok(strictly);
    }
    Ok(strictly.or(left.eq(right).and(precedes(peer, row, rest, backend)?)))
}

/// The column an ordering term names, or an error saying why a per-parent limit
/// needs one.
fn column_of(term: &OrderTerm, backend: Backend) -> Result<Ident> {
    term.expr()
        .as_column()
        .map(|column| column.name().clone())
        .ok_or(Error::Unsupported {
            feature: "a per-parent preload limit ordered by an expression rather than a column",
            backend,
        })
}

/// `E`'s first primary-key column.
///
/// An entity without one cannot exist — the derive refuses it — so the fallback
/// keeps this infallible without hiding a bug.
fn primary_key_of<E: Entity>() -> Ident {
    E::COLUMNS
        .iter()
        .find(|column| column.is_primary_key())
        .map_or_else(|| Ident::from_static("id"), crate::ColumnDef::ident)
}

/// Runs every preload in `preloads` against `parents`.
///
/// One statement per node, whatever `parents.len()` is. This is the function
/// [`Select::fetch_all`](crate::Select::fetch_all) calls once it has decoded its
/// rows, and the one [`load_many`](crate::relation::load_many) calls for a batch
/// somebody already has.
///
/// # Errors
///
/// Anything in [`Error`]: the statement may not build, the rows may not decode,
/// and a hand-written relation constant may be missing its setter.
///
/// ```no_run
/// # use moso_orm::{Entity, Handle, Preload, Result};
/// # use moso_orm::relation::run_preloads;
/// async fn eager<E: Entity>(
///     preloads: &[Preload],
///     parents: &mut [E],
///     handle: Handle<'_>,
/// ) -> Result<()> {
///     run_preloads(preloads, parents, handle).await
/// }
/// ```
pub async fn run_preloads<E: Entity>(
    preloads: &[Preload],
    parents: &mut [E],
    handle: Handle<'_>,
) -> Result<()> {
    for preload in preloads {
        run_one(preload, parents, handle).await?;
    }
    Ok(())
}

/// One node: collect, deduplicate, fetch once, group, link.
async fn run_one<E: Entity>(
    preload: &Preload,
    parents: &mut [E],
    handle: Handle<'_>,
) -> Result<()> {
    if parents.is_empty() {
        return Ok(());
    }
    let link = preload
        .link_fn::<E>()
        .ok_or_else(|| missing_link::<E>(preload))?;

    // The key each parent groups by, in parent order. A `belongs_to` batches on
    // the owner's *foreign key*, so falling back to its primary key would fetch
    // the wrong rows and say nothing — the one failure mode this module exists
    // to prevent. It is refused instead.
    let keys: Vec<Value> = match preload.parent_key_fn::<E>() {
        Some(read) => parents.iter().map(read).collect(),
        None if preload.kind() == RelationKind::BelongsTo => {
            return Err(missing_parent_key::<E>(preload));
        }
        None => parents
            .iter()
            .map(|parent| parent.pk().into_value())
            .collect(),
    };

    let payloads = preload.payloads(&keys, handle).await?;
    for (parent, payload) in parents.iter_mut().zip(payloads) {
        link(parent, payload)?;
    }
    Ok(())
}

/// The indices of `rows` that belong to each parent, in parent order.
///
/// Two parents with the same key both get every matching row, which is why a
/// `belongs_to` shared by a hundred posts does not need the author to be
/// `Clone`.
fn group_rows(preload: &Preload, rows: &[Row], keys: &[Value]) -> Result<Vec<Vec<usize>>> {
    let kind = keys
        .iter()
        .find(|key| !matches!(key, Value::Null(_)))
        .map_or(ValueKind::Unknown, Value::kind);
    let index = if preload.is_counting() {
        0
    } else {
        preload.target_columns.len()
    };

    let mut by_key: HashMap<RelationKey, Vec<usize>> = HashMap::new();
    for (position, row) in rows.iter().enumerate() {
        let key = read_key(row, index, kind, preload)?;
        by_key
            .entry(RelationKey::of(&key))
            .or_default()
            .push(position);
    }
    Ok(keys
        .iter()
        .map(|key| {
            by_key
                .get(&RelationKey::of(key))
                .cloned()
                .unwrap_or_default()
        })
        .collect())
}

/// One count per parent, for a `.with_count(..)` node.
fn counted_payloads(
    preload: &Preload,
    rows: &[Row],
    groups: &[Vec<usize>],
) -> Result<Vec<LoadedRows>> {
    groups
        .iter()
        .map(|group| {
            let count = match group.first() {
                Some(&position) => rows[position].get_i64(1)?,
                None => 0,
            };
            Ok(LoadedRows::counted(
                preload.relation(),
                preload.target(),
                count,
            ))
        })
        .collect()
}

/// Reads the grouping key out of a child row.
///
/// The key's *kind* comes from the parents, which is why this does not need the
/// related entity's type: whatever the parents' keys are, the children's must
/// compare equal to them.
fn read_key(row: &Row, index: usize, kind: ValueKind, preload: &Preload) -> Result<Value> {
    let value = match kind {
        ValueKind::Bool => Value::Bool(row.get_bool(index)?),
        ValueKind::I8 | ValueKind::I16 | ValueKind::U8 => Value::I16(row.get_i16(index)?),
        ValueKind::I32 | ValueKind::U16 => Value::I32(row.get_i32(index)?),
        ValueKind::I64 | ValueKind::U32 | ValueKind::U64 => Value::I64(row.get_i64(index)?),
        ValueKind::Text | ValueKind::Json => Value::text(row.get_string(index)?),
        ValueKind::Bytes => Value::bytes(row.get_bytes(index)?),
        ValueKind::Uuid => {
            Value::Uuid(moso_sql::Uuid::from_bytes(*row.get_uuid(index)?.as_bytes()))
        }
        // A float, a timestamp, an interval or an array is not a key: two of
        // them can be equal in SQL and hash differently here, which would put a
        // child in the wrong parent.
        _ => {
            return Err(Error::Decode(
                crate::DecodeError::type_mismatch(index, "a key column", format!("{kind:?}"))
                    .in_entity(preload.target())
                    .in_field(preload.relation())
                    .with_detail(
                        "a relation batches on a key it can compare and hash; make the key an \
                         integer, a uuid or text",
                    ),
            ));
        }
    };
    Ok(value)
}

/// Decode, recurse, regroup — the one operation that needs the related type.
///
/// Nested preloads run once over **every** child of **every** parent, which is
/// what makes a second level `+1` statement instead of `+1 per parent`.
fn load_rows<'a, T: Entity>(
    preload: &'a Preload,
    rows: &'a [Row],
    groups: &'a [Vec<usize>],
    handle: Handle<'a>,
) -> BoxFuture<'a, Result<Vec<LoadedRows>>> {
    Box::pin(async move {
        let mut flat: Vec<T> = Vec::with_capacity(rows.len());
        let mut bounds = Vec::with_capacity(groups.len());
        for group in groups {
            let start = flat.len();
            for &position in group {
                flat.push(T::from_row(&rows[position])?);
            }
            bounds.push(start);
        }

        if !preload.nested().is_empty() {
            run_preloads::<T>(preload.nested(), &mut flat, handle).await?;
        }

        // Split the flat vector back into per-parent runs. Walking backwards
        // means every `split_off` takes exactly one group.
        let mut payloads = Vec::with_capacity(bounds.len());
        for &start in bounds.iter().rev() {
            let tail = flat.split_off(start);
            payloads.push(LoadedRows::rows(preload.relation(), preload.target(), tail));
        }
        payloads.reverse();
        Ok(payloads)
    })
}

/// The error for a `belongs_to` with no way to read the owner's foreign key.
///
/// Batching on the primary key instead would return *some* rows — the ones
/// whose id happens to equal the parent's — which is worse than an error,
/// because nothing about it looks wrong.
fn missing_parent_key<E: Entity>(preload: &Preload) -> Error {
    tracing::debug!(
        target: "moso::orm",
        entity = E::NAME,
        relation = preload.relation(),
        "this `belongs_to` constant cannot read its own foreign key"
    );
    Error::Build(moso_sql::Error::incomplete(
        "preload",
        "a way to read the owner's foreign key",
        "a `belongs_to` batches on the owner's key column, not on its primary key; \
         `#[derive(Entity)]` supplies the reader, and a hand-written constant needs \
         `.keyed_by(|owner| owner.author_id.to_value())`",
    ))
}

/// The error for a relation constant with no field setter.
///
/// Unreachable from generated code — the derive emits the setter next to the
/// constant — so the message is aimed at the person who wrote the constant by
/// hand, and the `debug` record names which relation of which entity it was.
fn missing_link<E: Entity>(preload: &Preload) -> Error {
    tracing::debug!(
        target: "moso::orm",
        entity = E::NAME,
        relation = preload.relation(),
        "this relation constant carries no field setter"
    );
    Error::Build(moso_sql::Error::incomplete(
        "preload",
        "the setter that puts the loaded rows into the field",
        "`#[derive(Entity)]` generates one per relation; a hand-written relation constant needs \
         `.linking(|owner, rows| { owner.field = Related::Loaded(rows.into_rows()?); Ok(()) })`",
    ))
}

/// A relation key in a form that can be hashed, so grouping a thousand children
/// across a hundred parents is linear rather than quadratic.
///
/// [`Value`] cannot be the key itself: it holds floats, so it is `PartialEq` and
/// not `Eq`. Every kind a relation can key on — integers, text, bytes, uuids —
/// maps exactly; the rest fall back on a canonical rendering, which is still
/// injective because the enum is plain data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RelationKey {
    /// Any integer or boolean, widened.
    Int(i128),
    /// `text`.
    Text(String),
    /// `bytea`, and `uuid` as its sixteen bytes.
    Bytes(Vec<u8>),
    /// Anything else, canonically rendered.
    Other(String),
}

impl RelationKey {
    /// The hashable form of `value`.
    fn of(value: &Value) -> Self {
        match value {
            Value::Bool(flag) => Self::Int(i128::from(*flag)),
            Value::I8(n) => Self::Int(i128::from(*n)),
            Value::I16(n) => Self::Int(i128::from(*n)),
            Value::I32(n) => Self::Int(i128::from(*n)),
            Value::I64(n) => Self::Int(i128::from(*n)),
            Value::U8(n) => Self::Int(i128::from(*n)),
            Value::U16(n) => Self::Int(i128::from(*n)),
            Value::U32(n) => Self::Int(i128::from(*n)),
            Value::U64(n) => Self::Int(i128::from(*n)),
            Value::Text(text) => Self::Text(text.clone()),
            Value::Bytes(bytes) => Self::Bytes(bytes.clone()),
            Value::Uuid(uuid) => Self::Bytes(uuid.into_bytes().to_vec()),
            other => Self::Other(format!("{other:?}")),
        }
    }
}

// ── The N+1 detector ────────────────────────────────────────────────────────

/// What one request did to the database, for the N+1 warning.
///
/// A detector is installed for the duration of a future — a request, a job, a
/// test — and every statement that runs inside it is recorded by fingerprint.
/// When the future ends, a fingerprint that repeated more than the configured
/// threshold is a loop that should have been a preload, and it is logged with
/// the call site that issued it.
///
/// ```
/// use moso_orm::relation::NPlusOne;
///
/// let detector = NPlusOne::new(3);
/// for _ in 0..5 {
///     detector.record("SELECT FROM comments", None);
/// }
///
/// let report = detector.report().expect("over the threshold");
/// assert_eq!(report.repeats(), 5);
/// assert_eq!(report.statement(), "SELECT FROM comments");
/// ```
#[derive(Debug)]
pub struct NPlusOne {
    threshold: u32,
    seen: Mutex<HashMap<String, Repeat>>,
}

/// How often one fingerprint ran, and where from.
#[derive(Clone, Copy, Debug, Default)]
struct Repeat {
    count: u32,
    at: Option<CallSite>,
}

impl NPlusOne {
    /// A detector that warns above `threshold` repeats of one statement.
    ///
    /// ```
    /// use moso_orm::relation::NPlusOne;
    ///
    /// assert_eq!(NPlusOne::new(20).threshold(), 20);
    /// ```
    #[must_use]
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// A detector at the threshold the database is configured with, ready to
    /// install with [`detect`].
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    /// use moso_orm::relation::NPlusOne;
    ///
    /// let config = DatabaseConfig::from_url("sqlite://:memory:");
    /// assert_eq!(NPlusOne::configured(&config).threshold(), 20);
    /// ```
    #[must_use]
    pub fn configured(config: &crate::DatabaseConfig) -> Arc<Self> {
        Arc::new(Self::new(config.n_plus_one_threshold))
    }

    /// The threshold this detector warns above.
    ///
    /// ```
    /// # use moso_orm::relation::NPlusOne;
    /// assert_eq!(NPlusOne::new(5).threshold(), 5);
    /// ```
    #[must_use]
    pub const fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Records one statement.
    ///
    /// ```
    /// # use moso_orm::relation::NPlusOne;
    /// let detector = NPlusOne::new(1);
    /// detector.record("SELECT FROM users", None);
    /// assert!(detector.report().is_none(), "one is not a loop");
    /// ```
    pub fn record(&self, fingerprint: &str, at: Option<CallSite>) {
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        let entry = seen.entry(fingerprint.to_owned()).or_default();
        entry.count = entry.count.saturating_add(1);
        if entry.at.is_none() {
            entry.at = at;
        }
    }

    /// The worst repeat, when one is over the threshold.
    ///
    /// ```
    /// # use moso_orm::relation::NPlusOne;
    /// let detector = NPlusOne::new(2);
    /// detector.record("SELECT FROM posts", None);
    /// detector.record("SELECT FROM posts", None);
    /// detector.record("SELECT FROM posts", None);
    /// assert!(detector.report().is_some());
    /// ```
    #[must_use]
    pub fn report(&self) -> Option<NPlusOneReport> {
        let seen = self.seen.lock().ok()?;
        let (fingerprint, worst) = seen
            .iter()
            .max_by_key(|(_, repeat)| repeat.count)
            .filter(|(_, repeat)| repeat.count > self.threshold)?;
        Some(NPlusOneReport {
            statement: fingerprint.clone(),
            repeats: worst.count,
            threshold: self.threshold,
            at: worst.at,
        })
    }

    /// Logs a warning when a statement repeated more than the threshold.
    ///
    /// This is what a request emits on the way out in `dev`.
    ///
    /// ```
    /// # use moso_orm::relation::NPlusOne;
    /// NPlusOne::new(0).warn_if_over();
    /// ```
    pub fn warn_if_over(&self) {
        let Some(report) = self.report() else {
            return;
        };
        tracing::warn!(
            target: "moso::orm",
            statement = %report.statement(),
            repeats = report.repeats(),
            threshold = report.threshold(),
            at = %report.location(),
            "{report}"
        );
    }
}

/// A statement that ran too many times in one request.
///
/// ```
/// use moso_orm::relation::NPlusOne;
///
/// let detector = NPlusOne::new(1);
/// detector.record("SELECT FROM comments", None);
/// detector.record("SELECT FROM comments", None);
/// let report = detector.report().expect("over");
/// assert!(report.to_string().contains("SELECT FROM comments"));
/// assert!(report.to_string().contains("help:"));
/// ```
#[derive(Clone, Debug)]
pub struct NPlusOneReport {
    statement: String,
    repeats: u32,
    threshold: u32,
    at: Option<CallSite>,
}

impl NPlusOneReport {
    /// The statement's fingerprint.
    ///
    /// ```
    /// # use moso_orm::relation::NPlusOne;
    /// # let d = NPlusOne::new(0); d.record("SELECT FROM t", None);
    /// assert_eq!(d.report().unwrap().statement(), "SELECT FROM t");
    /// ```
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }

    /// How many times it ran.
    ///
    /// ```
    /// # use moso_orm::relation::NPlusOne;
    /// # let d = NPlusOne::new(0); d.record("SELECT FROM t", None);
    /// assert_eq!(d.report().unwrap().repeats(), 1);
    /// ```
    #[must_use]
    pub const fn repeats(&self) -> u32 {
        self.repeats
    }

    /// The threshold it went over.
    ///
    /// ```
    /// # use moso_orm::relation::NPlusOne;
    /// # let d = NPlusOne::new(0); d.record("SELECT FROM t", None);
    /// assert_eq!(d.report().unwrap().threshold(), 0);
    /// ```
    #[must_use]
    pub const fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Where it was issued from, when the call site was recorded.
    ///
    /// ```
    /// # use moso_orm::relation::NPlusOne;
    /// # let d = NPlusOne::new(0); d.record("SELECT FROM t", None);
    /// assert!(d.report().unwrap().call_site().is_none());
    /// ```
    #[must_use]
    pub const fn call_site(&self) -> Option<CallSite> {
        self.at
    }

    /// The call site as `file:line`, or `an unrecorded call site`.
    ///
    /// ```
    /// # use moso_orm::relation::NPlusOne;
    /// # let d = NPlusOne::new(0); d.record("SELECT FROM t", None);
    /// assert_eq!(d.report().unwrap().location(), "an unrecorded call site");
    /// ```
    #[must_use]
    pub fn location(&self) -> String {
        self.at.map_or_else(
            || String::from("an unrecorded call site"),
            |at| format!("{}:{}", at.file(), at.line()),
        )
    }
}

impl fmt::Display for NPlusOneReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "this request ran `{}` {} times (threshold {})",
            self.statement, self.repeats, self.threshold
        )?;
        writeln!(f, "  issued from {}", self.location())?;
        write!(
            f,
            "  help: load the relation once instead — `Parent::query().with(Parent::CHILDREN)` — \
             or, for rows you already have, `Parent::load_many(&mut rows, Parent::CHILDREN, &db)`"
        )
    }
}

/// The detector installed for the current task, if any.
///
/// A `tokio` task-local rather than a global: two requests on one runtime must
/// not pollute each other's count, and a global would need a request id
/// threaded through every call anyway.
mod scope {
    tokio::task_local! {
        /// The detector for the running request.
        pub static DETECTOR: std::sync::Arc<super::NPlusOne>;
    }
}

/// Runs `future` with `detector` installed, and warns on the way out.
///
/// The middleware and `moso-test` wrap a request in this; nothing else has to
/// know the detector exists.
///
/// ```
/// use moso_orm::relation::{NPlusOne, detect};
/// use std::sync::Arc;
///
/// async fn handle_a_request() {
///     let detector = Arc::new(NPlusOne::new(20));
///     detect(Arc::clone(&detector), async { /* run the handler */ }).await;
///     assert!(detector.report().is_none());
/// }
/// ```
pub async fn detect<F: Future>(detector: Arc<NPlusOne>, future: F) -> F::Output {
    let installed = Arc::clone(&detector);
    let output = scope::DETECTOR.scope(installed, future).await;
    detector.warn_if_over();
    output
}

/// Records `statement` against the running task's detector, if there is one.
///
/// Cheap when there is not: one task-local lookup that misses.
///
/// ```
/// use moso_orm::relation::observe;
/// use moso_sql::{Select, Statement};
///
/// // Outside a `detect(..)` scope this does nothing at all.
/// observe(&Select::new().into_statement(), None);
/// ```
pub fn observe(statement: &Statement, at: Option<CallSite>) {
    let _ = scope::DETECTOR.try_with(|detector| {
        detector.record(&fingerprint(statement), at);
    });
}

/// Records already-rendered SQL against the running task's detector.
///
/// This is the hook the execution layer wants: one line in
/// [`Handle::fetch_all_sql`](crate::Handle::fetch_all_sql) and its siblings
/// makes the detector see **every** statement, not only the preloads — and the
/// N+1 it is looking for is a loop of `fetch_one`s, which never comes through
/// [`observe`].
///
/// The text is the fingerprint: two runs of the same query through the same
/// code path render identically, and the parameters are bound, not
/// interpolated, so no value can leak into a log line.
///
/// ```
/// use moso_orm::relation::observe_sql;
///
/// // Outside a `detect(..)` scope this does nothing at all.
/// observe_sql("SELECT id FROM users WHERE id = $1", None);
/// ```
pub fn observe_sql(sql: &str, at: Option<CallSite>) {
    let _ = scope::DETECTOR.try_with(|detector| {
        detector.record(sql, at);
    });
}

/// A stable, readable identity for a statement, without rendering it.
///
/// Two runs of the same query through the same code path produce the same
/// string, which is all a repeat counter needs — and it never contains a bound
/// value, so a warning cannot leak a password into a log.
///
/// ```
/// use moso_orm::relation::fingerprint;
/// use moso_sql::{Select, TableRef};
///
/// let query = Select::from_table(TableRef::from_static("comments")).select_all();
/// assert_eq!(fingerprint(&query.into_statement()), "SELECT FROM comments");
/// ```
#[must_use]
pub fn fingerprint(statement: &Statement) -> String {
    match statement.borrowed() {
        StatementRef::Select(query) => match query.from_items().first() {
            Some(FromItem::Table { table, .. }) => {
                format!("SELECT FROM {}", table.name().as_str())
            }
            _ => String::from("SELECT"),
        },
        StatementRef::Insert(insert) => {
            format!("INSERT INTO {}", insert.table().name().as_str())
        }
        StatementRef::Delete(delete) => {
            format!("DELETE FROM {}", delete.target().name().as_str())
        }
        other => other.kind().as_str().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColumnDef;
    use crate::descriptor::EntityDescriptor;
    use crate::row::DecodeError;
    use moso_sql::{Quantifier, SelectItem};
    use std::sync::OnceLock;

    /// A comment, whose `post_id` is the foreign key a `has_many` batches on.
    #[derive(Clone, Debug)]
    struct Comment {
        id: i64,
    }

    /// A user, loaded by primary key for a `belongs_to`.
    #[derive(Clone, Debug)]
    struct User {
        id: i64,
    }

    /// A tag, reached through a join table.
    #[derive(Clone, Debug)]
    struct Tag {
        id: i64,
    }

    macro_rules! fixture_entity {
        ($name:ident, $table:literal, $columns:expr) => {
            impl Entity for $name {
                type Pk = i64;
                const TABLE: TableRef = TableRef::from_static($table);
                const COLUMNS: &'static [ColumnDef] = $columns;
                const NAME: &'static str = stringify!($name);
                fn pk(&self) -> i64 {
                    self.id
                }
                fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
                    Ok(Self {
                        id: row.get_i64(0)?,
                    })
                }
                fn descriptor() -> &'static EntityDescriptor {
                    static D: OnceLock<EntityDescriptor> = OnceLock::new();
                    D.get_or_init(|| {
                        EntityDescriptor::builder(stringify!($name), Self::TABLE).build()
                    })
                }
            }
        };
    }

    fixture_entity!(
        User,
        "users",
        &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("name", ValueKind::Text),
        ]
    );
    fixture_entity!(
        Tag,
        "tags",
        &[ColumnDef::new("id", ValueKind::I64).primary_key()]
    );
    fixture_entity!(
        Comment,
        "comments",
        &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("post_id", ValueKind::I64),
            ColumnDef::new("approved", ValueKind::Bool),
            ColumnDef::new("created_at", ValueKind::Timestamp),
        ]
    );

    /// `Post::COMMENTS`, as a node.
    fn comments() -> Preload {
        Preload::of::<Comment>("comments", RelationKind::HasMany).keyed("post_id")
    }

    /// `Post::AUTHOR`, as a node.
    fn author() -> Preload {
        Preload::of::<User>("author", RelationKind::BelongsTo).keyed("author_id")
    }

    /// `Post::TAGS`, as a node.
    fn tags() -> Preload {
        Preload::of::<Tag>("tags", RelationKind::ManyToMany).through(
            "post_tags",
            "post_id",
            "tag_id",
        )
    }

    /// `n` primary keys, as a hundred posts would produce.
    fn keys(n: i64) -> Vec<Value> {
        (1..=n).map(Value::I64).collect()
    }

    /// The `SELECT` inside a statement, for inspection.
    fn select_of(statement: &Statement) -> &SqlSelect {
        match statement {
            Statement::Select(query) => query,
            other => panic!("a preload issues a SELECT, not {:?}", other.kind()),
        }
    }

    /// How many keys the statement asks about, however the dialect spells it.
    fn keys_asked_for(statement: &Statement) -> usize {
        let filters = select_of(statement).filters();
        assert_eq!(filters.len(), 1, "one key filter and nothing else");
        match &filters[0] {
            Expr::Quantified {
                quantifier: Quantifier::Any,
                rhs,
                ..
            } => match rhs.as_ref() {
                Expr::Value(Value::Array(array)) => array.len(),
                other => panic!("the array form binds one array, not {other:?}"),
            },
            Expr::InList { items, .. } => items.len(),
            other => panic!("a preload matches keys, not {other:?}"),
        }
    }

    /// Non-negotiable N3, measured: the acceptance fixture is a hundred parents
    /// with ten children each, and the statement count does not depend on
    /// either number.
    #[test]
    fn n3_one_hundred_parents_and_a_thousand_children_are_two_statements() {
        let parents = 100;
        let node = comments();

        // The parent query is one; the preload adds exactly one more.
        assert_eq!(1 + node.statement_count(), 2);

        for backend in [Backend::Postgres, Backend::Sqlite] {
            let statement = node.statement(&keys(parents), backend).expect("plans");
            assert_eq!(
                keys_asked_for(&statement),
                usize::try_from(parents).expect("fits"),
                "one statement asks about every parent at once on {backend:?}"
            );
        }
    }

    /// Nested is `+1`, not `+1 per parent`.
    #[test]
    fn n3_a_nested_preload_is_three_statements() {
        let nested = comments().with(author());
        assert_eq!(1 + nested.statement_count(), 3);

        // And each level plans one statement, for any number of rows.
        let level_one = nested
            .statement(&keys(100), Backend::Postgres)
            .expect("plans");
        assert_eq!(keys_asked_for(&level_one), 100);
        let level_two = nested.nested()[0]
            .statement(&keys(1_000), Backend::Postgres)
            .expect("plans");
        assert_eq!(
            keys_asked_for(&level_two),
            1_000,
            "the second level batches every child of every parent"
        );
    }

    /// The statement's *shape* does not change with the parent count on a
    /// dialect with arrays, which is what keeps the server's plan cached.
    #[test]
    fn the_array_form_binds_one_parameter_however_many_parents_there_are() {
        let node = comments();
        for count in [1, 10, 100, 1_000] {
            let statement = node
                .statement(&keys(count), Backend::Postgres)
                .expect("plans");
            let filters = select_of(&statement).filters();
            assert!(
                matches!(filters[0], Expr::Quantified { .. }),
                "= ANY($1), not an IN list that grows"
            );
            assert_eq!(
                keys_asked_for(&statement),
                usize::try_from(count).expect("fits")
            );
        }
    }

    #[test]
    fn a_belongs_to_loads_the_target_by_its_primary_key() {
        let statement = author()
            .statement(&keys(3), Backend::Postgres)
            .expect("plans");
        let query = select_of(&statement);
        assert_eq!(query.from_items().len(), 1);
        assert!(query.joins().is_empty());
        // Two columns, then the grouping key.
        assert_eq!(query.items().len(), 3);
        assert_eq!(
            query.items()[2].alias().map(Ident::as_str),
            Some(KEY_ALIAS),
            "the key is projected last, under a known alias"
        );
    }

    #[test]
    fn a_has_many_loads_the_children_by_their_foreign_key() {
        let statement = comments()
            .statement(&keys(3), Backend::Postgres)
            .expect("plans");
        let query = select_of(&statement);
        assert_eq!(query.items().len(), 5, "four columns, then the key");
        assert_eq!(query.items()[4].alias().map(Ident::as_str), Some(KEY_ALIAS));
        let Expr::Quantified { lhs, .. } = &query.filters()[0] else {
            panic!("the array form");
        };
        let Expr::Column(column) = lhs.as_ref() else {
            panic!("keyed on a column");
        };
        assert_eq!(column.name().as_str(), "post_id");
    }

    #[test]
    fn a_many_to_many_is_one_statement_through_the_join_table() {
        let statement = tags()
            .statement(&keys(3), Backend::Postgres)
            .expect("plans");
        let query = select_of(&statement);
        assert_eq!(query.joins().len(), 1, "one join, and no second statement");
        let Expr::Quantified { lhs, .. } = &query.filters()[0] else {
            panic!("the array form");
        };
        let Expr::Column(column) = lhs.as_ref() else {
            panic!("keyed on a column");
        };
        assert_eq!(column.name().as_str(), "post_id");
        assert_eq!(
            column.qualifier().map(Ident::as_str),
            Some("post_tags"),
            "the owner's key lives on the join table"
        );
    }

    #[test]
    fn a_count_preload_groups_instead_of_fetching_rows() {
        let statement = comments()
            .counting()
            .statement(&keys(100), Backend::Postgres)
            .expect("plans");
        let query = select_of(&statement);
        assert_eq!(query.items().len(), 2, "the key and the count");
        assert_eq!(query.items()[0].alias().map(Ident::as_str), Some(KEY_ALIAS));
        assert_eq!(query.group_by_exprs().len(), 1);
        assert!(
            matches!(
                &query.items()[1],
                SelectItem::Expr {
                    expr: Expr::Aggregate(_),
                    ..
                }
            ),
            "count(*), not a thousand rows"
        );
    }

    #[test]
    fn filters_and_ordering_ride_along_in_the_one_statement() {
        let node = comments()
            .filter(Predicate::of(
                ["Comment"],
                Expr::col(Ident::from_static("approved")).eq(Expr::value(true)),
            ))
            .order_by(OrderTerm::desc(Expr::col(Ident::from_static("created_at"))));

        let statement = node.statement(&keys(10), Backend::Postgres).expect("plans");
        let query = select_of(&statement);
        assert_eq!(query.filters().len(), 2, "the key, and the user's filter");
        assert_eq!(query.order_terms().len(), 1);
    }

    #[test]
    fn a_per_parent_limit_ranks_with_a_window_where_the_dialect_has_one() {
        let node = comments()
            .order_by(OrderTerm::desc(Expr::col(Ident::from_static("created_at"))))
            .limit_per_parent(3);

        for backend in [Backend::Postgres, Backend::Sqlite] {
            let statement = node
                .statement_using(&keys(100), backend, LimitStrategy::Window)
                .expect("plans");
            let outer = select_of(&statement);

            let Some(FromItem::Subquery { query: inner, .. }) = outer.from_items().first() else {
                panic!("the ranked rows are a subquery");
            };
            let ranked = inner
                .items()
                .last()
                .expect("a projection")
                .alias()
                .map(Ident::as_str);
            assert_eq!(ranked, Some(ROW_ALIAS));
            assert!(
                matches!(
                    inner.items().last(),
                    Some(SelectItem::Expr {
                        expr: Expr::Window(_),
                        ..
                    })
                ),
                "ROW_NUMBER() OVER (PARTITION BY …)"
            );
            assert_eq!(outer.filters().len(), 1, "row_number <= 3");
            assert_eq!(
                outer.order_terms().len(),
                2,
                "by parent, then by rank, so the rows come back grouped and ordered"
            );
            // Still one statement, and still one key filter inside it.
            assert_eq!(keys_asked_for(&Statement::Select((**inner).clone())), 100);
        }
    }

    #[test]
    fn a_per_parent_limit_counts_peers_where_the_dialect_has_no_window() {
        let node = comments()
            .order_by(OrderTerm::desc(Expr::col(Ident::from_static("created_at"))))
            .limit_per_parent(3);

        for backend in [Backend::Postgres, Backend::Sqlite] {
            let statement = node
                .statement_using(&keys(100), backend, LimitStrategy::CorrelatedCount)
                .expect("plans");
            let outer = select_of(&statement);
            assert_eq!(outer.ctes().len(), 1, "the children are a CTE");
            assert_eq!(outer.filters().len(), 1);
            assert!(
                matches!(&outer.filters()[0], Expr::Binary { lhs, .. }
                    if matches!(lhs.as_ref(), Expr::Scalar(_))),
                "a correlated count(*) of the rows that sort before this one"
            );
            // The ordering is the user's, then the primary key, so that ties
            // break identically to the window form.
            assert_eq!(outer.order_terms().len(), 3, "key, created_at, id");
        }
    }

    #[test]
    fn both_limit_strategies_rank_by_the_same_terms() {
        let node = comments()
            .order_by(OrderTerm::desc(Expr::col(Ident::from_static("created_at"))))
            .limit_per_parent(3);
        let ordering = node.ordering(&Ident::from_static("id"));

        assert_eq!(ordering.len(), 2, "the user's term, then the tiebreaker");
        assert_eq!(
            ordering[1].expr().as_column().map(|c| c.name().as_str()),
            Some("id"),
            "a deterministic tiebreaker is what makes the two forms agree"
        );

        // Asking for the primary key explicitly does not add it twice.
        let explicit = comments()
            .order_by(OrderTerm::asc(Expr::col(Ident::from_static("id"))))
            .limit_per_parent(3);
        assert_eq!(explicit.ordering(&Ident::from_static("id")).len(), 1);
    }

    #[test]
    fn a_per_parent_limit_ordered_by_an_expression_says_what_to_do_instead() {
        let node = comments()
            .order_by(OrderTerm::desc(Expr::value(1_i64)))
            .limit_per_parent(3);
        let error = node
            .statement_using(&keys(3), Backend::Sqlite, LimitStrategy::CorrelatedCount)
            .expect_err("not a column");
        assert!(error.to_string().contains("help:"), "{error}");
    }

    #[test]
    fn a_column_subset_is_refused_rather_than_silently_shifting_the_decoder() {
        let node = comments().columns([Ident::from_static("id")]);
        let error = node
            .statement(&keys(1), Backend::Postgres)
            .expect_err("a narrowed row would not decode");
        assert!(error.to_string().contains("help:"), "{error}");

        // The full list in the declared order is a no-op, and plans.
        let full = comments().columns([
            Ident::from_static("id"),
            Ident::from_static("post_id"),
            Ident::from_static("approved"),
            Ident::from_static("created_at"),
        ]);
        assert!(full.statement(&keys(1), Backend::Postgres).is_ok());
    }

    #[test]
    fn a_relation_with_no_foreign_key_names_the_omission() {
        let node = Preload::of::<Comment>("comments", RelationKind::HasMany);
        let error = node
            .statement(&keys(1), Backend::Postgres)
            .expect_err("no foreign key");
        assert!(error.to_string().contains("foreign key"), "{error}");
    }

    #[test]
    fn a_preload_tree_costs_one_statement_per_node() {
        let flat = Preload::new("comments", RelationKind::HasMany, "Comment");
        assert_eq!(flat.statement_count(), 1);

        let nested = flat
            .clone()
            .with(Preload::new("author", RelationKind::BelongsTo, "User"));
        assert_eq!(nested.statement_count(), 2);

        let deeper = nested.with(Preload::new("tags", RelationKind::ManyToMany, "Tag"));
        assert_eq!(deeper.statement_count(), 3);
    }

    #[test]
    fn a_preload_keeps_its_refinements() {
        let refined = Preload::new("comments", RelationKind::HasMany, "Comment")
            .filter(Predicate::of(["Comment"], Expr::value(true)))
            .order_by(OrderTerm::desc(Expr::col(Ident::from_static("created_at"))))
            .limit_per_parent(3)
            .columns([Ident::from_static("id")]);

        assert_eq!(refined.filters().len(), 1);
        assert_eq!(refined.order_terms().len(), 1);
        assert_eq!(refined.limit_per_parent_value(), Some(3));
        assert_eq!(refined.column_names().len(), 1);
        assert!(!refined.is_counting());
    }

    #[test]
    fn a_described_relation_cannot_be_planned_and_says_why() {
        let described = Preload::new("comments", RelationKind::HasMany, "Comment");
        let error = described
            .statement(&[Value::I64(1)], Backend::Postgres)
            .expect_err("no target table");
        let text = error.to_string();
        assert!(text.contains("Preload::of"), "{text}");
        assert!(text.contains("help:"), "{text}");
    }

    #[test]
    fn a_relation_key_hashes_every_shape_a_key_can_be() {
        assert_eq!(RelationKey::of(&Value::I64(1)), RelationKey::Int(1));
        assert_eq!(RelationKey::of(&Value::I32(1)), RelationKey::Int(1));
        assert_eq!(
            RelationKey::of(&Value::text("a")),
            RelationKey::Text("a".into())
        );
        assert_ne!(
            RelationKey::of(&Value::I64(1)),
            RelationKey::of(&Value::I64(2))
        );
        // Widening must not make two different keys collide across types.
        assert_eq!(
            RelationKey::of(&Value::U8(7)),
            RelationKey::of(&Value::I64(7))
        );
    }

    #[test]
    fn the_detector_names_the_repeated_statement_and_its_call_site() {
        let site = CallSite::caller();
        let detector = NPlusOne::new(3);
        detector.record("SELECT FROM users", None);
        for _ in 0..4 {
            detector.record("SELECT FROM comments", Some(site));
        }

        let report = detector.report().expect("over the threshold");
        assert_eq!(report.statement(), "SELECT FROM comments");
        assert_eq!(report.repeats(), 4);
        assert!(
            report.location().contains("preload.rs"),
            "{}",
            report.location()
        );

        let text = report.to_string();
        assert!(text.contains("SELECT FROM comments"), "{text}");
        assert!(text.contains("load_many"), "{text}");
        assert!(text.contains("help:"), "{text}");
    }

    #[test]
    fn the_detector_is_quiet_below_the_threshold() {
        let detector = NPlusOne::new(20);
        for _ in 0..20 {
            detector.record("SELECT FROM comments", None);
        }
        assert!(detector.report().is_none(), "20 is not over 20");
        detector.record("SELECT FROM comments", None);
        assert!(detector.report().is_some(), "21 is");
    }

    #[test]
    fn observing_outside_a_scope_is_a_no_op() {
        observe(&SqlSelect::new().into_statement(), None);
    }

    /// The three newest approved comments per post.
    fn newest_three() -> Preload {
        approved()
            .order_by(OrderTerm::desc(Expr::column(
                Comment::TABLE.column(Ident::from_static("created_at")),
            )))
            .limit_per_parent(3)
    }

    /// Only the approved comments.
    fn approved() -> Preload {
        comments().filter(Predicate::of(
            ["Comment"],
            Expr::column(Comment::TABLE.column(Ident::from_static("approved")))
                .eq(Expr::value(true)),
        ))
    }

    /// Renders `node` with a forced limit strategy.
    fn render_using(node: &Preload, backend: Backend, strategy: LimitStrategy) -> String {
        node.statement_using(&keys(3), backend, strategy)
            .expect("plans")
            .build(backend.dialect())
            .expect("renders")
            .text
    }

    /// Renders `node` for `backend`, so the SQL itself can be asserted.
    fn render(node: &Preload, backend: Backend, count: i64) -> String {
        node.statement(&keys(count), backend)
            .expect("plans")
            .build(backend.dialect())
            .expect("renders")
            .text
    }

    /// D9: every construct gets a snapshot on **both** dialects.
    ///
    /// The point is not that the strings are pretty. It is that the array form
    /// and the `IN` list are the only difference between the two backends, that
    /// the grouping key is always the last projected column under a known
    /// alias, and that a per-parent limit is one statement either way.
    #[test]
    fn the_rendered_sql_says_what_it_does_on_both_dialects() {
        assert_eq!(
            render(&comments(), Backend::Postgres, 3),
            "SELECT \"comments\".\"id\", \"comments\".\"post_id\", \"comments\".\"approved\", \
             \"comments\".\"created_at\", \"comments\".\"post_id\" AS \"moso_key\" FROM \"comments\" WHERE \
             \"comments\".\"post_id\" = ANY ($1)",
            "has_many/postgres"
        );
        assert_eq!(
            render(&comments(), Backend::Sqlite, 3),
            "SELECT \"comments\".\"id\", \"comments\".\"post_id\", \"comments\".\"approved\", \
             \"comments\".\"created_at\", \"comments\".\"post_id\" AS \"moso_key\" FROM \"comments\" WHERE \
             \"comments\".\"post_id\" IN (?, ?, ?)",
            "has_many/sqlite"
        );
        assert_eq!(
            render(&author(), Backend::Postgres, 3),
            "SELECT \"users\".\"id\", \"users\".\"name\", \"users\".\"id\" AS \"moso_key\" FROM \"users\" WHERE \
             \"users\".\"id\" = ANY ($1)",
            "belongs_to/postgres"
        );
        assert_eq!(
            render(&author(), Backend::Sqlite, 3),
            "SELECT \"users\".\"id\", \"users\".\"name\", \"users\".\"id\" AS \"moso_key\" FROM \"users\" WHERE \
             \"users\".\"id\" IN (?, ?, ?)",
            "belongs_to/sqlite"
        );
        assert_eq!(
            render(&tags(), Backend::Postgres, 3),
            "SELECT \"tags\".\"id\", \"post_tags\".\"post_id\" AS \"moso_key\" FROM \"tags\" INNER JOIN \
             \"post_tags\" ON \"post_tags\".\"tag_id\" = \"tags\".\"id\" WHERE \"post_tags\".\"post_id\" = \
             ANY ($1)",
            "many_to_many/postgres"
        );
        assert_eq!(
            render(&tags(), Backend::Sqlite, 3),
            "SELECT \"tags\".\"id\", \"post_tags\".\"post_id\" AS \"moso_key\" FROM \"tags\" INNER JOIN \
             \"post_tags\" ON \"post_tags\".\"tag_id\" = \"tags\".\"id\" WHERE \"post_tags\".\"post_id\" IN \
             (?, ?, ?)",
            "many_to_many/sqlite"
        );
        assert_eq!(
            render(&comments().counting(), Backend::Postgres, 3),
            "SELECT \"comments\".\"post_id\" AS \"moso_key\", count(*) FROM \"comments\" WHERE \
             \"comments\".\"post_id\" = ANY ($1) GROUP BY \"comments\".\"post_id\"",
            "count/postgres"
        );
        assert_eq!(
            render(&comments().counting(), Backend::Sqlite, 3),
            "SELECT \"comments\".\"post_id\" AS \"moso_key\", count(*) FROM \"comments\" WHERE \
             \"comments\".\"post_id\" IN (?, ?, ?) GROUP BY \"comments\".\"post_id\"",
            "count/sqlite"
        );
        assert_eq!(
            render_using(&newest_three(), Backend::Postgres, LimitStrategy::Window),
            "SELECT * FROM (SELECT \"comments\".\"id\", \"comments\".\"post_id\", \
             \"comments\".\"approved\", \"comments\".\"created_at\", \"comments\".\"post_id\" AS \
             \"moso_key\", row_number() OVER (PARTITION BY \"comments\".\"post_id\" ORDER BY \
             \"comments\".\"created_at\" DESC, \"id\" ASC) AS \"moso_row\" FROM \"comments\" WHERE \
             \"comments\".\"post_id\" = ANY ($1) AND \"comments\".\"approved\" = $2) AS \"moso_children\" \
             WHERE \"moso_row\" <= $3 ORDER BY \"moso_key\" ASC, \"moso_row\" ASC",
            "window/postgres"
        );
        assert_eq!(
            render_using(&newest_three(), Backend::Sqlite, LimitStrategy::Window),
            "SELECT * FROM (SELECT \"comments\".\"id\", \"comments\".\"post_id\", \
             \"comments\".\"approved\", \"comments\".\"created_at\", \"comments\".\"post_id\" AS \
             \"moso_key\", row_number() OVER (PARTITION BY \"comments\".\"post_id\" ORDER BY \
             \"comments\".\"created_at\" DESC, \"id\" ASC) AS \"moso_row\" FROM \"comments\" WHERE \
             \"comments\".\"post_id\" IN (?, ?, ?) AND \"comments\".\"approved\" = ?) AS \
             \"moso_children\" WHERE \"moso_row\" <= ? ORDER BY \"moso_key\" ASC, \"moso_row\" ASC",
            "window/sqlite"
        );
        assert_eq!(
            render_using(
                &newest_three(),
                Backend::Postgres,
                LimitStrategy::CorrelatedCount
            ),
            "WITH \"moso_children\" AS (SELECT \"comments\".\"id\", \"comments\".\"post_id\", \
             \"comments\".\"approved\", \"comments\".\"created_at\", \"comments\".\"post_id\" AS \"moso_key\" \
             FROM \"comments\" WHERE \"comments\".\"post_id\" = ANY ($1) AND \"comments\".\"approved\" = \
             $2) SELECT * FROM \"moso_children\" WHERE (SELECT count(*) FROM \"moso_children\" AS \
             \"moso_peer\" WHERE \"moso_peer\".\"moso_key\" = \"moso_children\".\"moso_key\" AND \
             (\"moso_peer\".\"created_at\" > \"moso_children\".\"created_at\" OR \
             \"moso_peer\".\"created_at\" = \"moso_children\".\"created_at\" AND \"moso_peer\".\"id\" < \
             \"moso_children\".\"id\")) < $3 ORDER BY \"moso_children\".\"moso_key\" ASC, \
             \"moso_children\".\"created_at\" DESC, \"moso_children\".\"id\" ASC",
            "correlated/postgres"
        );
        assert_eq!(
            render_using(
                &newest_three(),
                Backend::Sqlite,
                LimitStrategy::CorrelatedCount
            ),
            "WITH \"moso_children\" AS (SELECT \"comments\".\"id\", \"comments\".\"post_id\", \
             \"comments\".\"approved\", \"comments\".\"created_at\", \"comments\".\"post_id\" AS \"moso_key\" \
             FROM \"comments\" WHERE \"comments\".\"post_id\" IN (?, ?, ?) AND \"comments\".\"approved\" \
             = ?) SELECT * FROM \"moso_children\" WHERE (SELECT count(*) FROM \"moso_children\" AS \
             \"moso_peer\" WHERE \"moso_peer\".\"moso_key\" = \"moso_children\".\"moso_key\" AND \
             (\"moso_peer\".\"created_at\" > \"moso_children\".\"created_at\" OR \
             \"moso_peer\".\"created_at\" = \"moso_children\".\"created_at\" AND \"moso_peer\".\"id\" < \
             \"moso_children\".\"id\")) < ? ORDER BY \"moso_children\".\"moso_key\" ASC, \
             \"moso_children\".\"created_at\" DESC, \"moso_children\".\"id\" ASC",
            "correlated/sqlite"
        );
        assert_eq!(
            render(&approved(), Backend::Postgres, 3),
            "SELECT \"comments\".\"id\", \"comments\".\"post_id\", \"comments\".\"approved\", \
             \"comments\".\"created_at\", \"comments\".\"post_id\" AS \"moso_key\" FROM \"comments\" WHERE \
             \"comments\".\"post_id\" = ANY ($1) AND \"comments\".\"approved\" = $2",
            "filtered/postgres"
        );
    }

    #[test]
    fn a_fingerprint_names_the_table_and_binds_nothing() {
        let query = SqlSelect::from_table(TableRef::from_static("comments"))
            .select_all()
            .filter(Expr::col(Ident::from_static("post_id")).eq(Expr::value(42_i64)));
        let print = fingerprint(&query.into_statement());
        assert_eq!(print, "SELECT FROM comments");
        assert!(!print.contains("42"), "a fingerprint never carries a value");
    }
}

/// The WP-12 acceptance criteria, against a real database.
///
/// The fixture is the one `docs/02-data/22-relations.md` names: **100 parents ×
/// 10 children**. Every assertion here is a statement count taken from the
/// live [`StatementCounter`](crate::StatementCounter), not from inspecting a
/// plan — the plan tests above prove the shape, and these prove that the shape
/// is what actually runs.
///
/// PostgreSQL runs when `DATABASE_URL` is set and is skipped with a message
/// otherwise, so the suite passes on a machine without Docker. SQLite runs
/// always, in memory.
#[cfg(test)]
mod real_database {
    use super::*;
    use crate::descriptor::EntityDescriptor;
    use crate::relation::{
        BelongsTo, HasMany, ManyToMany, Related, Relation, load_many, run_preloads,
    };
    use crate::row::DecodeError;
    use crate::{ColumnDef, Db, Executor};
    use moso_sql::{Insert, RawStatement};
    use std::sync::OnceLock;

    /// How many parents the acceptance fixture has.
    const PARENTS: i64 = 100;
    /// How many children each parent has.
    const CHILDREN: i64 = 10;

    /// A post: the parent of the fixture.
    #[derive(Clone, Debug)]
    struct Post {
        id: i64,
        author_id: i64,
        author: Related<Author>,
        comments: Related<Vec<Comment>>,
        tags: Related<Vec<Tag>>,
        comment_count: Option<i64>,
    }

    /// An author: one per ten posts, so deduplication is visible.
    #[derive(Clone, Debug)]
    struct Author {
        id: i64,
        posts: Related<Vec<Post>>,
    }

    /// A comment: ten per post.
    #[derive(Clone, Debug)]
    struct Comment {
        id: i64,
        post_id: i64,
        seq: i64,
    }

    /// A tag, reached through a join table.
    #[derive(Clone, Debug)]
    struct Tag {
        id: i64,
    }

    impl Entity for Post {
        type Pk = i64;
        const TABLE: TableRef = TableRef::from_static("rt_posts");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("author_id", ValueKind::I64),
        ];
        const NAME: &'static str = "Post";
        fn pk(&self) -> i64 {
            self.id
        }
        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
                author_id: row.get_i64(1)?,
                author: Related::NotLoaded,
                comments: Related::NotLoaded,
                tags: Related::NotLoaded,
                comment_count: None,
            })
        }
        fn descriptor() -> &'static EntityDescriptor {
            static D: OnceLock<EntityDescriptor> = OnceLock::new();
            D.get_or_init(|| EntityDescriptor::builder("Post", Self::TABLE).build())
        }
    }

    impl Entity for Author {
        type Pk = i64;
        const TABLE: TableRef = TableRef::from_static("rt_authors");
        const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
        const NAME: &'static str = "Author";
        fn pk(&self) -> i64 {
            self.id
        }
        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
                posts: Related::NotLoaded,
            })
        }
        fn descriptor() -> &'static EntityDescriptor {
            static D: OnceLock<EntityDescriptor> = OnceLock::new();
            D.get_or_init(|| EntityDescriptor::builder("Author", Self::TABLE).build())
        }
    }

    impl Entity for Comment {
        type Pk = i64;
        const TABLE: TableRef = TableRef::from_static("rt_comments");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("post_id", ValueKind::I64),
            ColumnDef::new("seq", ValueKind::I64),
        ];
        const NAME: &'static str = "Comment";
        fn pk(&self) -> i64 {
            self.id
        }
        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
                post_id: row.get_i64(1)?,
                seq: row.get_i64(2)?,
            })
        }
        fn descriptor() -> &'static EntityDescriptor {
            static D: OnceLock<EntityDescriptor> = OnceLock::new();
            D.get_or_init(|| EntityDescriptor::builder("Comment", Self::TABLE).build())
        }
    }

    impl Entity for Tag {
        type Pk = i64;
        const TABLE: TableRef = TableRef::from_static("rt_tags");
        const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
        const NAME: &'static str = "Tag";
        fn pk(&self) -> i64 {
            self.id
        }
        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }
        fn descriptor() -> &'static EntityDescriptor {
            static D: OnceLock<EntityDescriptor> = OnceLock::new();
            D.get_or_init(|| EntityDescriptor::builder("Tag", Self::TABLE).build())
        }
    }

    const AUTHOR: BelongsTo<Post, Author> = BelongsTo::new("author", "author_id")
        .keyed_by(|post: &Post| Value::I64(post.author_id))
        .linking(|post, rows| {
            post.author = Related::Loaded(rows.into_required_row::<Author>()?);
            Ok(())
        });

    const COMMENTS: HasMany<Post, Comment> =
        HasMany::new("comments", "post_id").linking(|post, rows| {
            post.comments = Related::Loaded(rows.into_rows::<Comment>()?);
            Ok(())
        });

    const TAGS: ManyToMany<Post, Tag> =
        ManyToMany::new("tags", "rt_post_tags", "post_id", "tag_id").linking(|post, rows| {
            post.tags = Related::Loaded(rows.into_rows::<Tag>()?);
            Ok(())
        });

    const AUTHOR_POSTS: HasMany<Author, Post> =
        HasMany::new("posts", "author_id").linking(|author, rows| {
            author.posts = Related::Loaded(rows.into_rows::<Post>()?);
            Ok(())
        });

    /// The `.with_count(Post::COMMENTS)` node, whose payload is a number.
    fn comment_count() -> Preload {
        Relation::<Post>::count_preload(&COMMENTS).linking(|post: &mut Post, rows| {
            post.comment_count = Some(rows.into_count()?);
            Ok(())
        })
    }

    /// Drops and recreates the fixture, then fills it.
    async fn fixture(db: &Db) -> Result<()> {
        let handle = Executor::handle(db);
        let serial = if db.backend() == Backend::Postgres {
            "bigint"
        } else {
            "integer"
        };
        for ddl in [
            "DROP TABLE IF EXISTS rt_post_tags".to_owned(),
            "DROP TABLE IF EXISTS rt_comments".to_owned(),
            "DROP TABLE IF EXISTS rt_tags".to_owned(),
            "DROP TABLE IF EXISTS rt_posts".to_owned(),
            "DROP TABLE IF EXISTS rt_authors".to_owned(),
            "CREATE TABLE rt_authors (id bigint PRIMARY KEY)".to_owned(),
            format!("CREATE TABLE rt_posts (id {serial} PRIMARY KEY, author_id bigint NOT NULL)"),
            format!(
                "CREATE TABLE rt_comments (id {serial} PRIMARY KEY, post_id bigint NOT NULL, \
                 seq bigint NOT NULL)"
            ),
            "CREATE TABLE rt_tags (id bigint PRIMARY KEY)".to_owned(),
            "CREATE TABLE rt_post_tags (post_id bigint NOT NULL, tag_id bigint NOT NULL, \
             PRIMARY KEY (post_id, tag_id))"
                .to_owned(),
        ] {
            handle
                .execute(&RawStatement::new(ddl).into_statement())
                .await?;
        }

        // Ten authors, a hundred posts, a thousand comments, three tags.
        let authors = (1..=10).map(|id| vec![Expr::value(id)]);
        handle
            .execute(
                &Insert::into_table(Author::TABLE)
                    .columns([Ident::from_static("id")])
                    .rows(authors)
                    .into_statement(),
            )
            .await?;

        let posts = (1..=PARENTS).map(|id| vec![Expr::value(id), Expr::value((id % 10) + 1)]);
        handle
            .execute(
                &Insert::into_table(Post::TABLE)
                    .columns([Ident::from_static("id"), Ident::from_static("author_id")])
                    .rows(posts)
                    .into_statement(),
            )
            .await?;

        let comments = (1..=PARENTS).flat_map(|post| {
            (1..=CHILDREN).map(move |seq| {
                vec![
                    Expr::value((post - 1) * CHILDREN + seq),
                    Expr::value(post),
                    Expr::value(seq),
                ]
            })
        });
        handle
            .execute(
                &Insert::into_table(Comment::TABLE)
                    .columns([
                        Ident::from_static("id"),
                        Ident::from_static("post_id"),
                        Ident::from_static("seq"),
                    ])
                    .rows(comments)
                    .into_statement(),
            )
            .await?;

        handle
            .execute(
                &Insert::into_table(Tag::TABLE)
                    .columns([Ident::from_static("id")])
                    .rows((1..=3).map(|id| vec![Expr::value(id)]))
                    .into_statement(),
            )
            .await?;
        Ok(())
    }

    /// Every post, fetched the way `Select::fetch_all` will: one statement.
    async fn all_posts(db: &Db) -> Result<Vec<Post>> {
        let query = SqlSelect::from_table(Post::TABLE)
            .select_column(Post::TABLE.column(Ident::from_static("id")))
            .select_column(Post::TABLE.column(Ident::from_static("author_id")))
            .order_by(OrderTerm::asc(Expr::column(
                Post::TABLE.column(Ident::from_static("id")),
            )))
            .into_statement();
        let rows = Executor::handle(db).fetch_all(&query).await?;
        rows.iter()
            .map(Post::from_row)
            .map(|row| Ok(row?))
            .collect()
    }

    /// Runs every acceptance criterion against `db`.
    async fn acceptance(db: &Db) -> Result<()> {
        fixture(db).await?;

        // 1 — a preload of 100 parents × 10 children is two statements.
        let mark = db.statements().mark();
        let mut posts = all_posts(db).await?;
        assert_eq!(posts.len(), 100);
        run_preloads(
            &[Relation::<Post>::preload(&COMMENTS)],
            &mut posts,
            Executor::handle(db),
        )
        .await?;
        assert_eq!(
            db.statements().since(mark),
            2,
            "100 parents × 10 children must be the parent query plus one"
        );
        for post in &posts {
            let children = post.comments.get()?;
            assert_eq!(children.len(), 10, "post {}", post.id);
            assert!(
                children.iter().all(|comment| comment.post_id == post.id),
                "every child must land under its own parent, not merely be counted"
            );
        }

        // …and touching what was loaded, or what was not, issues nothing.
        let mark = db.statements().mark();
        for post in &posts {
            let _ = post.comments.get()?;
            let _ = post.author.get().is_err();
            let _ = post.tags.as_option();
        }
        assert_eq!(
            db.statements().since(mark),
            0,
            "reading a relation is never a query"
        );

        // 2 — nested two levels deep is three statements, and a `belongs_to`
        // shared by ten posts is still one.
        let mark = db.statements().mark();
        let mut posts = all_posts(db).await?;
        run_preloads(
            &[
                Relation::<Post>::preload(&COMMENTS),
                Relation::<Post>::preload(&AUTHOR),
            ],
            &mut posts,
            Executor::handle(db),
        )
        .await?;
        assert_eq!(db.statements().since(mark), 3, "1 + comments + authors");
        assert_eq!(posts[0].author.get()?.id, (posts[0].id % 10) + 1);

        // …and a genuinely nested tree — posts → author → that author's posts —
        // is also three.
        let mark = db.statements().mark();
        let mut posts = all_posts(db).await?;
        let nested =
            Relation::<Post>::preload(&AUTHOR).with(Relation::<Author>::preload(&AUTHOR_POSTS));
        assert_eq!(nested.statement_count(), 2);
        run_preloads(&[nested], &mut posts, Executor::handle(db)).await?;
        assert_eq!(db.statements().since(mark), 3, "1 + authors + their posts");
        assert_eq!(posts[0].author.get()?.posts.get()?.len(), 10);

        // 3 — `limit_per_parent(3)` returns three per parent, and both
        // strategies return the same rows in the same order.
        let node = Relation::<Post>::preload(&COMMENTS)
            .order_by(OrderTerm::desc(Expr::column(
                Comment::TABLE.column(Ident::from_static("seq")),
            )))
            .limit_per_parent(3);
        let mut windowed = all_posts(db).await?;
        let mark = db.statements().mark();
        run_preloads(
            core::slice::from_ref(&node),
            &mut windowed,
            Executor::handle(db),
        )
        .await?;
        assert_eq!(db.statements().since(mark), 1, "a window is still one");
        for post in &windowed {
            let newest: Vec<i64> = post.comments.get()?.iter().map(|c| c.seq).collect();
            assert_eq!(newest, [10, 9, 8], "the three newest, post {}", post.id);
        }

        let keys: Vec<Value> = windowed.iter().map(|post| Value::I64(post.id)).collect();
        let correlated =
            node.statement_using(&keys, db.backend(), LimitStrategy::CorrelatedCount)?;
        let windowed_rows = Executor::handle(db)
            .fetch_all(&node.statement_using(&keys, db.backend(), LimitStrategy::Window)?)
            .await?;
        let correlated_rows = Executor::handle(db).fetch_all(&correlated).await?;
        fn shape(rows: &[Row]) -> Result<Vec<(i64, i64)>> {
            rows.iter()
                .map(|row| Ok((row.get_i64(1)?, row.get_i64(2)?)))
                .collect()
        }
        assert_eq!(
            shape(&windowed_rows)?,
            shape(&correlated_rows)?,
            "the two per-parent-limit strategies must be indistinguishable"
        );

        // 4 — `with_count` adds a number without fetching a row.
        let mut posts = all_posts(db).await?;
        let mark = db.statements().mark();
        run_preloads(&[comment_count()], &mut posts, Executor::handle(db)).await?;
        assert_eq!(db.statements().since(mark), 1);
        assert_eq!(posts[0].comment_count, Some(10));

        // 5 — `load_many` is one statement for a batch already in hand.
        let mut posts = all_posts(db).await?;
        let mark = db.statements().mark();
        load_many(&mut posts, COMMENTS, db).await?;
        assert_eq!(db.statements().since(mark), 1);

        // 6 — attach / detach / sync are one statement each and idempotent.
        let attachment = TAGS.on(&posts[0]);
        for _ in 0..2 {
            let mark = db.statements().mark();
            attachment.attach([1_i64, 2], db).await?;
            assert_eq!(db.statements().since(mark), 1, "attach is one statement");
        }
        let mut one = vec![posts.remove(0)];
        run_preloads(
            &[Relation::<Post>::preload(&TAGS)],
            &mut one,
            Executor::handle(db),
        )
        .await?;
        assert_eq!(
            one[0].tags.get()?.len(),
            2,
            "attaching twice added two rows"
        );

        let mark = db.statements().mark();
        attachment.detach([1_i64], db).await?;
        assert_eq!(db.statements().since(mark), 1, "detach is one statement");

        let expected = if db.backend() == Backend::Postgres {
            1
        } else {
            2
        };
        for _ in 0..2 {
            let mark = db.statements().mark();
            attachment.sync([2_i64, 3], db).await?;
            assert_eq!(db.statements().since(mark), expected, "sync is idempotent");
        }
        run_preloads(
            &[Relation::<Post>::preload(&TAGS)],
            &mut one,
            Executor::handle(db),
        )
        .await?;
        let mut synced: Vec<i64> = one[0].tags.get()?.iter().map(Entity::pk).collect();
        synced.sort_unstable();
        assert_eq!(synced, [2, 3], "sync leaves exactly the named set");
        Ok(())
    }

    #[tokio::test]
    async fn the_acceptance_criteria_hold_on_sqlite() {
        // A file rather than `:memory:`, because every connection in a pool
        // gets its *own* in-memory database and the fixture would vanish
        // between statements.
        let path =
            std::env::temp_dir().join(format!("moso-relations-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::connect_url(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("a SQLite database");
        let outcome = acceptance(&db).await;
        let _ = std::fs::remove_file(&path);
        outcome.expect("every criterion");
    }

    #[tokio::test]
    async fn the_acceptance_criteria_hold_on_postgres() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skipped: set DATABASE_URL to run the relation acceptance suite against \
                 PostgreSQL (docker compose -f compose.test.yaml up -d)"
            );
            return;
        };
        let db = Db::connect_url(&url).await.expect("the test database");
        acceptance(&db).await.expect("every criterion");
    }
}
