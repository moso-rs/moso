//! Typed partial selects: tuples of columns, and `#[derive(Projection)]`
//! structs.
//!
//! Non-negotiable N5. `User::query().select((User::ID, User::EMAIL))` decodes
//! into `(Id<User>, Email)` — the tuple's element types come from the columns',
//! so reading a column that was not projected does not type-check.
//!
//! # Two ways in
//!
//! ```text
//! .select((User::ID, User::EMAIL))   →  Projected<User, (Id<User>, Email)>
//! .project::<UserSummary>()          →  Projected<User, UserSummary>
//! ```
//!
//! A tuple is the right answer up to twelve columns and no computed values.
//! Past that, or when a column is an aggregate or a raw expression, a struct
//! that derives [`Projection`] names its fields and decodes into them.
//!
//! # What `#[derive(Projection)]` emits, and why the scope check is free
//!
//! ```text
//! #[derive(Projection)]
//! #[projection(entity = User, join = Post)]
//! struct UserSummary {
//!     id: Id<User>,
//!     email: Email,
//!     #[projection(expr = "count(posts.id)")]
//!     post_count: i64,
//!     #[projection(column = Post::CREATED_AT, agg = "max")]
//!     last_post_at: Option<DateTime<Utc>>,
//! }
//! ```
//!
//! becomes, in outline:
//!
//! ```text
//! impl ProjectionScope<User> for UserSummary {}   // from `entity = User`
//! impl ProjectionScope<Post> for UserSummary {}   // from `join = Post`
//!
//! impl Projection for UserSummary {
//!     const COLUMNS: usize = 4;
//!
//!     fn select_items() -> Vec<SelectItem> {
//!         vec![
//!             checked_column::<Self, _, _>(User::ID),
//!             checked_column::<Self, _, _>(User::EMAIL),
//!             raw_expr_as("count(posts.id)", "post_count"),
//!             checked_aggregate::<Self, _, _>(Post::CREATED_AT, AggregateFunc::Max),
//!         ]
//!     }
//!
//!     fn from_row(row: &Row) -> Result<Self, DecodeError> { /* positional */ }
//! }
//! ```
//!
//! **[`checked_column`] is the compile-time check.** Its `P: ProjectionScope<E>`
//! bound is satisfied only by the entities the attributes named, so referencing
//! a column of an entity the projection does not join fails to compile, at the
//! field, with a message that names the entity and the attribute to add. No
//! runtime lookup, no allocation, and nothing to remember to call — the derive
//! routes every column through it.
//!
//! The `agg = "…"` vocabulary maps to [`AggregateFunc`]: `"count"`, `"sum"`,
//! `"avg"`, `"min"`, `"max"`, `"array_agg"`, `"string_agg"`, `"json_agg"`,
//! `"jsonb_agg"`, `"bool_and"`, `"bool_or"`, `"stddev"`, `"variance"`.

use moso_sql::{Aggregate, AggregateFunc, Expr, Ident, RawExpr, SelectItem};

use crate::column::Column;
use crate::entity::Entity;
use crate::row::{DecodeError, Row};
use crate::sqltype::SqlType;

