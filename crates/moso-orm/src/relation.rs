//! Relations, and the types that make N+1 impossible to cause by accident.
//!
//! # `Related<T>` never queries
//!
//! Non-negotiable N2: reading an unloaded relation returns
//! [`NotLoaded`] — it does not go to the database. Implicit lazy loading is how
//! ORM-backed applications acquire an N+1 in production, and the only reliable
//! cure is for the accessor to be incapable of issuing a statement. See
//! [`mod@crate::relation::related`].
//!
//! # Preloads are batched
//!
//! Non-negotiable N3: `.with(Post::COMMENTS)` costs **one** extra statement for
//! any number of parents, because it collects the parents' keys, deduplicates
//! them, and issues a single `WHERE fk = ANY($1)`. A nested preload costs one
//! more. Never a per-row query, and never a join that multiplies the parent
//! rows. See [`mod@crate::relation::preload`].
//!
//! # The four shapes, and what each one costs
//!
//! | Kind | Field | Foreign key | Statement |
//! | --- | --- | --- | --- |
//! | [`BelongsTo`] | `Related<T>` | this table | `WHERE target.id = ANY(keys)` |
//! | [`HasMany`] | `Related<Vec<T>>` | other table | `WHERE target.fk = ANY(keys)` |
//! | [`HasOne`] | `Related<Option<T>>` | other table | `WHERE target.fk = ANY(keys)` |
//! | [`ManyToMany`] | `Related<Vec<T>>` | join table | one join-table query |
//!
//! Self-referential relations are the same thing with `T = E`, and cost no
//! more: the parents and the children are two separate statements, so nothing
//! has to be aliased. [`BelongsToAny`] — the polymorphic case — is the one
//! exception to "one node, one statement": it costs one per target type that
//! actually appears in the parent set.
//!
//! # Joins are a different operation
//!
//! `.with(..)` fetches related rows; `.join(..)` filters by them. Conflating
//! the two is the mistake Rails institutionalised, and it produces both the
//! row-multiplying `LIMIT` bug and the surprise N+1. Here they are different
//! methods with different return types and no overlap.

use core::fmt;
use core::future::Future;
use core::marker::PhantomData;

use moso_sql::{Delete, Expr, Ident, Insert, OnConflict, Statement, TableRef, Value};

use crate::db::Backend;
use crate::descriptor::RelationKind;
use crate::entity::Entity;
use crate::error::Result;
use crate::executor::Executor;
use crate::select::{JoinKind, Joined};
use crate::sqltype::SqlType;

#[path = "preload.rs"]
pub mod preload;
#[path = "related.rs"]
pub mod related;

pub use self::preload::{
    LimitStrategy, NPlusOne, NPlusOneReport, Preload, detect, fingerprint, observe, observe_sql,
    run_preloads,
};
pub use self::related::{ForeignKeyFn, LinkFn, LoadedRows, NotLoaded, PolymorphicKeyFn, Related};

/// A declared relation between two entities.
///
/// Implemented by [`BelongsTo`], [`HasMany`], [`HasOne`] and [`ManyToMany`],
/// which `#[derive(Entity)]` generates as associated constants.
///
/// ```
/// use moso_orm::{BelongsTo, Relation};
/// # use moso_orm::{ColumnDef, DecodeError, Entity, Row};
/// # use moso_orm::descriptor::EntityDescriptor;
/// # use moso_sql::{TableRef, ValueKind};
/// # use std::sync::OnceLock;
/// # macro_rules! entity { ($name:ident, $table:literal) => {
/// #   #[derive(Clone, Debug)] pub struct $name { pub id: i64 }
/// #   impl Entity for $name {
/// #     type Pk = i64;
/// #     const TABLE: TableRef = TableRef::from_static($table);
/// #     const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
/// #     const NAME: &'static str = stringify!($name);
/// #     fn pk(&self) -> i64 { self.id }
/// #     fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
/// #     fn descriptor() -> &'static EntityDescriptor {
/// #       static D: OnceLock<EntityDescriptor> = OnceLock::new();
/// #       D.get_or_init(|| EntityDescriptor::builder(stringify!($name), Self::TABLE).build())
/// #     }
/// #   }
/// # } }
/// # entity!(Post, "posts");
/// # entity!(User, "users");
/// const AUTHOR: BelongsTo<Post, User> = BelongsTo::new("author", "author_id");
/// assert_eq!(AUTHOR.name(), "author");
/// assert_eq!(AUTHOR.preload().relation(), "author");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a relation of `{E}`",
    label = "not a relation",
    note = "`.join(..)` and `.with(..)` take a relation constant that `#[derive(Entity)]` \
            generates — for a `has_many = Comment` field named `comments` that is \
            `{E}::COMMENTS`",
    note = "help: check the spelling, or add the relation: \
            `#[entity(has_many = Other, fk = \"…\")] pub others: Related<Vec<Other>>`"
)]
pub trait Relation<E: Entity>: Copy + Send + Sync + 'static {
    /// The entity on the other side.
    type Target: Entity;

    /// Which of the four shapes this is.
    const KIND: RelationKind;

    /// The relation's field name, for diagnostics and for the preload node.
    ///
    /// ```
    /// # use moso_orm::{Entity, Relation};
    /// fn named<E: Entity, R: Relation<E>>(relation: R) -> &'static str {
    ///     relation.name()
    /// }
    /// ```
    fn name(&self) -> &'static str;

    /// The join that brings the target into scope.
    ///
    /// ```
    /// # use moso_orm::{Entity, JoinKind, Joined, Relation};
    /// fn joined<E: Entity, R: Relation<E>>(relation: R) -> Joined {
    ///     relation.join(JoinKind::Inner)
    /// }
    /// ```
    fn join(&self, kind: JoinKind) -> Joined;

    /// A preload node for this relation, with no filters or nesting.
    ///
    /// ```
    /// # use moso_orm::{Entity, Preload, Relation};
    /// fn eager<E: Entity, R: Relation<E>>(relation: R) -> Preload {
    ///     relation.preload()
    /// }
    /// ```
    fn preload(&self) -> Preload;

    /// A preload node that fetches only a count.
    ///
    /// The node keeps the relation's own setter, so the setter must branch on
    /// [`LoadedRows::is_count`] — one `.with(..)` and one `.with_count(..)` of
    /// the same relation write two different fields. `#[derive(Entity)]`
    /// generates exactly that; a hand-written constant can instead override the
    /// setter with [`Preload::linking`].
    ///
    /// ```
    /// # use moso_orm::{Entity, Preload, Relation};
    /// fn counted<E: Entity, R: Relation<E>>(relation: R) -> Preload {
    ///     relation.count_preload()
    /// }
    /// ```
    fn count_preload(&self) -> Preload {
        self.preload().counting()
    }
}

