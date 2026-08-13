//! Expressions that need a query to build: subqueries, aggregates over the
//! whole table, and the raw escape hatch.
//!
//! # What is *not* here
//!
//! Almost everything. Comparisons come off [`Column`] (`User::AGE.gt(18)`),
//! combination comes off [`Predicate`] (`a & b`, `!a`,
//! [`all`](crate::all) / [`any`](crate::any) / [`not`](crate::not)), and the
//! full expression grammar — casts, window functions, `CASE`, every scalar
//! function — is [`moso_sql::Expr`], which is public and which every one of
//! these helpers returns. This module holds only the constructors that cannot
//! live anywhere else because they need to render a whole [`Select`] first.
//!
//! # Subqueries
//!
//! ```
//! use moso_orm::expr;
//! # use moso_orm::{Column, ColumnDef, DecodeError, Entity, Result, Row, Select};
//! # use moso_orm::descriptor::EntityDescriptor;
//! # use moso_sql::{TableRef, ValueKind};
//! # use std::sync::OnceLock;
//! # #[derive(Clone, Debug)] pub struct Post { pub id: i64 }
//! # impl Entity for Post {
//! #     type Pk = i64;
//! #     const TABLE: TableRef = TableRef::from_static("posts");
//! #     const COLUMNS: &'static [ColumnDef] = &[
//! #         ColumnDef::new("id", ValueKind::I64).primary_key(),
//! #         ColumnDef::new("author_id", ValueKind::I64),
//! #     ];
//! #     const NAME: &'static str = "Post";
//! #     fn pk(&self) -> i64 { self.id }
//! #     fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
//! #     fn descriptor() -> &'static EntityDescriptor {
//! #         static D: OnceLock<EntityDescriptor> = OnceLock::new();
//! #         D.get_or_init(|| EntityDescriptor::builder("Post", Self::TABLE).build())
//! #     }
//! # }
//! # const POST_ID: Column<Post, i64> = Column::new("id");
//! # fn demo() -> Result<()> {
//! // `exists (select 1 from posts where posts.id > 10)`
//! let has_posts = expr::exists(&Select::<Post>::new().filter(POST_ID.gt(10)))?;
//! assert!(has_posts.entities().is_empty());
//! # Ok(())
//! # }
//! # demo().unwrap();
//! ```

use moso_sql::{Aggregate, AggregateFunc, Case, Expr, Function, RawExpr, TrimMode, Value};

use crate::column::Column;
use crate::entity::Entity;
use crate::error::Result;
use crate::predicate::Predicate;
use crate::select::Select;
use crate::sqltype::SqlType;

/// `exists (select 1 from … )`.
///
/// The subquery is rendered with the same soft-delete, tenant and joined-set
/// rules as a top-level query, so an `EXISTS` over a tenant-scoped entity still
/// has to name its tenant.
///
/// The returned [`Predicate`] has an **empty** entity set: a subquery's columns
/// are resolved inside the subquery, so the outer query's scope check has
/// nothing to say about them.
///
/// # Errors
///
/// Whatever [`Select::to_statement`] would return — an out-of-scope filter, or
/// a missing tenant.
///
/// ```
/// use moso_orm::expr;
/// # use moso_orm::{Entity, Predicate, Result, Select};
/// fn any_at_all<E: Entity>(query: &Select<E>) -> Result<Predicate> {
///     expr::exists(query)
/// }
/// ```
pub fn exists<E: Entity, J>(query: &Select<E, J>) -> Result<Predicate> {
    Ok(Predicate::unchecked(Expr::exists(query.to_subquery()?)))
}

/// `not exists (select 1 from … )`.
///
/// # Errors
///
/// As [`exists`].
///
/// ```
/// use moso_orm::expr;
/// # use moso_orm::{Entity, Predicate, Result, Select};
/// fn none_at_all<E: Entity>(query: &Select<E>) -> Result<Predicate> {
///     expr::not_exists(query)
/// }
/// ```
pub fn not_exists<E: Entity, J>(query: &Select<E, J>) -> Result<Predicate> {
    Ok(Predicate::unchecked(Expr::not_exists(query.to_subquery()?)))
}