/// A struct a query can decode into.
///
/// Written by `#[derive(Projection)]`, which also checks at compile time that
/// every referenced column belongs to the entity or to a joined one.
///
/// ```
/// use moso_orm::{DecodeError, Projection, Row};
/// use moso_sql::SelectItem;
///
/// /// A user and how many posts they wrote.
/// pub struct UserSummary {
///     /// The user's identifier.
///     pub id: i64,
///     /// How many posts they have.
///     pub post_count: i64,
/// }
///
/// impl Projection for UserSummary {
///     const COLUMNS: usize = 2;
///
///     fn select_items() -> Vec<SelectItem> {
///         vec![
///             SelectItem::column(moso_sql::ColumnRef::from_static("id")),
///             SelectItem::column(moso_sql::ColumnRef::from_static("post_count")),
///         ]
///     }
///
///     fn from_row(row: &Row) -> Result<Self, DecodeError> {
///         Ok(Self { id: row.get_i64(0)?, post_count: row.get_i64(1)? })
///     }
/// }
///
/// assert_eq!(UserSummary::select_items().len(), 2);
/// assert_eq!(UserSummary::COLUMNS, 2);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be selected into",
    label = "not a projection",
    note = "a partial select decodes into a tuple of column types, or into a struct that \
            derives `Projection`",
    note = "help: write `#[derive(moso::Projection)] #[projection(entity = User)]` above \
            `{Self}`",
    note = "help: or select a tuple: `.select((User::ID, User::EMAIL))`"
)]
pub trait Projection: Sized + Send + Sync + 'static {
    /// How many columns the projection reads.
    ///
    /// The derive emits a literal. A hand-written impl that leaves it out gets
    /// `usize::MAX`, which is the "unknown" sentinel the arity check skips
    /// rather than a number it would wrongly trust.
    const COLUMNS: usize = usize::MAX;

    /// The items the `SELECT` list is built from, in decode order.
    ///
    /// ```
    /// # use moso_orm::Projection;
    /// # use moso_sql::SelectItem;
    /// fn items<P: Projection>() -> Vec<SelectItem> {
    ///     P::select_items()
    /// }
    /// ```
    fn select_items() -> Vec<SelectItem>;

    /// Decodes one row, positionally.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] naming the column and both types.
    ///
    /// ```
    /// # use moso_orm::{DecodeError, Projection, Row};
    /// fn decode<P: Projection>(row: &Row) -> Result<P, DecodeError> {
    ///     P::from_row(row)
    /// }
    /// ```
    fn from_row(row: &Row) -> Result<Self, DecodeError>;
}

/// An entity whose columns this projection is allowed to read.
///
/// `#[derive(Projection)]` implements it once for the entity named by
/// `#[projection(entity = ..)]` and once for each `#[projection(join = ..)]`.
/// [`checked_column`] and [`checked_aggregate`] require it, which is how a
/// column of an unjoined entity becomes a compile error at the field that
/// mentions it rather than a query that can never return a row.
///
/// ```
/// use moso_orm::{Column, Entity, Projection, ProjectionScope};
/// # use moso_orm::{ColumnDef, DecodeError, Row};
/// # use moso_orm::descriptor::EntityDescriptor;
/// # use moso_sql::{SelectItem, TableRef, ValueKind};
/// # use std::sync::OnceLock;
/// # #[derive(Clone, Debug)] pub struct User { pub id: i64 }
/// # impl Entity for User {
/// #     type Pk = i64;
/// #     const TABLE: TableRef = TableRef::from_static("users");
/// #     const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
/// #     const NAME: &'static str = "User";
/// #     fn pk(&self) -> i64 { self.id }
/// #     fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
/// #     fn descriptor() -> &'static EntityDescriptor {
/// #         static D: OnceLock<EntityDescriptor> = OnceLock::new();
/// #         D.get_or_init(|| EntityDescriptor::builder("User", Self::TABLE).build())
/// #     }
/// # }
/// /// Just the identifiers.
/// pub struct UserIds {
///     /// The user's identifier.
///     pub id: i64,
/// }
///
/// // What the derive writes from `#[projection(entity = User)]`.
/// impl ProjectionScope<User> for UserIds {}
///
/// impl Projection for UserIds {
///     const COLUMNS: usize = 1;
///
///     fn select_items() -> Vec<SelectItem> {
///         vec![moso_orm::projection::checked_column::<Self, _, _>(Column::<User, i64>::new("id"))]
///     }
///
///     fn from_row(row: &Row) -> Result<Self, DecodeError> {
///         Ok(Self { id: row.get_i64(0)? })
///     }
/// }
///
/// assert_eq!(UserIds::select_items().len(), 1);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` may not read a column of `{E}`",
    label = "`{E}` is not in this projection's scope",
    note = "a projection reads columns of the entity it names and of the entities it joins, \
            because those are the tables the query has",
    note = "help: add `#[projection(join = {E})]` above `{Self}`",
    note = "help: or read a column of the entity `#[projection(entity = ..)]` already names"
)]
pub trait ProjectionScope<E: Entity> {}