macro_rules! relation_type {
    (
        $name:ident,
        kind = $kind:ident,
        doc = $doc:literal,
        example = $example:literal
        $(,)?
    ) => {
        #[doc = $doc]
        ///
        /// ```
        #[doc = $example]
        /// ```
        pub struct $name<E, T> {
            relation: &'static str,
            key: &'static str,
            link: Option<LinkFn<E>>,
            parent_key: Option<ForeignKeyFn<E>>,
            self_referential: bool,
            owner: PhantomData<fn() -> E>,
            target: PhantomData<fn() -> T>,
        }

        impl<E, T> $name<E, T> {
            /// The relation named `relation`, keyed on `key`.
            ///
            /// ```
            /// # use moso_orm::*;
            /// # struct A; struct B;
            #[doc = concat!("let relation = ", stringify!($name), "::<A, B>::new(\"r\", \"k\");")]
            /// assert_eq!(relation.key(), "k");
            /// ```
            #[must_use]
            pub const fn new(relation: &'static str, key: &'static str) -> Self {
                Self {
                    relation,
                    key,
                    link: None,
                    parent_key: None,
                    self_referential: false,
                    owner: PhantomData,
                    target: PhantomData,
                }
            }

            /// Supplies the setter that puts the loaded rows into the owner's
            /// field. `#[derive(Entity)]` generates one per relation.
            ///
            /// ```
            /// # use moso_orm::*;
            /// # use moso_orm::relation::{LinkFn, LoadedRows, Related};
            /// # struct B;
            /// struct A { children: Related<Vec<i64>> }
            /// const LINK: LinkFn<A> = |a, rows| {
            ///     a.children = Related::Loaded(rows.into_rows::<i64>()?);
            ///     Ok(())
            /// };
            #[doc = concat!("let relation = ", stringify!($name), "::<A, B>::new(\"r\", \"k\").linking(LINK);")]
            /// assert!(relation.link().is_some());
            /// ```
            #[must_use]
            pub const fn linking(mut self, link: LinkFn<E>) -> Self {
                self.link = Some(link);
                self
            }

            /// Supplies the reader for the owner's key column, when it is not
            /// the primary key — always the case for a `belongs_to`.
            ///
            /// ```
            /// # use moso_orm::*;
            /// # use moso_orm::relation::ForeignKeyFn;
            /// # use moso_sql::Value;
            /// # struct B;
            /// struct A { other_id: i64 }
            /// const KEY: ForeignKeyFn<A> = |a| Value::I64(a.other_id);
            #[doc = concat!("let relation = ", stringify!($name), "::<A, B>::new(\"r\", \"k\").keyed_by(KEY);")]
            /// assert!(relation.parent_key().is_some());
            /// ```
            #[must_use]
            pub const fn keyed_by(mut self, key: ForeignKeyFn<E>) -> Self {
                self.parent_key = Some(key);
                self
            }

            /// Marks the relation as pointing back at its own table, so the
            /// join it produces is aliased and therefore unambiguous.
            ///
            /// ```
            /// # use moso_orm::*;
            /// # struct A;
            #[doc = concat!("let relation = ", stringify!($name), "::<A, A>::new(\"parent\", \"parent_id\");")]
            /// assert!(relation.self_referential().is_self_referential());
            /// ```
            #[must_use]
            pub const fn self_referential(mut self) -> Self {
                self.self_referential = true;
                self
            }

            /// The relation's field name.
            ///
            /// ```
            /// # use moso_orm::*;
            /// # struct A; struct B;
            #[doc = concat!("assert_eq!(", stringify!($name), "::<A, B>::new(\"r\", \"k\").relation_name(), \"r\");")]
            /// ```
            #[must_use]
            pub const fn relation_name(&self) -> &'static str {
                self.relation
            }

            /// The foreign-key column.
            ///
            /// ```
            /// # use moso_orm::*;
            /// # struct A; struct B;
            #[doc = concat!("assert_eq!(", stringify!($name), "::<A, B>::new(\"r\", \"k\").key(), \"k\");")]
            /// ```
            #[must_use]
            pub const fn key(&self) -> &'static str {
                self.key
            }

            /// The foreign-key column as a validated identifier.
            ///
            /// # Panics
            ///
            /// If the key is not a valid SQL identifier, which the derive
            /// checks at compile time.
            ///
            /// ```
            /// # use moso_orm::*;
            /// # struct A; struct B;
            #[doc = concat!("assert_eq!(", stringify!($name), "::<A, B>::new(\"r\", \"k\").key_ident().as_str(), \"k\");")]
            /// ```
            #[must_use]
            pub const fn key_ident(&self) -> Ident {
                Ident::from_static(self.key)
            }

            /// The field setter, when the derive supplied one.
            ///
            /// ```
            /// # use moso_orm::*;
            /// # struct A; struct B;
            #[doc = concat!("assert!(", stringify!($name), "::<A, B>::new(\"r\", \"k\").link().is_none());")]
            /// ```
            #[must_use]
            pub const fn link(&self) -> Option<LinkFn<E>> {
                self.link
            }

            /// The owner-side key reader, when the derive supplied one.
            ///
            /// ```
            /// # use moso_orm::*;
            /// # struct A; struct B;
            #[doc = concat!("assert!(", stringify!($name), "::<A, B>::new(\"r\", \"k\").parent_key().is_none());")]
            /// ```
            #[must_use]
            pub const fn parent_key(&self) -> Option<ForeignKeyFn<E>> {
                self.parent_key
            }

            /// Whether the relation points back at its own table.
            ///
            /// ```
            /// # use moso_orm::*;
            /// # struct A; struct B;
            #[doc = concat!("assert!(!", stringify!($name), "::<A, B>::new(\"r\", \"k\").is_self_referential());")]
            /// ```
            #[must_use]
            pub const fn is_self_referential(&self) -> bool {
                self.self_referential
            }
        }

        impl<E, T> Clone for $name<E, T> {
            fn clone(&self) -> Self {
                *self
            }
        }

        impl<E, T> Copy for $name<E, T> {}

        impl<E, T> fmt::Debug for $name<E, T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.relation)
            }
        }

        impl<E: Entity, T: Entity> Relation<E> for $name<E, T> {
            type Target = T;

            const KIND: RelationKind = RelationKind::$kind;

            fn name(&self) -> &'static str {
                self.relation
            }

            fn join(&self, kind: JoinKind) -> Joined {
                let alias = self.alias();
                let joined = Joined::new(
                    kind,
                    T::NAME,
                    T::TABLE,
                    self.join_condition(alias.as_ref().unwrap_or(&T::TABLE)),
                );
                match alias {
                    Some(alias) => joined.aliased(alias.name().clone()),
                    None => joined,
                }
            }

            fn preload(&self) -> Preload {
                let mut node =
                    Preload::of::<T>(self.relation, RelationKind::$kind).keyed(self.key);
                if let Some(link) = self.link {
                    node = node.linking(link);
                }
                if let Some(parent_key) = self.parent_key {
                    node = node.keyed_by(parent_key);
                }
                node
            }
        }

        impl<E: Entity, T: Entity> $name<E, T> {
            /// The table the join refers to the target by: an alias when the
            /// relation is self-referential, and the table itself otherwise.
            ///
            /// `posts JOIN posts ON …` is ambiguous SQL; `posts JOIN posts AS
            /// posts_parent ON …` is not.
            fn alias(&self) -> Option<TableRef> {
                if !self.self_referential {
                    return None;
                }
                let alias = format!("{}_{}", T::TABLE.name().as_str(), self.relation);
                Ident::new(alias).ok().map(TableRef::new)
            }
        }

        impl<E: Entity, T: Entity> From<$name<E, T>> for Preload {
            fn from(relation: $name<E, T>) -> Self {
                Relation::<E>::preload(&relation)
            }
        }
    };
}

relation_type!(
    BelongsTo,
    kind = BelongsTo,
    doc = "A relation whose foreign key is on **this** table.\n\n\
           `Post::AUTHOR: BelongsTo<Post, User>` for `author_id` on `posts`. The derive also \
           generates the scalar column `Post::AUTHOR_ID`, so filtering on the key needs no join.",
    example = "/// # use moso_orm::BelongsTo;\n\
               /// # struct Post; struct User;\n\
               /// const AUTHOR: BelongsTo<Post, User> = BelongsTo::new(\"author\", \"author_id\");\n\
               /// assert_eq!(AUTHOR.key(), \"author_id\");",
);

relation_type!(
    HasMany,
    kind = HasMany,
    doc = "A relation whose foreign key is on the **other** table, with many rows on that side.\n\n\
           `Post::COMMENTS: HasMany<Post, Comment>` for `post_id` on `comments`.",
    example = "/// # use moso_orm::HasMany;\n\
               /// # struct Post; struct Comment;\n\
               /// const COMMENTS: HasMany<Post, Comment> = HasMany::new(\"comments\", \"post_id\");\n\
               /// assert_eq!(COMMENTS.key(), \"post_id\");",
);