/// `outer in (select inner from … )`.
///
/// The two columns share `T`, which is the same trick [`Column::eq_col`] uses:
/// `Post::AUTHOR_ID.in_query(User::ID, …)` compiles and
/// `Post::TITLE.in_query(User::ID, …)` does not.
///
/// The result mentions only `outer`'s entity, because `inner` is resolved
/// inside the subquery.
///
/// # Errors
///
/// As [`exists`].
///
/// ```
/// use moso_orm::expr;
/// # use moso_orm::{Column, Entity, Predicate, Result, Select};
/// fn authored_by<A: Entity, B: Entity>(
///     author: Column<A, i64>,
///     id: Column<B, i64>,
///     admins: &Select<B>,
/// ) -> Result<Predicate> {
///     expr::in_query(author, id, admins)
/// }
/// ```
pub fn in_query<A: Entity, B: Entity, T: SqlType, J>(
    outer: Column<A, T>,
    inner: Column<B, T>,
    query: &Select<B, J>,
) -> Result<Predicate> {
    let subquery = query.to_column_subquery(inner)?;
    Ok(Predicate::of([A::NAME], outer.expr().in_subquery(subquery)))
}

/// `outer not in (select inner from … )`.
///
/// # Errors
///
/// As [`exists`].
///
/// ```
/// use moso_orm::expr;
/// # use moso_orm::{Column, Entity, Predicate, Result, Select};
/// fn not_authored_by<A: Entity, B: Entity>(
///     author: Column<A, i64>,
///     id: Column<B, i64>,
///     banned: &Select<B>,
/// ) -> Result<Predicate> {
///     expr::not_in_query(author, id, banned)
/// }
/// ```
pub fn not_in_query<A: Entity, B: Entity, T: SqlType, J>(
    outer: Column<A, T>,
    inner: Column<B, T>,
    query: &Select<B, J>,
) -> Result<Predicate> {
    let subquery = query.to_column_subquery(inner)?;
    Ok(Predicate::of(
        [A::NAME],
        outer.expr().not_in_subquery(subquery),
    ))
}

/// `(select column from … )` as a scalar — one row, one column.
///
/// SQL raises an error at run time if the subquery returns more than one row,
/// so this is for a query that is limited or aggregated.
///
/// # Errors
///
/// As [`exists`].
///
/// ```
/// use moso_orm::expr;
/// # use moso_orm::{Column, Entity, Result, Select};
/// fn newest_id<E: Entity>(id: Column<E, i64>, query: &Select<E>) -> Result<moso_sql::Expr> {
///     expr::scalar(id, &query.clone().limit(1))
/// }
/// ```
pub fn scalar<E: Entity, T: SqlType, J>(
    column: Column<E, T>,
    query: &Select<E, J>,
) -> Result<Expr> {
    Ok(Expr::scalar(query.to_column_subquery(column)?))
}

/// `count(*)`.
///
/// Counts rows rather than non-`NULL` values, which is what
/// [`Select::count`](crate::Select::count) uses and what a `HAVING` almost
/// always wants.
///
/// ```
/// use moso_orm::expr::count_star;
///
/// assert_ne!(count_star(), moso_sql::Expr::value(1_i64));
/// ```
#[must_use]
pub fn count_star() -> Expr {
    Aggregate::count_star().into_expr()
}

/// `coalesce(a, b, …)` — the first argument that is not `NULL`.
///
/// ```
/// use moso_orm::expr::coalesce;
/// use moso_sql::Expr;
///
/// let with_default = coalesce([Expr::null(), Expr::value(0_i64)]);
/// assert_ne!(with_default, Expr::null());
/// ```
#[must_use]
pub fn coalesce(operands: impl IntoIterator<Item = Expr>) -> Expr {
    Function::Coalesce(operands.into_iter().collect()).into_expr()
}

/// `greatest(a, b, …)`.
///
/// ```
/// use moso_orm::expr::greatest;
/// use moso_sql::Expr;
///
/// assert_ne!(greatest([Expr::value(1_i64), Expr::value(2_i64)]), Expr::value(2_i64));
/// ```
#[must_use]
pub fn greatest(operands: impl IntoIterator<Item = Expr>) -> Expr {
    Function::Greatest(operands.into_iter().collect()).into_expr()
}

/// `least(a, b, …)`.
///
/// ```
/// use moso_orm::expr::least;
/// use moso_sql::Expr;
///
/// assert_ne!(least([Expr::value(1_i64), Expr::value(2_i64)]), Expr::value(1_i64));
/// ```
#[must_use]
pub fn least(operands: impl IntoIterator<Item = Expr>) -> Expr {
    Function::Least(operands.into_iter().collect()).into_expr()
}

/// `lower(s)`.
///
/// ```
/// use moso_orm::expr::lower;
/// use moso_sql::Expr;
///
/// assert_ne!(lower(Expr::value("ADA")), Expr::value("ada"));
/// ```
#[must_use]
pub fn lower(operand: Expr) -> Expr {
    Function::Lower(Box::new(operand)).into_expr()
}