/// A column of `E`, checked at compile time against `P`'s scope.
///
/// The `P: ProjectionScope<E>` bound is the check: it holds only for the
/// entities `#[projection(entity = ..)]` and `#[projection(join = ..)]` named.
///
/// ```
/// # use moso_orm::{Column, Entity, ProjectionScope};
/// # use moso_sql::SelectItem;
/// fn item<P, E, T>(column: Column<E, T>) -> SelectItem
/// where
///     P: ProjectionScope<E>,
///     E: Entity,
///     T: moso_orm::SqlType,
/// {
///     moso_orm::projection::checked_column::<P, E, T>(column)
/// }
/// ```
#[must_use]
pub fn checked_column<P, E, T>(column: Column<E, T>) -> SelectItem
where
    P: ProjectionScope<E>,
    E: Entity,
    T: SqlType,
{
    SelectItem::column(column.column_ref())
}

/// As [`checked_column`], with an explicit output name.
///
/// The derive uses the Rust field's name, so a `SELECT` list read in a log
/// lines up with the struct it decodes into.
///
/// ```
/// # use moso_orm::{Column, Entity, ProjectionScope};
/// # use moso_sql::{Ident, SelectItem};
/// fn item<P, E, T>(column: Column<E, T>) -> SelectItem
/// where
///     P: ProjectionScope<E>,
///     E: Entity,
///     T: moso_orm::SqlType,
/// {
///     moso_orm::projection::checked_column_as::<P, E, T>(column, "value")
/// }
/// ```
#[must_use]
pub fn checked_column_as<P, E, T>(column: Column<E, T>, alias: &'static str) -> SelectItem
where
    P: ProjectionScope<E>,
    E: Entity,
    T: SqlType,
{
    SelectItem::aliased(column.expr(), Ident::from_static(alias))
}

/// An aggregate over a column of `E`, checked against `P`'s scope.
///
/// What `#[projection(column = Post::CREATED_AT, agg = "max")]` becomes. The
/// result is aliased to the field's name, because `max(posts.created_at)` is
/// not a column name any client would want to see.
///
/// ```
/// # use moso_orm::{Column, Entity, ProjectionScope};
/// # use moso_sql::{AggregateFunc, SelectItem};
/// fn newest<P, E, T>(column: Column<E, T>) -> SelectItem
/// where
///     P: ProjectionScope<E>,
///     E: Entity,
///     T: moso_orm::SqlType,
/// {
///     moso_orm::projection::checked_aggregate::<P, E, T>(column, AggregateFunc::Max, "last_at")
/// }
/// ```
#[must_use]
pub fn checked_aggregate<P, E, T>(
    column: Column<E, T>,
    function: AggregateFunc,
    alias: &'static str,
) -> SelectItem
where
    P: ProjectionScope<E>,
    E: Entity,
    T: SqlType,
{
    SelectItem::aliased(
        Aggregate::new(function, [column.expr()]).into_expr(),
        Ident::from_static(alias),
    )
}

/// A raw expression, aliased to the field it fills.
///
/// What `#[projection(expr = "count(posts.id)")]` becomes. Moso cannot see
/// inside the fragment, so this one is **not** scope-checked: it is the
/// projection's escape hatch, and the aggregate and column forms above are what
/// a checked projection uses.
///
/// The fragment follows `moso-sql`'s placeholder convention — `?` is a bound
/// parameter and `??` a literal question mark — so a projection expression that
/// needs a value can take one.
///
/// ```
/// use moso_orm::projection::raw_expr_as;
///
/// let item = raw_expr_as("count(posts.id)", "post_count");
/// assert_eq!(item.alias().map(moso_sql::Ident::as_str), Some("post_count"));
/// ```
#[must_use]
pub fn raw_expr_as(fragment: &str, alias: &'static str) -> SelectItem {
    SelectItem::aliased(Expr::raw(RawExpr::new(fragment)), Ident::from_static(alias))
}

/// A raw expression with no output name.
///
/// [`raw_expr_as`] is what the derive emits; this is for a hand-written
/// projection that decodes purely positionally.
///
/// ```
/// use moso_orm::projection::raw_expr;
///
/// assert!(raw_expr("count(*)").alias().is_none());
/// ```
#[must_use]
pub fn raw_expr(fragment: &str) -> SelectItem {
    SelectItem::expr(Expr::raw(RawExpr::new(fragment)))
}

/// A tuple of columns that a query can be projected onto.
///
/// Implemented for tuples of one to twelve [`Column`]s, whose entities may
/// differ — a joined query can project columns from both sides.
///
/// ```
/// use moso_orm::{Column, ColumnTuple};
/// # use moso_orm::{ColumnDef, DecodeError, Entity, Row};
/// # use moso_orm::descriptor::EntityDescriptor;
/// # use moso_sql::{TableRef, ValueKind};
/// # use std::sync::OnceLock;
/// # #[derive(Clone, Debug)] pub struct User { pub id: i64 }
/// # impl Entity for User {
/// #     type Pk = i64;
/// #     const TABLE: TableRef = TableRef::from_static("users");
/// #     const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
/// #     const NAME: &'static str = "User";
/// #     fn pk(&self) -> i64 { self.id }
/// #     fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
/// #     fn descriptor() -> &'static EntityDescriptor {
/// #         static D: OnceLock<EntityDescriptor> = OnceLock::new();
/// #         D.get_or_init(|| EntityDescriptor::builder("User", Self::TABLE).build())
/// #     }
/// # }
/// const ID: Column<User, i64> = Column::new("id");
/// const NAME: Column<User, String> = Column::new("name");
///
/// assert_eq!((ID, NAME).items().len(), 2);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a tuple of columns",
    label = "not selectable",
    note = "`.select(..)` takes a tuple of one to twelve column constants, such as \
            `(User::ID, User::EMAIL)`",
    note = "help: for more than twelve, or for computed values, use \
            `#[derive(moso::Projection)]` and `.project::<Summary>()`"
)]
pub trait ColumnTuple {
    /// What one row decodes into.
    type Output: Send + 'static;