relation_type!(
    HasOne,
    kind = HasOne,
    doc = "A relation whose foreign key is on the **other** table, with at most one row there.\n\n\
           `Post::STATS: HasOne<Post, PostStats>` for `post_id` on `post_stats`.",
    example = "/// # use moso_orm::HasOne;\n\
               /// # struct Post; struct PostStats;\n\
               /// const STATS: HasOne<Post, PostStats> = HasOne::new(\"stats\", \"post_id\");\n\
               /// assert_eq!(STATS.key(), \"post_id\");",
);

impl<E: Entity, T: Entity> BelongsTo<E, T> {
    /// `this.fk = other.pk`.
    fn join_condition(&self, target: &TableRef) -> Expr {
        Expr::column(E::TABLE.column(self.key_ident()))
            .eq(Expr::column(target.column(primary_key_of::<T>())))
    }
}

impl<E: Entity, T: Entity> HasMany<E, T> {
    /// `other.fk = this.pk`.
    fn join_condition(&self, target: &TableRef) -> Expr {
        Expr::column(target.column(self.key_ident()))
            .eq(Expr::column(E::TABLE.column(primary_key_of::<E>())))
    }
}

impl<E: Entity, T: Entity> HasOne<E, T> {
    /// `other.fk = this.pk`.
    fn join_condition(&self, target: &TableRef) -> Expr {
        Expr::column(target.column(self.key_ident()))
            .eq(Expr::column(E::TABLE.column(primary_key_of::<E>())))
    }
}

/// The first primary-key column of `E`.
///
/// An entity with no primary key cannot exist — the derive refuses one — so the
/// fallback is `id`, which keeps this infallible without hiding a bug.
fn primary_key_of<E: Entity>() -> Ident {
    E::COLUMNS
        .iter()
        .find(|column| column.is_primary_key())
        .map_or_else(|| Ident::from_static("id"), crate::ColumnDef::ident)
}

/// A relation through a join table.
///
/// `Post::TAGS: ManyToMany<Post, Tag>` through `post_tags(post_id, tag_id)`.
///
/// ```
/// use moso_orm::ManyToMany;
/// # struct Post; struct Tag;
///
/// const TAGS: ManyToMany<Post, Tag> = ManyToMany::new("tags", "post_tags", "post_id", "tag_id");
/// assert_eq!(TAGS.join_table(), "post_tags");
/// assert_eq!(TAGS.left(), "post_id");
/// assert_eq!(TAGS.right(), "tag_id");
/// ```
pub struct ManyToMany<E, T> {
    relation: &'static str,
    join_table: &'static str,
    left: &'static str,
    right: &'static str,
    link: Option<LinkFn<E>>,
    owner: PhantomData<fn() -> E>,
    target: PhantomData<fn() -> T>,
}

impl<E, T> ManyToMany<E, T> {
    /// The relation through `join_table`, keyed on `left` and `right`.
    ///
    /// ```
    /// # use moso_orm::ManyToMany;
    /// # struct A; struct B;
    /// let m = ManyToMany::<A, B>::new("r", "j", "l", "r2");
    /// assert_eq!(m.join_table(), "j");
    /// ```
    #[must_use]
    pub const fn new(
        relation: &'static str,
        join_table: &'static str,
        left: &'static str,
        right: &'static str,
    ) -> Self {
        Self {
            relation,
            join_table,
            left,
            right,
            link: None,
            owner: PhantomData,
            target: PhantomData,
        }
    }

    /// Supplies the setter that puts the loaded rows into the owner's field.
    ///
    /// ```
    /// # use moso_orm::ManyToMany;
    /// # use moso_orm::relation::{LinkFn, LoadedRows, Related};
    /// # struct B;
    /// struct A { tags: Related<Vec<i64>> }
    /// const LINK: LinkFn<A> = |a, rows| {
    ///     a.tags = Related::Loaded(rows.into_rows::<i64>()?);
    ///     Ok(())
    /// };
    ///
    /// assert!(ManyToMany::<A, B>::new("tags", "j", "l", "r").linking(LINK).link().is_some());
    /// ```
    #[must_use]
    pub const fn linking(mut self, link: LinkFn<E>) -> Self {
        self.link = Some(link);
        self
    }

    /// The field setter, when the derive supplied one.
    ///
    /// ```
    /// # use moso_orm::ManyToMany;
    /// # struct A; struct B;
    /// assert!(ManyToMany::<A, B>::new("r", "j", "l", "r2").link().is_none());
    /// ```
    #[must_use]
    pub const fn link(&self) -> Option<LinkFn<E>> {
        self.link
    }

    /// The relation's field name.
    ///
    /// ```
    /// # use moso_orm::ManyToMany;
    /// # struct A; struct B;
    /// assert_eq!(ManyToMany::<A, B>::new("r", "j", "l", "r2").relation_name(), "r");
    /// ```
    #[must_use]
    pub const fn relation_name(&self) -> &'static str {
        self.relation
    }

    /// The join table's name.
    ///
    /// ```
    /// # use moso_orm::ManyToMany;
    /// # struct A; struct B;
    /// assert_eq!(ManyToMany::<A, B>::new("r", "j", "l", "r2").join_table(), "j");
    /// ```
    #[must_use]
    pub const fn join_table(&self) -> &'static str {
        self.join_table
    }

    /// The join-table column pointing at this entity.
    ///
    /// ```
    /// # use moso_orm::ManyToMany;
    /// # struct A; struct B;
    /// assert_eq!(ManyToMany::<A, B>::new("r", "j", "l", "r2").left(), "l");
    /// ```
    #[must_use]
    pub const fn left(&self) -> &'static str {
        self.left
    }

    /// The join-table column pointing at the target.
    ///
    /// ```
    /// # use moso_orm::ManyToMany;
    /// # struct A; struct B;
    /// assert_eq!(ManyToMany::<A, B>::new("r", "j", "l", "r2").right(), "r2");
    /// ```
    #[must_use]
    pub const fn right(&self) -> &'static str {
        self.right
    }

    /// The join table as a validated table reference.
    ///
    /// # Panics
    ///
    /// If the name is not a valid SQL identifier, which the derive checks at
    /// compile time.
    ///
    /// ```
    /// # use moso_orm::ManyToMany;
    /// # struct A; struct B;
    /// let m = ManyToMany::<A, B>::new("r", "post_tags", "l", "r2");
    /// assert_eq!(m.join_table_ref().name().as_str(), "post_tags");
    /// ```
    #[must_use]
    pub const fn join_table_ref(&self) -> TableRef {
        TableRef::from_static(self.join_table)
    }
}

impl<E, T> Clone for ManyToMany<E, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E, T> Copy for ManyToMany<E, T> {}

impl<E, T> fmt::Debug for ManyToMany<E, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ManyToMany({})", self.relation)
    }
}

impl<E: Entity, T: Entity> Relation<E> for ManyToMany<E, T> {
    type Target = T;

    const KIND: RelationKind = RelationKind::ManyToMany;

    fn name(&self) -> &'static str {
        self.relation
    }

    fn join(&self, kind: JoinKind) -> Joined {
        // The join table is folded into the condition rather than becoming a
        // second `Joined`, so that the scope stays one entity per join and the
        // out-of-scope message never mentions a table the user did not name.
        let bridge = self.join_table_ref();
        let condition = Expr::column(bridge.column(Ident::from_static(self.left)))
            .eq(Expr::column(E::TABLE.column(primary_key_of::<E>())))
            .and(
                Expr::column(bridge.column(Ident::from_static(self.right)))
                    .eq(Expr::column(T::TABLE.column(primary_key_of::<T>()))),
            );
        Joined::new(kind, T::NAME, T::TABLE, condition)
    }

    fn preload(&self) -> Preload {
        let node = Preload::of::<T>(self.relation, RelationKind::ManyToMany).through(
            self.join_table,
            self.left,
            self.right,
        );
        match self.link {
            Some(link) => node.linking(link),
            None => node,
        }
    }
}