/// `upper(s)`.
///
/// ```
/// use moso_orm::expr::upper;
/// use moso_sql::Expr;
///
/// assert_ne!(upper(Expr::value("ada")), Expr::value("ADA"));
/// ```
#[must_use]
pub fn upper(operand: Expr) -> Expr {
    Function::Upper(Box::new(operand)).into_expr()
}

/// `length(s)`.
///
/// ```
/// use moso_orm::expr::length;
/// use moso_sql::Expr;
///
/// assert_ne!(length(Expr::value("ada")), Expr::value(3_i64));
/// ```
#[must_use]
pub fn length(operand: Expr) -> Expr {
    Function::Length(Box::new(operand)).into_expr()
}

/// `trim(s)` — whitespace from both ends.
///
/// ```
/// use moso_orm::expr::trim;
/// use moso_sql::Expr;
///
/// assert_ne!(trim(Expr::value(" ada ")), Expr::value("ada"));
/// ```
#[must_use]
pub fn trim(operand: Expr) -> Expr {
    Function::Trim {
        operand: Box::new(operand),
        mode: TrimMode::Both,
        characters: None,
    }
    .into_expr()
}

/// `now()` — the transaction's start time on PostgreSQL.
///
/// ```
/// use moso_orm::expr::now;
///
/// assert_eq!(now(), now());
/// ```
#[must_use]
pub fn now() -> Expr {
    Function::Now.into_expr()
}

/// A bound parameter.
///
/// Every value in a Moso query is bound, never interpolated: there is no API
/// here that takes a runtime string as SQL *structure* (`04-devex/45`).
///
/// ```
/// use moso_orm::expr::bind;
/// use moso_sql::Expr;
///
/// assert_eq!(bind(7_i64), Expr::value(7_i64));
/// ```
#[must_use]
pub fn bind(value: impl SqlType) -> Expr {
    Expr::bound(value.into_value())
}

/// A raw SQL fragment, as a predicate the scope check will not examine.
///
/// The escape hatch of non-negotiable N8, at expression granularity: when a
/// PostgreSQL operator has no wrapper yet, write it here rather than dropping
/// the whole query to [`RawQuery`](crate::RawQuery).
///
/// `?` is a placeholder and `??` is a literal question mark; the dialect
/// renumbers placeholders to `$1`, `$2`, … Bind one value per `?` with
/// [`raw_with`].
///
/// ```
/// use moso_orm::expr::raw;
///
/// let predicate = raw("age_range @> 30");
/// assert!(predicate.entities().is_empty());
/// ```
#[must_use]
pub fn raw(fragment: impl Into<String>) -> Predicate {
    Predicate::unchecked(RawExpr::new(fragment).into_expr())
}

/// A raw SQL fragment with bound parameters.
///
/// ```
/// use moso_orm::expr::raw_with;
/// use moso_sql::Value;
///
/// let predicate = raw_with("age_range @> ?", [Value::I32(30)]);
/// assert!(predicate.entities().is_empty());
/// ```
#[must_use]
pub fn raw_with(fragment: impl Into<String>, values: impl IntoIterator<Item = Value>) -> Predicate {
    Predicate::unchecked(RawExpr::with_args(fragment, values).into_expr())
}

/// A `CASE` expression, to be filled in with `.when(..)` and `.otherwise(..)`.
///
/// ```
/// use moso_orm::expr::case;
/// use moso_sql::Expr;
///
/// let bucket = case()
///     .when(Expr::value(true), Expr::value("yes"))
///     .otherwise(Expr::value("no"))
///     .into_expr();
/// assert_ne!(bucket, Expr::value("no"));
/// ```
#[must_use]
pub fn case() -> Case {
    Case::new()
}

/// `min(expr)`, for an expression that is not a bare column.
///
/// ```
/// use moso_orm::expr::{lower, min};
/// use moso_sql::Expr;
///
/// assert_ne!(min(lower(Expr::col(moso_sql::Ident::from_static("name")))), Expr::null());
/// ```
#[must_use]
pub fn min(operand: Expr) -> Expr {
    Aggregate::new(AggregateFunc::Min, [operand]).into_expr()
}

/// `max(expr)`, for an expression that is not a bare column.
///
/// ```
/// use moso_orm::expr::{max, upper};
/// use moso_sql::Expr;
///
/// assert_ne!(max(upper(Expr::col(moso_sql::Ident::from_static("name")))), Expr::null());
/// ```
#[must_use]
pub fn max(operand: Expr) -> Expr {
    Aggregate::new(AggregateFunc::Max, [operand]).into_expr()
}