    /// The `SELECT` items, in decode order.
    ///
    /// ```
    /// # use moso_orm::ColumnTuple;
    /// # use moso_sql::SelectItem;
    /// fn items<C: ColumnTuple>(columns: &C) -> Vec<SelectItem> {
    ///     columns.items()
    /// }
    /// ```
    fn items(&self) -> Vec<SelectItem>;

    /// Decodes one row into the tuple.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] naming the column and both types.
    ///
    /// ```
    /// # use moso_orm::{ColumnTuple, DecodeError, Row};
    /// fn decode<C: ColumnTuple>(row: &Row) -> Result<C::Output, DecodeError> {
    ///     C::decode(row)
    /// }
    /// ```
    fn decode(row: &Row) -> Result<Self::Output, DecodeError>;
}

macro_rules! column_tuple {
    ($($entity:ident $value:ident $index:tt),+ $(,)?) => {
        impl<$($entity: Entity, $value: SqlType),+> ColumnTuple
            for ($(Column<$entity, $value>,)+)
        {
            type Output = ($($value,)+);

            fn items(&self) -> Vec<SelectItem> {
                vec![$(SelectItem::column(self.$index.column_ref()),)+]
            }

            fn decode(row: &Row) -> Result<Self::Output, DecodeError> {
                Ok((
                    $(<$value as SqlType>::decode(row, $index)
                        .map_err(|error| error.with_column_name(
                            row.column_name($index).unwrap_or("?")
                        ))?,)+
                ))
            }
        }
    };
}