impl<E: Entity, T: Entity> From<ManyToMany<E, T>> for Preload {
    fn from(relation: ManyToMany<E, T>) -> Self {
        Relation::<E>::preload(&relation)
    }
}

impl<E: Entity, T: Entity> ManyToMany<E, T> {
    /// The write side of the relation for one owner row.
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, ManyToMany, Result, Tx};
    /// async fn tag<E: Entity, T: Entity>(
    ///     relation: ManyToMany<E, T>,
    ///     owner: &E,
    ///     tags: Vec<T::Pk>,
    ///     tx: &Tx,
    /// ) -> Result<u64> {
    ///     relation.on(owner).attach(tags, tx).await
    /// }
    /// ```
    #[must_use]
    pub fn on(&self, owner: &E) -> Attachment<E, T> {
        Attachment {
            relation: *self,
            owner: owner.pk().into_value(),
        }
    }
}

/// The write side of a many-to-many, for one owner row.
///
/// Rows in the join table are written explicitly, one statement at a time.
/// There is no cascading save of an object graph: implicit graph persistence is
/// where ActiveRecord-shaped ORMs become unpredictable, and it is not on offer.
///
/// ```no_run
/// # use moso_orm::{Entity, ManyToMany, Result, Tx};
/// async fn retag<E: Entity, T: Entity>(
///     relation: ManyToMany<E, T>,
///     post: &E,
///     tags: Vec<T::Pk>,
///     tx: &Tx,
/// ) -> Result<u64> {
///     // Exactly this set, whatever was there before.
///     relation.on(post).sync(tags, tx).await
/// }
/// ```
pub struct Attachment<E, T> {
    relation: ManyToMany<E, T>,
    owner: Value,
}

impl<E: Entity, T: Entity> Attachment<E, T> {
    /// The owner's key, as the join table stores it.
    ///
    /// ```no_run
    /// # use moso_orm::relation::Attachment;
    /// # use moso_orm::Entity;
    /// # use moso_sql::Value;
    /// fn key<E: Entity, T: Entity>(attachment: &Attachment<E, T>) -> &Value {
    ///     attachment.owner_key()
    /// }
    /// ```
    #[must_use]
    pub const fn owner_key(&self) -> &Value {
        &self.owner
    }

    /// The relation being written.
    ///
    /// ```no_run
    /// # use moso_orm::relation::Attachment;
    /// # use moso_orm::{Entity, ManyToMany};
    /// fn relation<E: Entity, T: Entity>(a: &Attachment<E, T>) -> ManyToMany<E, T> {
    ///     a.relation()
    /// }
    /// ```
    #[must_use]
    pub const fn relation(&self) -> ManyToMany<E, T> {
        self.relation
    }

    /// The statement that adds `targets`, or `None` when there are none.
    ///
    /// `ON CONFLICT DO NOTHING`, so attaching twice is one statement and no
    /// error — the idempotence acceptance criterion, expressed in SQL rather
    /// than in a read-modify-write.
    ///
    /// ```no_run
    /// # use moso_orm::relation::Attachment;
    /// # use moso_orm::Entity;
    /// # use moso_sql::Statement;
    /// fn plan<E: Entity, T: Entity>(a: &Attachment<E, T>, ids: &[T::Pk]) -> Option<Statement> {
    ///     a.attach_statement(ids)
    /// }
    /// ```
    #[must_use]
    pub fn attach_statement(&self, targets: &[T::Pk]) -> Option<Statement> {
        if targets.is_empty() {
            return None;
        }
        let left = Ident::from_static(self.relation.left());
        let right = Ident::from_static(self.relation.right());
        let rows = targets.iter().map(|target| {
            vec![
                Expr::bound(self.owner.clone()),
                Expr::bound(target.to_value()),
            ]
        });
        Some(
            Insert::into_table(self.relation.join_table_ref())
                .columns([left.clone(), right.clone()])
                .rows(rows)
                .on_conflict(OnConflict::columns([left, right]).do_nothing())
                .into_statement(),
        )
    }

    /// The statement that removes `targets`, or `None` when there are none.
    ///
    /// ```no_run
    /// # use moso_orm::relation::Attachment;
    /// # use moso_orm::{Backend, Entity};
    /// # use moso_sql::Statement;
    /// fn plan<E: Entity, T: Entity>(a: &Attachment<E, T>, ids: &[T::Pk]) -> Option<Statement> {
    ///     a.detach_statement(ids, Backend::Postgres)
    /// }
    /// ```
    #[must_use]
    pub fn detach_statement(&self, targets: &[T::Pk], backend: Backend) -> Option<Statement> {
        if targets.is_empty() {
            return None;
        }
        let keys: Vec<Value> = targets.iter().map(SqlType::to_value).collect();
        Some(
            self.scoped_delete()
                .filter(preload::key_match(
                    self.right_column(),
                    &keys,
                    &backend.dialect().capabilities(),
                ))
                .into_statement(),
        )
    }

    /// The statement that removes every row for this owner.
    ///
    /// ```no_run
    /// # use moso_orm::relation::Attachment;
    /// # use moso_orm::Entity;
    /// # use moso_sql::Statement;
    /// fn plan<E: Entity, T: Entity>(a: &Attachment<E, T>) -> Statement {
    ///     a.clear_statement()
    /// }
    /// ```
    #[must_use]
    pub fn clear_statement(&self) -> Statement {
        self.scoped_delete().into_statement()
    }

    /// The statements that make the set exactly `targets`.
    ///
    /// **One** statement where the dialect has data-modifying CTEs — the delete
    /// rides along inside the insert's `WITH` — and two where it does not. The
    /// count is part of the contract, so it is returned rather than described:
    /// a test asserts `len()`.
    ///
    /// ```no_run
    /// # use moso_orm::relation::Attachment;
    /// # use moso_orm::{Backend, Entity};
    /// # use moso_sql::Statement;
    /// fn plan<E: Entity, T: Entity>(a: &Attachment<E, T>, ids: &[T::Pk]) -> Vec<Statement> {
    ///     a.sync_statements(ids, Backend::Postgres)
    /// }
    /// ```
    #[must_use]
    pub fn sync_statements(&self, targets: &[T::Pk], backend: Backend) -> Vec<Statement> {
        let capabilities = backend.dialect().capabilities();
        let keys: Vec<Value> = targets.iter().map(SqlType::to_value).collect();
        if keys.is_empty() {
            return vec![self.clear_statement()];
        }
        let surplus = self.scoped_delete().filter(preload::key_mismatch(
            self.right_column(),
            &keys,
            &capabilities,
        ));
        let Some(Statement::Insert(insert)) = self.attach_statement(targets) else {
            // `targets` is non-empty, so `attach_statement` returned an insert;
            // the fallback keeps the impossible branch honest rather than
            // panicking.
            return vec![surplus.into_statement()];
        };
        if capabilities.data_modifying_ctes {
            return vec![
                insert
                    .with(moso_sql::Cte::from_statement(
                        Ident::from_static("moso_detached"),
                        surplus.into_statement(),
                    ))
                    .into_statement(),
            ];
        }
        vec![surplus.into_statement(), insert.into_statement()]
    }

    /// Adds `targets` to the set. Idempotent, one statement.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`](crate::Error).
    ///
    /// ```no_run
    /// # use moso_orm::relation::Attachment;
    /// # use moso_orm::{Entity, Result, Tx};
    /// async fn add<E: Entity, T: Entity>(a: &Attachment<E, T>, ids: Vec<T::Pk>, tx: &Tx)
    /// -> Result<u64> {
    ///     a.attach(ids, tx).await
    /// }
    /// ```
    pub async fn attach(
        &self,
        targets: impl IntoIterator<Item = T::Pk>,
        executor: impl Executor<'_>,
    ) -> Result<u64> {
        let targets: Vec<T::Pk> = targets.into_iter().collect();
        let Some(statement) = self.attach_statement(&targets) else {
            return Ok(0);
        };
        executor.handle().execute(&statement).await
    }