/// `sum(expr)`, for an expression that is not a bare column.
///
/// ```
/// use moso_orm::expr::sum;
/// use moso_sql::Expr;
///
/// assert_ne!(sum(Expr::value(1_i64)), Expr::value(1_i64));
/// ```
#[must_use]
pub fn sum(operand: Expr) -> Expr {
    Aggregate::new(AggregateFunc::Sum, [operand]).into_expr()
}

/// `avg(expr)`, for an expression that is not a bare column.
///
/// ```
/// use moso_orm::expr::avg;
/// use moso_sql::Expr;
///
/// assert_ne!(avg(Expr::value(1_i64)), Expr::value(1_i64));
/// ```
#[must_use]
pub fn avg(operand: Expr) -> Expr {
    Aggregate::new(AggregateFunc::Avg, [operand]).into_expr()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::EntityDescriptor;
    use crate::entity::ColumnDef;
    use crate::row::{DecodeError, Row};
    use moso_sql::{TableRef, ValueKind};
    use std::sync::OnceLock;

    #[derive(Clone, Debug)]
    struct User {
        id: i64,
    }

    impl Entity for User {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("users");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("is_admin", ValueKind::Bool),
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

    #[derive(Clone, Debug)]
    struct Post {
        id: i64,
    }

    impl Entity for Post {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("posts");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("author_id", ValueKind::I64),
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

    const USER_ID: Column<User, i64> = Column::new("id");
    const IS_ADMIN: Column<User, bool> = Column::new("is_admin");
    const AUTHOR_ID: Column<Post, i64> = Column::new("author_id");

    #[test]
    fn exists_wraps_the_whole_query() {
        let admins = Select::<User>::new().filter(IS_ADMIN.eq(true));
        let predicate = exists(&admins).expect("a renderable query");
        assert!(predicate.entities().is_empty());
        assert_ne!(predicate.expr(), &moso_sql::Expr::value(true));

        let none = not_exists(&admins).expect("a renderable query");
        assert_ne!(none.expr(), predicate.expr());
    }

    #[test]
    fn a_subquery_inherits_the_scope_check() {
        // A filter on an unjoined entity fails inside `exists` exactly as it
        // does at the top level — the subquery is not a way around the check.
        let broken = Select::<User>::new().filter(AUTHOR_ID.eq(1));
        let error = exists(&broken).expect_err("`Post` is not joined");
        assert!(error.to_string().contains("`Post` is not joined"));
    }

    #[test]
    fn in_query_mentions_only_the_outer_entity() {
        let admins = Select::<User>::new().filter(IS_ADMIN.eq(true));
        let predicate = in_query(AUTHOR_ID, USER_ID, &admins).expect("renderable");
        assert_eq!(predicate.entities(), ["Post"]);

        let excluded = not_in_query(AUTHOR_ID, USER_ID, &admins).expect("renderable");
        assert_eq!(excluded.entities(), ["Post"]);
        assert_ne!(excluded.expr(), predicate.expr());
    }

    #[test]
    fn a_scalar_subquery_projects_one_column() {
        let newest = Select::<User>::new().limit(1);
        let value = scalar(USER_ID, &newest).expect("renderable");
        assert_ne!(value, Expr::null());
    }

    #[test]
    fn the_scalar_helpers_are_distinct_expressions() {
        let column = Expr::col(moso_sql::Ident::from_static("name"));
        let built = [
            count_star(),
            coalesce([column.clone(), Expr::value("")]),
            greatest([column.clone(), Expr::value("z")]),
            least([column.clone(), Expr::value("a")]),
            lower(column.clone()),
            upper(column.clone()),
            length(column.clone()),
            trim(column.clone()),
            now(),
            min(column.clone()),
            max(column.clone()),
            sum(column.clone()),
            avg(column.clone()),
        ];
        for (index, left) in built.iter().enumerate() {
            for right in &built[index + 1..] {
                assert_ne!(left, right, "two helpers built the same expression");
            }
        }
    }

    #[test]
    fn a_raw_fragment_is_never_scope_checked() {
        assert!(raw("age_range @> 30").entities().is_empty());
        assert!(
            raw_with("age_range @> ?", [Value::I32(30)])
                .entities()
                .is_empty()
        );
    }

    #[test]
    fn bind_and_case_build_what_they_say() {
        assert_eq!(bind(7_i64), Expr::value(7_i64));
        let branch = case()
            .when(Expr::value(true), Expr::value(1_i64))
            .otherwise(Expr::value(0_i64))
            .into_expr();
        assert_ne!(branch, Expr::value(1_i64));
    }
}