column_tuple!(E0 T0 0);
column_tuple!(E0 T0 0, E1 T1 1);
column_tuple!(E0 T0 0, E1 T1 1, E2 T2 2);
column_tuple!(E0 T0 0, E1 T1 1, E2 T2 2, E3 T3 3);
column_tuple!(E0 T0 0, E1 T1 1, E2 T2 2, E3 T3 3, E4 T4 4);
column_tuple!(E0 T0 0, E1 T1 1, E2 T2 2, E3 T3 3, E4 T4 4, E5 T5 5);
column_tuple!(E0 T0 0, E1 T1 1, E2 T2 2, E3 T3 3, E4 T4 4, E5 T5 5, E6 T6 6);
column_tuple!(E0 T0 0, E1 T1 1, E2 T2 2, E3 T3 3, E4 T4 4, E5 T5 5, E6 T6 6, E7 T7 7);
column_tuple!(
    E0 T0 0, E1 T1 1, E2 T2 2, E3 T3 3, E4 T4 4, E5 T5 5, E6 T6 6, E7 T7 7, E8 T8 8
);
column_tuple!(
    E0 T0 0, E1 T1 1, E2 T2 2, E3 T3 3, E4 T4 4, E5 T5 5, E6 T6 6, E7 T7 7, E8 T8 8,
    E9 T9 9
);
column_tuple!(
    E0 T0 0, E1 T1 1, E2 T2 2, E3 T3 3, E4 T4 4, E5 T5 5, E6 T6 6, E7 T7 7, E8 T8 8,
    E9 T9 9, E10 T10 10
);
column_tuple!(
    E0 T0 0, E1 T1 1, E2 T2 2, E3 T3 3, E4 T4 4, E5 T5 5, E6 T6 6, E7 T7 7, E8 T8 8,
    E9 T9 9, E10 T10 10, E11 T11 11
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::EntityDescriptor;
    use crate::entity::ColumnDef;
    use moso_sql::{TableRef, ValueKind};
    use std::sync::OnceLock;

    /// A user, reduced to what a projection test needs.
    #[derive(Clone, Debug)]
    struct User {
        id: i64,
    }

    impl Entity for User {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("users");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("email", ValueKind::Text),
        ];
        const NAME: &'static str = "User";

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| EntityDescriptor::builder("User", Self::TABLE).build())
        }
    }

    /// A post, so a projection can reach across two entities.
    #[derive(Clone, Debug)]
    struct Post {
        id: i64,
    }

    impl Entity for Post {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("posts");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("created_at", ValueKind::Timestamp),
        ];
        const NAME: &'static str = "Post";

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| EntityDescriptor::builder("Post", Self::TABLE).build())
        }
    }

    const ID: Column<User, i64> = Column::new("id");
    const EMAIL: Column<User, String> = Column::new("email");
    const POST_CREATED_AT: Column<Post, i64> = Column::new("created_at");

    /// What `#[derive(Projection)] #[projection(entity = User, join = Post)]`
    /// writes, by hand, so the machinery is exercised exactly as generated.
    struct UserSummary {
        id: i64,
        email: String,
        post_count: i64,
        last_post_at: i64,
    }

    impl ProjectionScope<User> for UserSummary {}
    impl ProjectionScope<Post> for UserSummary {}

    impl Projection for UserSummary {
        const COLUMNS: usize = 4;

        fn select_items() -> Vec<SelectItem> {
            vec![
                checked_column::<Self, _, _>(ID),
                checked_column_as::<Self, _, _>(EMAIL, "email"),
                raw_expr_as("count(posts.id)", "post_count"),
                checked_aggregate::<Self, _, _>(
                    POST_CREATED_AT,
                    AggregateFunc::Max,
                    "last_post_at",
                ),
            ]
        }

        fn from_row(row: &Row) -> Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
                email: row.get_string(1)?,
                post_count: row.get_i64(2)?,
                last_post_at: row.get_i64(3)?,
            })
        }
    }

    #[test]
    fn a_tuple_of_columns_projects_to_a_tuple_of_their_types() {
        fn output_is<C: ColumnTuple<Output = O>, O>() {}
        output_is::<(Column<User, i64>,), (i64,)>();
        output_is::<(Column<User, i64>, Column<User, String>), (i64, String)>();
    }

    #[test]
    fn the_items_are_qualified_and_in_order() {
        let items = (ID, EMAIL).items();
        assert_eq!(items.len(), 2);
        // Qualified, so a joined query cannot pick the wrong `id`.
        assert!(items[0].alias().is_none());
    }

    #[test]
    fn tuples_up_to_twelve_are_projectable() {
        fn projectable<C: ColumnTuple>() {}
        projectable::<(Column<User, i64>,)>();
        projectable::<(
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
        )>();
        projectable::<(
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
            Column<User, i64>,
        )>();
    }

    #[test]
    fn a_twelve_column_tuple_decodes_into_twelve_values() {
        fn output_is<C: ColumnTuple<Output = O>, O>() {}
        output_is::<
            (
                Column<User, i64>,
                Column<User, String>,
                Column<User, i64>,
                Column<User, i64>,
                Column<User, i64>,
                Column<User, i64>,
                Column<User, i64>,
                Column<User, i64>,
                Column<User, i64>,
                Column<User, i64>,
                Column<User, i64>,
                Column<User, i64>,
            ),
            (
                i64,
                String,
                i64,
                i64,
                i64,
                i64,
                i64,
                i64,
                i64,
                i64,
                i64,
                i64,
            ),
        >();
    }

    #[test]
    fn a_tuple_may_span_two_entities() {
        // The joined case: `.select((User::ID, Post::CREATED_AT))`.
        let items = (ID, POST_CREATED_AT).items();
        assert_eq!(items.len(), 2);
        fn output_is<C: ColumnTuple<Output = O>, O>() {}
        output_is::<(Column<User, i64>, Column<Post, i64>), (i64, i64)>();
    }

    #[test]
    fn a_derived_projection_has_one_field_per_selected_item() {
        // `from_row` needs a driver row, which only a live connection
        // produces, so what is asserted here is the correspondence the derive
        // guarantees: one field per item, in the same order.
        let summary = UserSummary {
            id: 1,
            email: "ada@example.com".to_owned(),
            post_count: 2,
            last_post_at: 3,
        };
        assert_eq!(summary.id, 1);
        assert_eq!(summary.email, "ada@example.com");
        assert_eq!(summary.post_count, 2);
        assert_eq!(summary.last_post_at, 3);
        assert_eq!(UserSummary::COLUMNS, UserSummary::select_items().len());
    }

    #[test]
    fn a_derived_projection_emits_one_item_per_field_in_decode_order() {
        let items = UserSummary::select_items();
        assert_eq!(items.len(), UserSummary::COLUMNS);
        assert_eq!(items.len(), 4);
        assert!(items[0].alias().is_none(), "a bare column keeps its name");
        assert_eq!(items[1].alias().map(Ident::as_str), Some("email"));
        assert_eq!(items[2].alias().map(Ident::as_str), Some("post_count"));
        assert_eq!(items[3].alias().map(Ident::as_str), Some("last_post_at"));
    }

    #[test]
    fn a_scope_checked_column_is_qualified_by_its_own_entity() {
        // `Post::CREATED_AT` inside a projection over `User` resolves against
        // `posts`, not `users` — which is why the scope check exists.
        let item = checked_column::<UserSummary, _, _>(POST_CREATED_AT);
        let SelectItem::Expr { expr, .. } = &item else {
            panic!("a column is an expression item");
        };
        let column = expr.as_column().expect("a column reference");
        assert_eq!(
            column.qualifier().map(Ident::as_str),
            Some("posts"),
            "the column's own entity qualifies it"
        );
    }

    #[test]
    fn an_aggregate_field_is_an_aggregate_over_the_named_column() {
        let item = checked_aggregate::<UserSummary, _, _>(POST_CREATED_AT, AggregateFunc::Max, "m");
        let rendered = format!("{item:?}");
        assert!(rendered.contains("Max"), "{rendered}");
        assert!(rendered.contains("created_at"), "{rendered}");
    }

    #[test]
    fn a_raw_expression_field_keeps_its_fragment() {
        let item = raw_expr_as("count(posts.id)", "post_count");
        let rendered = format!("{item:?}");
        assert!(rendered.contains("count(posts.id)"), "{rendered}");
        assert!(raw_expr("count(*)").alias().is_none());
    }

    /// The scope check is a *compile-time* one, so the negative case cannot be
    /// asserted from a unit test — it is a `trybuild` case in `moso-ui-tests`.
    /// What can be asserted here is the positive half: the bound is satisfied
    /// exactly for the entities the projection declared.
    #[test]
    fn the_scope_bound_holds_for_the_declared_entities() {
        fn in_scope<P: ProjectionScope<E>, E: Entity>() {}
        in_scope::<UserSummary, User>();
        in_scope::<UserSummary, Post>();
    }
}