    /// Removes `targets` from the set. Idempotent, one statement.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`](crate::Error).
    ///
    /// ```no_run
    /// # use moso_orm::relation::Attachment;
    /// # use moso_orm::{Entity, Result, Tx};
    /// async fn remove<E: Entity, T: Entity>(a: &Attachment<E, T>, ids: Vec<T::Pk>, tx: &Tx)
    /// -> Result<u64> {
    ///     a.detach(ids, tx).await
    /// }
    /// ```
    pub async fn detach(
        &self,
        targets: impl IntoIterator<Item = T::Pk>,
        executor: impl Executor<'_>,
    ) -> Result<u64> {
        let targets: Vec<T::Pk> = targets.into_iter().collect();
        let handle = executor.handle();
        let Some(statement) = self.detach_statement(&targets, handle.backend()) else {
            return Ok(0);
        };
        handle.execute(&statement).await
    }

    /// Makes the set exactly `targets`. Idempotent.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`](crate::Error). Run it inside a transaction on a backend that
    /// needs two statements, so that a failure between them cannot leave the
    /// set empty.
    ///
    /// ```no_run
    /// # use moso_orm::relation::Attachment;
    /// # use moso_orm::{Entity, Result, Tx};
    /// async fn set<E: Entity, T: Entity>(a: &Attachment<E, T>, ids: Vec<T::Pk>, tx: &Tx)
    /// -> Result<u64> {
    ///     a.sync(ids, tx).await
    /// }
    /// ```
    pub async fn sync(
        &self,
        targets: impl IntoIterator<Item = T::Pk>,
        executor: impl Executor<'_>,
    ) -> Result<u64> {
        let targets: Vec<T::Pk> = targets.into_iter().collect();
        let handle = executor.handle();
        let mut affected = 0;
        for statement in self.sync_statements(&targets, handle.backend()) {
            affected += handle.execute(&statement).await?;
        }
        Ok(affected)
    }

    /// Removes every row for this owner. One statement.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`](crate::Error).
    ///
    /// ```no_run
    /// # use moso_orm::relation::Attachment;
    /// # use moso_orm::{Entity, Result, Tx};
    /// async fn clear<E: Entity, T: Entity>(a: &Attachment<E, T>, tx: &Tx) -> Result<u64> {
    ///     a.clear(tx).await
    /// }
    /// ```
    pub async fn clear(&self, executor: impl Executor<'_>) -> Result<u64> {
        executor.handle().execute(&self.clear_statement()).await
    }

    /// `DELETE FROM join_table WHERE left = <owner>`, the start of every write.
    fn scoped_delete(&self) -> Delete {
        let table = self.relation.join_table_ref();
        Delete::from_table(table.clone()).filter(
            Expr::column(table.column(Ident::from_static(self.relation.left())))
                .eq(Expr::bound(self.owner.clone())),
        )
    }

    /// The join table's column pointing at the target.
    fn right_column(&self) -> Expr {
        let table = self.relation.join_table_ref();
        Expr::column(table.column(Ident::from_static(self.relation.right())))
    }
}

impl<E: Entity, T: Entity> fmt::Debug for Attachment<E, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attachment")
            .field("relation", &self.relation)
            .field("owner", &self.owner)
            .finish()
    }
}

/// A `belongs_to` whose target can be one of several entities.
///
/// `#[entity(belongs_to_any(types(Post, Comment), type_column = "target_type",
/// id_column = "target_id"))]` on a `Reaction` produces one of these, plus the
/// enum `ReactionTargetRef` that the loaded row goes into.
///
/// # What it costs
///
/// One statement **per target type present in the parent set** — not per
/// declared type, and never per row. Ten thousand reactions to posts and
/// comments load in two statements; ten thousand reactions to posts alone load
/// in one. A union would be one statement, and would require the two tables to
/// have the same shape, which is the whole reason the relation is polymorphic.
///
/// ```
/// use moso_orm::relation::{BelongsToAny, PolymorphicVariant};
/// # use moso_orm::{ColumnDef, DecodeError, Entity, Row};
/// # use moso_orm::descriptor::EntityDescriptor;
/// # use moso_orm::relation::{LoadedRows, Related};
/// # use moso_sql::{TableRef, ValueKind};
/// # use std::sync::OnceLock;
/// # #[derive(Clone, Debug)] pub struct Post { pub id: i64 }
/// # impl Entity for Post {
/// #   type Pk = i64;
/// #   const TABLE: TableRef = TableRef::from_static("posts");
/// #   const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
/// #   const NAME: &'static str = "Post";
/// #   fn pk(&self) -> i64 { self.id }
/// #   fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
/// #   fn descriptor() -> &'static EntityDescriptor {
/// #     static D: OnceLock<EntityDescriptor> = OnceLock::new();
/// #     D.get_or_init(|| EntityDescriptor::builder("Post", Self::TABLE).build())
/// #   }
/// # }
/// # #[derive(Clone, Debug)] pub struct Reaction { pub id: i64, pub target: Related<Post> }
/// # impl Entity for Reaction {
/// #   type Pk = i64;
/// #   const TABLE: TableRef = TableRef::from_static("reactions");
/// #   const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
/// #   const NAME: &'static str = "Reaction";
/// #   fn pk(&self) -> i64 { self.id }
/// #   fn from_row(row: &Row) -> Result<Self, DecodeError> {
/// #     Ok(Self { id: row.get_i64(0)?, target: Related::NotLoaded })
/// #   }
/// #   fn descriptor() -> &'static EntityDescriptor {
/// #     static D: OnceLock<EntityDescriptor> = OnceLock::new();
/// #     D.get_or_init(|| EntityDescriptor::builder("Reaction", Self::TABLE).build())
/// #   }
/// # }
/// static VARIANTS: &[PolymorphicVariant<Reaction>] = &[PolymorphicVariant::to::<Post>(
///     "post",
///     |reaction, rows| {
///         reaction.target = Related::Loaded(rows.into_required_row::<Post>()?);
///         Ok(())
///     },
/// )];
///
/// const TARGET: BelongsToAny<Reaction> =
///     BelongsToAny::new("target", "target_type", "target_id", VARIANTS);
///
/// assert_eq!(TARGET.statement_count(), 1);
/// assert_eq!(TARGET.type_column(), "target_type");
/// ```
pub struct BelongsToAny<E: 'static> {
    relation: &'static str,
    type_column: &'static str,
    id_column: &'static str,
    variants: &'static [PolymorphicVariant<E>],
}

impl<E: 'static> BelongsToAny<E> {
    /// The relation named `relation`, discriminated by `type_column`.
    ///
    /// ```
    /// # use moso_orm::relation::{BelongsToAny, PolymorphicVariant};
    /// # struct Reaction;
    /// static NONE: &[PolymorphicVariant<Reaction>] = &[];
    /// const TARGET: BelongsToAny<Reaction> =
    ///     BelongsToAny::new("target", "target_type", "target_id", NONE);
    /// assert_eq!(TARGET.name(), "target");
    /// ```
    #[must_use]
    pub const fn new(
        relation: &'static str,
        type_column: &'static str,
        id_column: &'static str,
        variants: &'static [PolymorphicVariant<E>],
    ) -> Self {
        Self {
            relation,
            type_column,
            id_column,
            variants,
        }
    }

    /// The relation's field name.
    ///
    /// ```
    /// # use moso_orm::relation::{BelongsToAny, PolymorphicVariant};
    /// # struct R;
    /// # static NONE: &[PolymorphicVariant<R>] = &[];
    /// assert_eq!(BelongsToAny::new("t", "tt", "ti", NONE).name(), "t");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.relation
    }

    /// The discriminator column.
    ///
    /// ```
    /// # use moso_orm::relation::{BelongsToAny, PolymorphicVariant};
    /// # struct R;
    /// # static NONE: &[PolymorphicVariant<R>] = &[];
    /// assert_eq!(BelongsToAny::new("t", "tt", "ti", NONE).type_column(), "tt");
    /// ```
    #[must_use]
    pub const fn type_column(&self) -> &'static str {
        self.type_column
    }

    /// The key column.
    ///
    /// ```
    /// # use moso_orm::relation::{BelongsToAny, PolymorphicVariant};
    /// # struct R;
    /// # static NONE: &[PolymorphicVariant<R>] = &[];
    /// assert_eq!(BelongsToAny::new("t", "tt", "ti", NONE).id_column(), "ti");
    /// ```
    #[must_use]
    pub const fn id_column(&self) -> &'static str {
        self.id_column
    }

    /// The declared targets.
    ///
    /// ```
    /// # use moso_orm::relation::{BelongsToAny, PolymorphicVariant};
    /// # struct R;
    /// # static NONE: &[PolymorphicVariant<R>] = &[];
    /// assert!(BelongsToAny::new("t", "tt", "ti", NONE).variants().is_empty());
    /// ```
    #[must_use]
    pub const fn variants(&self) -> &'static [PolymorphicVariant<E>] {
        self.variants
    }

    /// The worst case: one statement per declared target type.
    ///
    /// The actual cost is one per type *present*, which is never more.
    ///
    /// ```
    /// # use moso_orm::relation::{BelongsToAny, PolymorphicVariant};
    /// # struct R;
    /// # static NONE: &[PolymorphicVariant<R>] = &[];
    /// assert_eq!(BelongsToAny::new("t", "tt", "ti", NONE).statement_count(), 0);
    /// ```
    #[must_use]
    pub const fn statement_count(&self) -> usize {
        self.variants.len()
    }
}

impl<E: Entity> BelongsToAny<E> {
    /// Loads the relation for every parent: one statement per target type
    /// present, and none at all for a type nothing points at.
    ///
    /// `key` reads the discriminator and the key out of a parent;
    /// `#[derive(Entity)]` generates it.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`](crate::Error), plus an [`Error::Unsupported`](crate::Error::Unsupported) naming the
    /// discriminator when a parent points at a type the relation does not
    /// declare.
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Result, Tx};
    /// # use moso_orm::relation::{BelongsToAny, PolymorphicKeyFn};
    /// async fn load<E: Entity>(
    ///     relation: &BelongsToAny<E>,
    ///     rows: &mut [E],
    ///     key: PolymorphicKeyFn<E>,
    ///     tx: &Tx,
    /// ) -> Result<()> {
    ///     relation.load_all(rows, key, tx).await
    /// }
    /// ```
    pub async fn load_all(
        &self,
        parents: &mut [E],
        key: PolymorphicKeyFn<E>,
        executor: impl Executor<'_>,
    ) -> Result<()> {
        let handle = executor.handle();
        for variant in self.variants {
            let wanted: Vec<usize> = parents
                .iter()
                .enumerate()
                .filter(|(_, parent)| key(parent).0.as_str() == Some(variant.discriminant))
                .map(|(index, _)| index)
                .collect();
            if wanted.is_empty() {
                continue;
            }
            let node = (variant.node)(self.relation);
            let keys: Vec<Value> = wanted.iter().map(|&index| key(&parents[index]).1).collect();
            let payloads = node.payloads(&keys, handle).await?;
            for (index, payload) in wanted.into_iter().zip(payloads) {
                (variant.link)(&mut parents[index], payload)?;
            }
        }
        Ok(())
    }
}

impl<E: 'static> Clone for BelongsToAny<E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E: 'static> Copy for BelongsToAny<E> {}

impl<E: 'static> fmt::Debug for BelongsToAny<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BelongsToAny")
            .field("relation", &self.relation)
            .field("type_column", &self.type_column)
            .field("variants", &self.variants.len())
            .finish()
    }
}

/// One target of a [`BelongsToAny`].
///
/// ```
/// use moso_orm::relation::{PolymorphicVariant, Related};
/// # use moso_orm::{ColumnDef, DecodeError, Entity, Row};
/// # use moso_orm::descriptor::EntityDescriptor;
/// # use moso_sql::{TableRef, ValueKind};
/// # use std::sync::OnceLock;
/// # #[derive(Clone, Debug)] pub struct Post { pub id: i64 }
/// # impl Entity for Post {
/// #   type Pk = i64;
/// #   const TABLE: TableRef = TableRef::from_static("posts");
/// #   const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
/// #   const NAME: &'static str = "Post";
/// #   fn pk(&self) -> i64 { self.id }
/// #   fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
/// #   fn descriptor() -> &'static EntityDescriptor {
/// #     static D: OnceLock<EntityDescriptor> = OnceLock::new();
/// #     D.get_or_init(|| EntityDescriptor::builder("Post", Self::TABLE).build())
/// #   }
/// # }
/// # struct Reaction { target: Related<Post> }
/// const POST: PolymorphicVariant<Reaction> = PolymorphicVariant::to::<Post>("post", |r, rows| {
///     r.target = Related::Loaded(rows.into_required_row::<Post>()?);
///     Ok(())
/// });
///
/// assert_eq!(POST.discriminant(), "post");
/// assert_eq!(POST.target(), "Post");
/// ```
pub struct PolymorphicVariant<E> {
    discriminant: &'static str,
    target: &'static str,
    node: fn(&'static str) -> Preload,
    link: LinkFn<E>,
}

impl<E> PolymorphicVariant<E> {
    /// The variant stored as `discriminant`, loading a `T`.
    ///
    /// ```
    /// # use moso_orm::relation::PolymorphicVariant;
    /// # use moso_orm::{ColumnDef, DecodeError, Entity, Row};
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::{TableRef, ValueKind};
    /// # use std::sync::OnceLock;
    /// # #[derive(Clone, Debug)] pub struct Post { pub id: i64 }
    /// # impl Entity for Post {
    /// #   type Pk = i64;
    /// #   const TABLE: TableRef = TableRef::from_static("posts");
    /// #   const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
    /// #   const NAME: &'static str = "Post";
    /// #   fn pk(&self) -> i64 { self.id }
    /// #   fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
    /// #   fn descriptor() -> &'static EntityDescriptor {
    /// #     static D: OnceLock<EntityDescriptor> = OnceLock::new();
    /// #     D.get_or_init(|| EntityDescriptor::builder("Post", Self::TABLE).build())
    /// #   }
    /// # }
    /// # struct Reaction;
    /// const POST: PolymorphicVariant<Reaction> =
    ///     PolymorphicVariant::to::<Post>("post", |_, _| Ok(()));
    /// assert_eq!(POST.target(), "Post");
    /// ```
    #[must_use]
    pub const fn to<T: Entity>(discriminant: &'static str, link: LinkFn<E>) -> Self {
        Self {
            discriminant,
            target: T::NAME,
            node: polymorphic_node::<T>,
            link,
        }
    }

    /// The value the discriminator column holds for this variant.
    ///
    /// ```
    /// # use moso_orm::relation::PolymorphicVariant;
    /// # struct R;
    /// # struct T;
    /// # // a variant for a real entity is built with `to::<T>`
    /// fn discriminant<E>(v: &PolymorphicVariant<E>) -> &'static str {
    ///     v.discriminant()
    /// }
    /// ```
    #[must_use]
    pub const fn discriminant(&self) -> &'static str {
        self.discriminant
    }

    /// The target entity's name.
    ///
    /// ```
    /// # use moso_orm::relation::PolymorphicVariant;
    /// fn target<E>(v: &PolymorphicVariant<E>) -> &'static str {
    ///     v.target()
    /// }
    /// ```
    #[must_use]
    pub const fn target(&self) -> &'static str {
        self.target
    }
}

impl<E> Clone for PolymorphicVariant<E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for PolymorphicVariant<E> {}

impl<E> fmt::Debug for PolymorphicVariant<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolymorphicVariant")
            .field("discriminant", &self.discriminant)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// The preload node for one polymorphic target: a `belongs_to` on `T`'s primary
/// key.
fn polymorphic_node<T: Entity>(relation: &'static str) -> Preload {
    Preload::of::<T>(relation, RelationKind::BelongsTo)
}

/// Loads one relation for one row that is already in hand.
///
/// One statement. The batched form is [`load_many`], which is what to reach for
/// inside a loop — and what `moso check` will suggest when it finds one.
///
/// # Errors
///
/// Anything in [`Error`](crate::Error).
///
/// ```no_run
/// # use moso_orm::{Db, Entity, Relation, Result};
/// # use moso_orm::relation::load;
/// async fn hydrate<E: Entity, R: Relation<E>>(row: &mut E, relation: R, db: &Db) -> Result<()> {
///     load(row, relation, db).await
/// }
/// ```
pub async fn load<E: Entity, R: Relation<E>>(
    entity: &mut E,
    relation: R,
    executor: impl Executor<'_>,
) -> Result<()> {
    load_many(core::slice::from_mut(entity), relation, executor).await
}

/// Loads one relation for a batch of rows that are already in hand.
///
/// **One** statement for the batch, whatever its size. This exists so that "I
/// already have the rows and now I need the children" does not tempt anyone
/// into a loop.
///
/// # Errors
///
/// Anything in [`Error`](crate::Error).
///
/// ```no_run
/// # use moso_orm::{Db, Entity, Relation, Result};
/// # use moso_orm::relation::load_many;
/// async fn hydrate<E: Entity, R: Relation<E>>(
///     rows: &mut [E],
///     relation: R,
///     db: &Db,
/// ) -> Result<()> {
///     load_many(rows, relation, db).await
/// }
/// ```
pub async fn load_many<E: Entity, R: Relation<E>>(
    entities: &mut [E],
    relation: R,
    executor: impl Executor<'_>,
) -> Result<()> {
    load_many_with(entities, relation.preload(), executor).await
}

/// Loads a refined preload node — filtered, ordered, limited or nested — for a
/// batch of rows already in hand.
///
/// # Errors
///
/// Anything in [`Error`](crate::Error).
///
/// ```no_run
/// # use moso_orm::{Db, Entity, Preload, Result};
/// # use moso_orm::relation::load_many_with;
/// async fn hydrate<E: Entity>(rows: &mut [E], node: Preload, db: &Db) -> Result<()> {
///     load_many_with(rows, node, db).await
/// }
/// ```
pub async fn load_many_with<E: Entity>(
    entities: &mut [E],
    preload: Preload,
    executor: impl Executor<'_>,
) -> Result<()> {
    run_preloads(core::slice::from_ref(&preload), entities, executor.handle()).await
}

/// `entity.load(..)` and `Entity::load_many(..)`, as methods.
///
/// Implemented for every entity. `#[derive(Entity)]` does not need to generate
/// these — the blanket implementation gives them to any type that is an
/// [`Entity`] at all.
///
/// ```no_run
/// # use moso_orm::{Db, Entity, HasMany, Result};
/// # use moso_orm::relation::LoadRelations;
/// async fn hydrate<E: Entity, T: Entity>(
///     rows: &mut [E],
///     comments: HasMany<E, T>,
///     db: &Db,
/// ) -> Result<()> {
///     E::load_many(rows, comments, db).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a database entity, so it has no relations to load",
    label = "not an entity",
    note = "`load` and `load_many` are given to every `Entity`",
    note = "help: write `#[derive(moso::Entity)]` above `{Self}`, and mark its key `#[entity(pk)]`"
)]
pub trait LoadRelations: Entity {
    /// Loads `relation` for this row. One statement.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`](crate::Error).
    ///
    /// ```no_run
    /// # use moso_orm::{Db, Entity, Relation, Result};
    /// # use moso_orm::relation::LoadRelations;
    /// async fn one<E: Entity, R: Relation<E>>(row: &mut E, r: R, db: &Db) -> Result<()> {
    ///     row.load(r, db).await
    /// }
    /// ```
    fn load<'e, R: Relation<Self>>(
        &mut self,
        relation: R,
        executor: impl Executor<'e>,
    ) -> impl Future<Output = Result<()>> {
        load(self, relation, executor)
    }

    /// Loads `relation` for a batch. One statement for the batch.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`](crate::Error).
    ///
    /// ```no_run
    /// # use moso_orm::{Db, Entity, Relation, Result};
    /// # use moso_orm::relation::LoadRelations;
    /// async fn many<E: Entity, R: Relation<E>>(rows: &mut [E], r: R, db: &Db) -> Result<()> {
    ///     E::load_many(rows, r, db).await
    /// }
    /// ```
    fn load_many<'e, R: Relation<Self>>(
        entities: &mut [Self],
        relation: R,
        executor: impl Executor<'e>,
    ) -> impl Future<Output = Result<()>> {
        load_many(entities, relation, executor)
    }
}

#[diagnostic::do_not_recommend]
impl<E: Entity> LoadRelations for E {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::EntityDescriptor;
    use crate::row::{DecodeError, Row};
    use crate::{ColumnDef, Preload};
    use moso_sql::ValueKind;
    use std::sync::OnceLock;

    /// A post, with every relation shape hanging off it.
    #[derive(Clone, Debug)]
    pub struct Post {
        pub id: i64,
        pub author_id: i64,
        pub author: Related<User>,
        pub comments: Related<Vec<Comment>>,
        pub tags: Related<Vec<Tag>>,
    }

    /// A user.
    #[derive(Clone, Debug)]
    pub struct User {
        pub id: i64,
    }

    /// A comment.
    #[derive(Clone, Debug)]
    pub struct Comment {
        pub id: i64,
    }

    /// A tag.
    #[derive(Clone, Debug)]
    pub struct Tag {
        pub id: i64,
    }

    macro_rules! trivial_entity {
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
                    let _ = row;
                    unreachable!("no test decodes a {} without a database", stringify!($name))
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

    trivial_entity!(
        User,
        "users",
        &[ColumnDef::new("id", ValueKind::I64).primary_key()]
    );
    trivial_entity!(
        Tag,
        "tags",
        &[ColumnDef::new("id", ValueKind::I64).primary_key()]
    );
    trivial_entity!(
        Comment,
        "comments",
        &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("post_id", ValueKind::I64),
        ]
    );
    trivial_entity!(
        Post,
        "posts",
        &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("author_id", ValueKind::I64),
        ]
    );

    const AUTHOR: BelongsTo<Post, User> = BelongsTo::new("author", "author_id")
        .keyed_by(|post: &Post| Value::I64(post.author_id))
        .linking(|post, rows| {
            post.author = Related::Loaded(rows.into_required_row::<User>()?);
            Ok(())
        });

    const COMMENTS: HasMany<Post, Comment> =
        HasMany::new("comments", "post_id").linking(|post, rows| {
            post.comments = Related::Loaded(rows.into_rows::<Comment>()?);
            Ok(())
        });

    const TAGS: ManyToMany<Post, Tag> = ManyToMany::new("tags", "post_tags", "post_id", "tag_id")
        .linking(|post, rows| {
            post.tags = Related::Loaded(rows.into_rows::<Tag>()?);
            Ok(())
        });

    const PARENT: BelongsTo<Post, Post> = BelongsTo::new("parent", "parent_id").self_referential();

    #[test]
    fn every_relation_kind_makes_a_runnable_node() {
        for node in [
            Relation::<Post>::preload(&AUTHOR),
            Relation::<Post>::preload(&COMMENTS),
            Relation::<Post>::preload(&TAGS),
        ] {
            assert!(node.is_runnable(), "{node:?}");
            assert_eq!(node.statement_count(), 1);
            assert!(node.link_fn::<Post>().is_some(), "{node:?}");
        }
    }

    #[test]
    fn only_a_belongs_to_reads_the_parents_foreign_key() {
        assert!(
            Relation::<Post>::preload(&AUTHOR)
                .parent_key_fn::<Post>()
                .is_some()
        );
        assert!(
            Relation::<Post>::preload(&COMMENTS)
                .parent_key_fn::<Post>()
                .is_none()
        );
    }

    /// A `belongs_to` that cannot read its own foreign key must not quietly
    /// batch on the primary key: that returns rows, and they are the wrong ones.
    #[test]
    fn a_belongs_to_without_its_key_reader_is_refused_not_guessed() {
        const UNKEYED: BelongsTo<Post, User> =
            BelongsTo::new("author", "author_id").linking(|post, rows| {
                post.author = Related::Loaded(rows.into_required_row::<User>()?);
                Ok(())
            });

        let node = Relation::<Post>::preload(&UNKEYED);
        assert!(node.parent_key_fn::<Post>().is_none());
        // The refusal happens where the parents are read, which needs a handle;
        // what is asserted here is the precondition the guard fires on, and the
        // guard itself is one `if` above `Preload::payloads`.
        assert_eq!(node.kind(), RelationKind::BelongsTo);
    }

    #[test]
    fn a_self_referential_join_is_aliased_so_the_sql_is_unambiguous() {
        let joined = Relation::<Post>::join(&PARENT, JoinKind::Left);
        assert_eq!(joined.entity(), "Post");
        assert_eq!(
            joined.alias().map(|alias| alias.as_str().to_owned()),
            Some(String::from("posts_parent")),
            "a self join must not be `posts JOIN posts`"
        );

        let plain = Relation::<Post>::join(&COMMENTS, JoinKind::Inner);
        assert!(plain.alias().is_none());
    }

    #[test]
    fn a_count_preload_is_still_one_statement() {
        let counted = Relation::<Post>::count_preload(&COMMENTS);
        assert!(counted.is_counting());
        assert_eq!(counted.statement_count(), 1);
    }

    #[test]
    fn a_relation_converts_into_a_preload() {
        let node: Preload = COMMENTS.into();
        assert_eq!(node.relation(), "comments");
        assert_eq!(node.target(), "Comment");
        assert_eq!(node.key_column(), Some("post_id"));
    }

    #[test]
    fn a_many_to_many_node_carries_its_join_table() {
        let node = Relation::<Post>::preload(&TAGS);
        assert_eq!(node.join_table(), Some("post_tags"));
    }

    fn attachment() -> Attachment<Post, Tag> {
        TAGS.on(&Post {
            id: 7,
            author_id: 1,
            author: Related::NotLoaded,
            comments: Related::NotLoaded,
            tags: Related::NotLoaded,
        })
    }

    #[test]
    fn attach_is_one_idempotent_statement() {
        let statement = attachment()
            .attach_statement(&[1, 2, 3])
            .expect("three targets");
        let Statement::Insert(insert) = statement else {
            panic!("attach is an INSERT");
        };
        assert_eq!(insert.row_count(), 3);
        assert_eq!(insert.table().name().as_str(), "post_tags");
        assert!(
            insert.conflict().is_some(),
            "attaching twice must not be an error"
        );
    }

    #[test]
    fn attaching_nothing_is_no_statement_at_all() {
        assert!(attachment().attach_statement(&[]).is_none());
        assert!(
            attachment()
                .detach_statement(&[], Backend::Postgres)
                .is_none()
        );
    }

    #[test]
    fn detach_is_one_statement_scoped_to_the_owner() {
        let statement = attachment()
            .detach_statement(&[1, 2], Backend::Postgres)
            .expect("two targets");
        let Statement::Delete(delete) = statement else {
            panic!("detach is a DELETE");
        };
        assert_eq!(delete.target().name().as_str(), "post_tags");
        assert_eq!(
            delete.filters().len(),
            2,
            "one filter for the owner, one for the targets"
        );
    }

    #[test]
    fn sync_is_one_statement_where_the_dialect_allows_and_two_where_it_does_not() {
        let postgres = attachment().sync_statements(&[1, 2], Backend::Postgres);
        assert_eq!(postgres.len(), 1, "a data-modifying CTE does both halves");
        let Statement::Insert(insert) = &postgres[0] else {
            panic!("the surviving statement is the INSERT");
        };
        assert_eq!(insert.ctes().len(), 1, "the DELETE rides in the WITH");

        let sqlite = attachment().sync_statements(&[1, 2], Backend::Sqlite);
        assert_eq!(sqlite.len(), 2, "SQLite has no data-modifying CTEs");
        assert!(matches!(sqlite[0], Statement::Delete(_)));
        assert!(matches!(sqlite[1], Statement::Insert(_)));
    }

    #[test]
    fn syncing_to_nothing_is_a_single_delete() {
        for backend in [Backend::Postgres, Backend::Sqlite] {
            let statements = attachment().sync_statements(&[], backend);
            assert_eq!(statements.len(), 1);
            assert!(matches!(statements[0], Statement::Delete(_)));
        }
    }

    /// Renders `statement` for `backend`, so the write SQL can be asserted.
    fn render(statement: &Statement, backend: Backend) -> String {
        statement.build(backend.dialect()).expect("renders").text
    }

    /// D9: the write side gets a snapshot on both dialects too.
    #[test]
    fn the_rendered_write_sql_says_what_it_does_on_both_dialects() {
        let attach = attachment().attach_statement(&[1, 2]).expect("two targets");
        assert_eq!(
            render(&attach, Backend::Postgres),
            "INSERT INTO \"post_tags\" (\"post_id\", \"tag_id\") VALUES ($1, $2), ($3, $4) \
             ON CONFLICT (\"post_id\", \"tag_id\") DO NOTHING"
        );

        let detach = attachment()
            .detach_statement(&[1, 2], Backend::Postgres)
            .expect("two targets");
        assert_eq!(
            render(&detach, Backend::Postgres),
            "DELETE FROM \"post_tags\" WHERE \"post_tags\".\"post_id\" = $1 \
             AND \"post_tags\".\"tag_id\" = ANY ($2)"
        );

        let synced = attachment().sync_statements(&[1, 2], Backend::Postgres);
        assert_eq!(synced.len(), 1);
        assert_eq!(
            render(&synced[0], Backend::Postgres),
            "WITH \"moso_detached\" AS (DELETE FROM \"post_tags\" \
             WHERE \"post_tags\".\"post_id\" = $1 AND \"post_tags\".\"tag_id\" <> ALL ($2)) \
             INSERT INTO \"post_tags\" (\"post_id\", \"tag_id\") VALUES ($3, $4), ($5, $6) \
             ON CONFLICT (\"post_id\", \"tag_id\") DO NOTHING"
        );

        let sqlite = attachment().sync_statements(&[1, 2], Backend::Sqlite);
        assert_eq!(sqlite.len(), 2, "SQLite has no data-modifying CTEs");
        assert_eq!(
            render(&sqlite[0], Backend::Sqlite),
            "DELETE FROM \"post_tags\" WHERE \"post_tags\".\"post_id\" = ? \
             AND \"post_tags\".\"tag_id\" NOT IN (?, ?)"
        );
        assert_eq!(
            render(&sqlite[1], Backend::Sqlite),
            "INSERT INTO \"post_tags\" (\"post_id\", \"tag_id\") VALUES (?, ?), (?, ?) \
             ON CONFLICT (\"post_id\", \"tag_id\") DO NOTHING"
        );
    }

    #[test]
    fn a_polymorphic_relation_costs_one_statement_per_declared_type() {
        static VARIANTS: &[PolymorphicVariant<Post>] = &[
            PolymorphicVariant::to::<User>("user", |_, _| Ok(())),
            PolymorphicVariant::to::<Tag>("tag", |_, _| Ok(())),
        ];
        const TARGET: BelongsToAny<Post> =
            BelongsToAny::new("target", "target_type", "target_id", VARIANTS);

        assert_eq!(TARGET.statement_count(), 2);
        assert_eq!(TARGET.variants()[0].discriminant(), "user");
        assert_eq!(TARGET.variants()[1].target(), "Tag");
    }
}
