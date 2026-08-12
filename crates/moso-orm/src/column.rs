//! [`Column<E, T>`] — where the type safety lives.
//!
//! ADR-0007 gives up type-level query encoding, so `Select<User>` never becomes
//! a forty-line type. The safety it gives up there is bought back **here**: a
//! column knows its entity and its Rust type, so `User::AGE.gt(18)` compiles
//! and `User::AGE.gt("18")` does not, with a message that names the column and
//! both types.
//!
//! The whole type is a `&'static str` and two `PhantomData`, so a column
//! constant is a pointer and a length, and comparing against one monomorphises
//! nothing new.
//!
//! ```
//! use moso_orm::Column;
//!
//! struct User;
//! const AGE: Column<User, i32> = Column::new("age");
//!
//! assert_eq!(AGE.name(), "age");
//! assert_eq!(core::mem::size_of::<Column<User, i32>>(), core::mem::size_of::<&'static str>());
//! ```

use core::marker::PhantomData;

use moso_sql::{
    Aggregate, AggregateFunc, ColumnRef, Expr, Ident, JsonOp, OrderTerm, TextQuery, Value,
};

use crate::entity::Entity;
use crate::predicate::Predicate;
use crate::sqltype::{JsonLike, Nullable, SqlType, TextLike};

/// One column of one entity, carrying its Rust type.
///
/// Generated as an associated constant by `#[derive(Entity)]`:
/// `User::EMAIL: Column<User, Email>`.
///
/// ```
/// use moso_orm::Column;
///
/// struct Post;
/// const TITLE: Column<Post, String> = Column::new("title");
/// const VIEWS: Column<Post, i64> = Column::new("views");
///
/// // Both are ordinary constants, and both are one word wide.
/// assert_eq!(TITLE.name(), "title");
/// assert_eq!(VIEWS.name(), "views");
/// ```
pub struct Column<E, T> {
    name: &'static str,
    entity: PhantomData<fn() -> E>,
    value: PhantomData<fn() -> T>,
}

impl<E, T> Column<E, T> {
    /// A column with this SQL name.
    ///
    /// ```
    /// use moso_orm::Column;
    ///
    /// struct User;
    /// const ID: Column<User, i64> = Column::new("id");
    /// assert_eq!(ID.name(), "id");
    /// ```
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            entity: PhantomData,
            value: PhantomData,
        }
    }

    /// The SQL column name.
    ///
    /// ```
    /// # use moso_orm::Column;
    /// # struct User;
    /// assert_eq!(Column::<User, i64>::new("id").name(), "id");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The column name as a validated identifier.
    ///
    /// # Panics
    ///
    /// If the name is not a valid SQL identifier — checked at compile time in
    /// generated code, since the constant is `const`.
    ///
    /// ```
    /// # use moso_orm::Column;
    /// # struct User;
    /// assert_eq!(Column::<User, i64>::new("id").ident().as_str(), "id");
    /// ```
    #[must_use]
    pub const fn ident(&self) -> Ident {
        Ident::from_static(self.name)
    }
}

impl<E: Entity, T> Column<E, T> {
    /// The column, qualified by the entity's table.
    ///
    /// Qualification is what lets the build-time scope check say *which*
    /// entity a filter reached for.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn qualified<E: Entity>(column: Column<E, i64>) -> String {
    ///     column.column_ref().to_string()
    /// }
    /// ```
    #[must_use]
    pub fn column_ref(&self) -> ColumnRef {
        ColumnRef::qualified(E::TABLE.name().clone(), self.ident())
    }

    /// The column as an expression.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn as_expr<E: Entity>(column: Column<E, i64>) -> moso_sql::Expr {
    ///     column.expr()
    /// }
    /// ```
    #[must_use]
    pub fn expr(&self) -> Expr {
        Expr::column(self.column_ref())
    }

    /// The entity this column belongs to, by name.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn owner<E: Entity>(column: Column<E, i64>) -> &'static str {
    ///     column.entity_name()
    /// }
    /// ```
    #[must_use]
    pub fn entity_name(&self) -> &'static str {
        E::NAME
    }

    /// Sorts by this column, ascending.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn oldest_first<E: Entity>(column: Column<E, i64>) -> moso_sql::OrderTerm {
    ///     column.asc()
    /// }
    /// ```
    #[must_use]
    pub fn asc(&self) -> OrderTerm {
        OrderTerm::asc(self.expr())
    }

    /// Sorts by this column, descending.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn newest_first<E: Entity>(column: Column<E, i64>) -> moso_sql::OrderTerm {
    ///     column.desc()
    /// }
    /// ```
    #[must_use]
    pub fn desc(&self) -> OrderTerm {
        OrderTerm::desc(self.expr())
    }

    /// `count(column)`, for a projection or a `having`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn how_many<E: Entity>(column: Column<E, i64>) -> moso_sql::Expr {
    ///     column.count()
    /// }
    /// ```
    #[must_use]
    pub fn count(&self) -> Expr {
        Aggregate::new(AggregateFunc::Count, [self.expr()]).into_expr()
    }

    /// `count(distinct column)`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn distinct<E: Entity>(column: Column<E, i64>) -> moso_sql::Expr {
    ///     column.count_distinct()
    /// }
    /// ```
    #[must_use]
    pub fn count_distinct(&self) -> Expr {
        Aggregate::new(AggregateFunc::Count, [self.expr()])
            .distinct()
            .into_expr()
    }

    /// `min(column)`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn smallest<E: Entity>(column: Column<E, i64>) -> moso_sql::Expr {
    ///     column.min()
    /// }
    /// ```
    #[must_use]
    pub fn min(&self) -> Expr {
        Aggregate::new(AggregateFunc::Min, [self.expr()]).into_expr()
    }

    /// `max(column)`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn largest<E: Entity>(column: Column<E, i64>) -> moso_sql::Expr {
    ///     column.max()
    /// }
    /// ```
    #[must_use]
    pub fn max(&self) -> Expr {
        Aggregate::new(AggregateFunc::Max, [self.expr()]).into_expr()
    }

    /// `sum(column)`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn total<E: Entity>(column: Column<E, i64>) -> moso_sql::Expr {
    ///     column.sum()
    /// }
    /// ```
    #[must_use]
    pub fn sum(&self) -> Expr {
        Aggregate::new(AggregateFunc::Sum, [self.expr()]).into_expr()
    }

    /// `avg(column)`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn mean<E: Entity>(column: Column<E, i64>) -> moso_sql::Expr {
    ///     column.avg()
    /// }
    /// ```
    #[must_use]
    pub fn avg(&self) -> Expr {
        Aggregate::new(AggregateFunc::Avg, [self.expr()]).into_expr()
    }
}

impl<E: Entity, T: SqlType> Column<E, T> {
    /// `column = value`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn named<E: Entity>(column: Column<E, String>) -> moso_orm::Predicate {
    ///     column.eq("Ada")
    /// }
    /// ```
    #[must_use]
    pub fn eq(&self, value: impl ColumnValue<T>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr().eq(Expr::bound(value.into_column_value()))
        })
    }

    /// `column <> value`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn not_named<E: Entity>(column: Column<E, String>) -> moso_orm::Predicate {
    ///     column.ne("Ada")
    /// }
    /// ```
    #[must_use]
    pub fn ne(&self, value: impl ColumnValue<T>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr().ne(Expr::bound(value.into_column_value()))
        })
    }

    /// `column < value`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn cheap<E: Entity>(column: Column<E, i64>) -> moso_orm::Predicate {
    ///     column.lt(1000)
    /// }
    /// ```
    #[must_use]
    pub fn lt(&self, value: impl ColumnValue<T>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr().lt(Expr::bound(value.into_column_value()))
        })
    }

    /// `column <= value`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn affordable<E: Entity>(column: Column<E, i64>) -> moso_orm::Predicate {
    ///     column.le(1000)
    /// }
    /// ```
    #[must_use]
    pub fn le(&self, value: impl ColumnValue<T>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr().le(Expr::bound(value.into_column_value()))
        })
    }

    /// `column > value`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn in_stock<E: Entity>(column: Column<E, i64>) -> moso_orm::Predicate {
    ///     column.gt(0)
    /// }
    /// ```
    #[must_use]
    pub fn gt(&self, value: impl ColumnValue<T>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr().gt(Expr::bound(value.into_column_value()))
        })
    }

    /// `column >= value`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn adult<E: Entity>(column: Column<E, i32>) -> moso_orm::Predicate {
    ///     column.ge(18)
    /// }
    /// ```
    #[must_use]
    pub fn ge(&self, value: impl ColumnValue<T>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr().ge(Expr::bound(value.into_column_value()))
        })
    }

    /// `column between low and high`, inclusive at both ends.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn mid_priced<E: Entity>(column: Column<E, i64>) -> moso_orm::Predicate {
    ///     column.between(1000, 5000)
    /// }
    /// ```
    #[must_use]
    pub fn between(&self, low: impl ColumnValue<T>, high: impl ColumnValue<T>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr().between(
                Expr::bound(low.into_column_value()),
                Expr::bound(high.into_column_value()),
            )
        })
    }

    /// `column not between low and high`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn extreme<E: Entity>(column: Column<E, i64>) -> moso_orm::Predicate {
    ///     column.not_between(1000, 5000)
    /// }
    /// ```
    #[must_use]
    pub fn not_between(&self, low: impl ColumnValue<T>, high: impl ColumnValue<T>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr().not_between(
                Expr::bound(low.into_column_value()),
                Expr::bound(high.into_column_value()),
            )
        })
    }

    /// `column in (…)`.
    ///
    /// An empty list becomes a constant-false predicate rather than invalid
    /// SQL, which is what makes `is_in(user_supplied_ids)` safe to write.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn one_of<E: Entity>(column: Column<E, i64>) -> moso_orm::Predicate {
    ///     column.is_in([1, 2, 3])
    /// }
    /// ```
    #[must_use]
    pub fn is_in(&self, values: impl IntoIterator<Item = impl ColumnValue<T>>) -> Predicate {
        Predicate::of([E::NAME], {
            let items: Vec<Expr> = values
                .into_iter()
                .map(|value| Expr::bound(value.into_column_value()))
                .collect();
            if items.is_empty() {
                return Predicate::of([E::NAME], Expr::value(false));
            }
            self.expr().in_list(items)
        })
    }

    /// `column not in (…)`.
    ///
    /// An empty list becomes a constant-true predicate, which is the mirror of
    /// [`Column::is_in`] and equally the mathematically right answer.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn none_of<E: Entity>(column: Column<E, i64>) -> moso_orm::Predicate {
    ///     column.not_in([1, 2])
    /// }
    /// ```
    #[must_use]
    pub fn not_in(&self, values: impl IntoIterator<Item = impl ColumnValue<T>>) -> Predicate {
        Predicate::of([E::NAME], {
            let items: Vec<Expr> = values
                .into_iter()
                .map(|value| Expr::bound(value.into_column_value()))
                .collect();
            if items.is_empty() {
                return Predicate::of([E::NAME], Expr::value(true));
            }
            self.expr().not_in_list(items)
        })
    }

    /// `column = other`, where both columns have the same Rust type.
    ///
    /// The shared `T` is the point: comparing a `Column<Post, Id<User>>` with a
    /// `Column<User, Id<User>>` compiles, and comparing it with a
    /// `Column<Tag, Id<Tag>>` does not.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn same<A: Entity, B: Entity>(left: Column<A, i64>, right: Column<B, i64>) -> moso_orm::Predicate {
    ///     left.eq_col(right)
    /// }
    /// ```
    #[must_use]
    pub fn eq_col<F: Entity>(&self, other: Column<F, T>) -> Predicate {
        Predicate::of([E::NAME, F::NAME], self.expr().eq(other.expr()))
    }

    /// `column <> other`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn differ<A: Entity, B: Entity>(a: Column<A, i64>, b: Column<B, i64>) -> moso_orm::Predicate {
    ///     a.ne_col(b)
    /// }
    /// ```
    #[must_use]
    pub fn ne_col<F: Entity>(&self, other: Column<F, T>) -> Predicate {
        Predicate::of([E::NAME, F::NAME], self.expr().ne(other.expr()))
    }

    /// `column is distinct from value` — `NULL`-aware inequality.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn changed<E: Entity>(column: Column<E, String>) -> moso_orm::Predicate {
    ///     column.is_distinct_from("old")
    /// }
    /// ```
    #[must_use]
    pub fn is_distinct_from(&self, value: impl ColumnValue<T>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr()
                .is_distinct_from(Expr::bound(value.into_column_value()))
        })
    }
}

impl<E: Entity, T: Nullable> Column<E, T> {
    /// `column is null`.
    ///
    /// Exists only on `Option<..>` columns, so asking whether a `NOT NULL`
    /// column is null is a missing method rather than a query that is always
    /// false.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn live<E: Entity>(column: Column<E, Option<String>>) -> moso_orm::Predicate {
    ///     column.is_null()
    /// }
    /// ```
    #[must_use]
    pub fn is_null(&self) -> Predicate {
        Predicate::of([E::NAME], self.expr().is_null())
    }

    /// `column is not null`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn deleted<E: Entity>(column: Column<E, Option<String>>) -> moso_orm::Predicate {
    ///     column.is_not_null()
    /// }
    /// ```
    #[must_use]
    pub fn is_not_null(&self) -> Predicate {
        Predicate::of([E::NAME], self.expr().is_not_null())
    }
}

impl<E: Entity, T: TextLike> Column<E, T> {
    /// `column like pattern`, with `%` and `_` as the caller wrote them.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn starts<E: Entity>(column: Column<E, String>) -> moso_orm::Predicate {
    ///     column.like("a%")
    /// }
    /// ```
    #[must_use]
    pub fn like(&self, pattern: impl AsRef<str>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr().like(Expr::value(pattern.as_ref()))
        })
    }

    /// `column not like pattern`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn avoids<E: Entity>(column: Column<E, String>) -> moso_orm::Predicate {
    ///     column.not_like("%spam%")
    /// }
    /// ```
    #[must_use]
    pub fn not_like(&self, pattern: impl AsRef<str>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr().not_like(Expr::value(pattern.as_ref()))
        })
    }

    /// `column ilike pattern` — case-insensitive.
    ///
    /// SQLite has no `ILIKE`; there this becomes `LIKE`, which is already
    /// case-insensitive for ASCII. The divergence is documented rather than
    /// hidden.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn search<E: Entity>(column: Column<E, String>, q: &str) -> moso_orm::Predicate {
    ///     column.ilike(format!("%{q}%"))
    /// }
    /// ```
    #[must_use]
    pub fn ilike(&self, pattern: impl AsRef<str>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr().ilike(Expr::value(pattern.as_ref()))
        })
    }

    /// `column like 'prefix%'`, escaping the pattern metacharacters in
    /// `prefix`.
    ///
    /// This is the one people want and the one they get wrong: a user typing
    /// `100%` must not match everything.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn by_prefix<E: Entity>(column: Column<E, String>) -> moso_orm::Predicate {
    ///     column.starts_with("100%")
    /// }
    /// ```
    #[must_use]
    pub fn starts_with(&self, prefix: impl AsRef<str>) -> Predicate {
        Predicate::of([E::NAME], {
            self.like_escaped(&format!("{}%", escape_like(prefix.as_ref())))
        })
    }

    /// `column like '%suffix'`, escaping the pattern metacharacters.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn by_domain<E: Entity>(column: Column<E, String>) -> moso_orm::Predicate {
    ///     column.ends_with("@acme.com")
    /// }
    /// ```
    #[must_use]
    pub fn ends_with(&self, suffix: impl AsRef<str>) -> Predicate {
        Predicate::of([E::NAME], {
            self.like_escaped(&format!("%{}", escape_like(suffix.as_ref())))
        })
    }

    /// `column like '%needle%'`, escaping the pattern metacharacters.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn containing<E: Entity>(column: Column<E, String>) -> moso_orm::Predicate {
    ///     column.contains("50%")
    /// }
    /// ```
    #[must_use]
    pub fn contains(&self, needle: impl AsRef<str>) -> Predicate {
        Predicate::of([E::NAME], {
            self.like_escaped(&format!("%{}%", escape_like(needle.as_ref())))
        })
    }

    /// `column ilike '%needle%'`, escaping the pattern metacharacters.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// fn loosely<E: Entity>(column: Column<E, String>) -> moso_orm::Predicate {
    ///     column.icontains("ada")
    /// }
    /// ```
    #[must_use]
    pub fn icontains(&self, needle: impl AsRef<str>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr()
                .ilike(Expr::value(format!("%{}%", escape_like(needle.as_ref()))))
                .escape(LIKE_ESCAPE)
        })
    }

    /// `to_tsvector(column) @@ query` — full-text search.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// # use moso_sql::TextQuery;
    /// fn find<E: Entity>(column: Column<E, String>, q: &str) -> moso_orm::Predicate {
    ///     column.matches(TextQuery::Websearch(q.to_owned()))
    /// }
    /// ```
    #[must_use]
    pub fn matches(&self, query: TextQuery) -> Predicate {
        Predicate::of([E::NAME], Expr::text_match(self.expr(), query, None))
    }

    /// `to_tsvector(config, column) @@ query`, with a named text-search
    /// configuration.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity};
    /// # use moso_sql::{Ident, TextQuery};
    /// fn find_fr<E: Entity>(column: Column<E, String>, q: &str) -> moso_orm::Predicate {
    ///     column.matches_in(TextQuery::Websearch(q.to_owned()), Ident::from_static("french"))
    /// }
    /// ```
    #[must_use]
    pub fn matches_in(&self, query: TextQuery, config: Ident) -> Predicate {
        Predicate::of([E::NAME], {
            Expr::text_match(self.expr(), query, Some(config))
        })
    }

    /// Shared body for the escaping pattern helpers.
    fn like_escaped(&self, pattern: &str) -> Expr {
        self.expr().like(Expr::value(pattern)).escape(LIKE_ESCAPE)
    }
}

impl<E: Entity, T: JsonLike> Column<E, T> {
    /// `column -> key` — the value at a top-level key, as JSON.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Json};
    /// fn theme<E: Entity>(column: Column<E, Json<serde_json::Value>>) -> moso_sql::Expr {
    ///     column.get("theme")
    /// }
    /// ```
    #[must_use]
    pub fn get(&self, key: impl AsRef<str>) -> Expr {
        self.expr().json(JsonOp::Get, Expr::value(key.as_ref()))
    }

    /// `column ->> key` — the value at a top-level key, as text.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Json};
    /// fn theme_text<E: Entity>(column: Column<E, Json<serde_json::Value>>) -> moso_sql::Expr {
    ///     column.get_text("theme")
    /// }
    /// ```
    #[must_use]
    pub fn get_text(&self, key: impl AsRef<str>) -> Expr {
        self.expr().json(JsonOp::GetText, Expr::value(key.as_ref()))
    }

    /// `column #>> '{a,b}'` — the value at a path, as text.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Json};
    /// fn nested<E: Entity>(column: Column<E, Json<serde_json::Value>>) -> moso_sql::Expr {
    ///     column.path(["notifications", "email"])
    /// }
    /// ```
    #[must_use]
    pub fn path(&self, segments: impl IntoIterator<Item = impl AsRef<str>>) -> Expr {
        let path = Expr::array(
            segments
                .into_iter()
                .map(|segment| Expr::value(segment.as_ref())),
        );
        self.expr().json(JsonOp::GetPathText, path)
    }

    /// `column ? key` — does the document have this top-level key?
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Json};
    /// fn has_theme<E: Entity>(column: Column<E, Json<serde_json::Value>>) -> moso_orm::Predicate {
    ///     column.has_key("theme")
    /// }
    /// ```
    #[must_use]
    pub fn has_key(&self, key: impl AsRef<str>) -> Predicate {
        Predicate::of([E::NAME], {
            self.expr().json(JsonOp::HasKey, Expr::value(key.as_ref()))
        })
    }

    /// `column @> value` — does the document contain this one?
    ///
    /// The argument is JSON *text*; serialising is the caller's job, which is
    /// what keeps `serde_json::Value` out of the query builder's signatures.
    ///
    /// # Errors
    ///
    /// [`moso_sql::ValueError`] if `json` is not valid JSON.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Json};
    /// fn dark<E: Entity>(column: Column<E, Json<serde_json::Value>>) -> moso_orm::Predicate {
    ///     column.contains_json(r#"{"theme":"dark"}"#).expect("a JSON literal")
    /// }
    /// ```
    pub fn contains_json(&self, json: &str) -> Result<Predicate, moso_sql::ValueError> {
        let value = Value::json(json)?;
        Ok(Predicate::of(
            [E::NAME],
            self.expr().json(JsonOp::Contains, Expr::bound(value)),
        ))
    }
}

/// The escape character the pattern helpers use.
///
/// Backslash is PostgreSQL's default and SQLite's convention, and stating it
/// explicitly with `ESCAPE` makes the statement independent of
/// `standard_conforming_strings`.
const LIKE_ESCAPE: char = '\\';

/// Escapes `%`, `_` and the escape character itself in a `LIKE` pattern.
///
/// ```
/// # // private, documented for the reader of `starts_with`
/// ```
fn escape_like(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for character in raw.chars() {
        if matches!(character, '%' | '_' | LIKE_ESCAPE) {
            escaped.push(LIKE_ESCAPE);
        }
        escaped.push(character);
    }
    escaped
}

impl<E, T> Clone for Column<E, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E, T> Copy for Column<E, T> {}

impl<E, T> PartialEq for Column<E, T> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl<E, T> Eq for Column<E, T> {}

impl<E, T> core::fmt::Debug for Column<E, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Column({})", self.name)
    }
}

impl<E: Entity, T> core::fmt::Display for Column<E, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}", E::TABLE.name(), self.name)
    }
}

/// A value that can be compared against a `T` column.
///
/// This trait exists **for its error message**. Every comparison on
/// [`Column<E, T>`] takes `impl ColumnValue<T>`, so passing the wrong type
/// produces a hand-written diagnostic that names the column's type and the
/// value's, instead of a generic `Into` failure.
///
/// Implemented for `T` itself, for `&T` where `T: Clone`, for `&str` against a
/// `String` column, and for `T` against an `Option<T>` column — so a nullable
/// column still takes a plain value.
///
/// ```
/// use moso_orm::ColumnValue;
/// use moso_sql::Value;
///
/// assert_eq!(ColumnValue::<String>::into_column_value("Ada"), Value::text("Ada"));
/// assert_eq!(ColumnValue::<Option<i64>>::into_column_value(7_i64), Value::I64(7));
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be compared with a `{T}` column",
    label = "expected a `{T}`",
    note = "the column's type comes from the entity's field, so this comparison could never \
            match a row",
    note = "help: pass a `{T}`, or convert first — `value.parse::<{T}>()?`, `value.into()`, \
            `value.to_string()`",
    note = "help: to compare two columns instead, use `.eq_col(Other::COLUMN)`"
)]
pub trait ColumnValue<T> {
    /// Binds the value as a statement parameter.
    ///
    /// ```
    /// use moso_orm::ColumnValue;
    /// use moso_sql::Value;
    ///
    /// assert_eq!(ColumnValue::<i64>::into_column_value(1_i64), Value::I64(1));
    /// ```
    fn into_column_value(self) -> Value;
}

#[diagnostic::do_not_recommend]
impl<T: SqlType> ColumnValue<T> for T {
    fn into_column_value(self) -> Value {
        self.into_value()
    }
}

#[diagnostic::do_not_recommend]
impl<T: SqlType + Clone> ColumnValue<T> for &T {
    fn into_column_value(self) -> Value {
        self.to_value()
    }
}

impl ColumnValue<String> for &str {
    fn into_column_value(self) -> Value {
        Value::text(self)
    }
}

impl ColumnValue<Option<String>> for &str {
    fn into_column_value(self) -> Value {
        Value::text(self)
    }
}

#[diagnostic::do_not_recommend]
impl<T: SqlType> ColumnValue<Option<T>> for T {
    fn into_column_value(self) -> Value {
        self.into_value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::EntityDescriptor;
    use crate::entity::ColumnDef;
    use crate::row::Row;
    use moso_sql::{TableRef, ValueKind};
    use std::sync::OnceLock;

    /// A user, reduced to what a column test needs.
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
            ColumnDef::new("bio", ValueKind::Text).nullable(),
        ];
        const NAME: &'static str = "User";

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> Result<Self, crate::DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| EntityDescriptor::builder("User", Self::TABLE).build())
        }
    }

    const ID: Column<User, i64> = Column::new("id");
    const EMAIL: Column<User, String> = Column::new("email");
    const BIO: Column<User, Option<String>> = Column::new("bio");

    #[test]
    fn a_column_is_one_word_wide() {
        assert_eq!(
            core::mem::size_of::<Column<User, i64>>(),
            core::mem::size_of::<&'static str>()
        );
    }

    #[test]
    fn a_predicate_remembers_the_entity_it_touches() {
        assert_eq!(ID.eq(1).entities(), ["User"]);
        assert_eq!(BIO.is_null().entities(), ["User"]);
        let joined = ID.eq(1) & EMAIL.like("a%");
        assert_eq!(joined.entities(), ["User"]);
    }

    #[test]
    fn a_column_qualifies_itself_with_its_table() {
        assert_eq!(ID.column_ref().to_string(), "users.id");
        assert_eq!(ID.to_string(), "users.id");
    }

    #[test]
    fn a_string_column_takes_a_str_a_string_and_a_reference() {
        let by_str = EMAIL.eq("ada@example.com");
        let by_string = EMAIL.eq(String::from("ada@example.com"));
        let owned = String::from("ada@example.com");
        let by_reference = EMAIL.eq(&owned);
        assert_eq!(by_str, by_string);
        assert_eq!(by_str, by_reference);
    }

    #[test]
    fn a_nullable_column_takes_a_plain_value() {
        let _ = BIO.eq("hello");
        let _ = BIO.eq(String::from("hello"));
        let _ = BIO.is_null();
        let _ = BIO.is_not_null();
    }

    #[test]
    fn an_empty_in_list_is_false_not_broken_sql() {
        let never = ID.is_in(Vec::<i64>::new());
        assert_eq!(never.expr(), &Expr::value(false));
        let always = ID.not_in(Vec::<i64>::new());
        assert_eq!(always.expr(), &Expr::value(true));
    }

    #[test]
    fn pattern_metacharacters_in_user_input_are_escaped() {
        assert_eq!(escape_like("100%"), r"100\%");
        assert_eq!(escape_like("a_b"), r"a\_b");
        assert_eq!(escape_like(r"back\slash"), r"back\\slash");
        // …and `like` itself passes the pattern through untouched, because
        // there the caller wrote the metacharacters on purpose.
        let raw = EMAIL.like("a%");
        let escaped = EMAIL.starts_with("a%");
        assert_ne!(raw, escaped);
    }

    #[test]
    fn ordering_terms_come_off_the_column() {
        assert_eq!(ID.asc(), OrderTerm::asc(ID.expr()));
        assert_eq!(ID.desc(), OrderTerm::desc(ID.expr()));
    }

    #[test]
    fn aggregates_come_off_the_column() {
        for aggregate in [ID.count(), ID.min(), ID.max(), ID.sum(), ID.avg()] {
            assert_ne!(aggregate, ID.expr());
        }
        assert_eq!(EMAIL.eq("x").entities(), ["User"]);
        assert_ne!(ID.count(), ID.count_distinct());
    }

    #[test]
    fn comparing_two_columns_shares_the_rust_type() {
        let other: Column<User, i64> = Column::new("manager_id");
        let _ = ID.eq_col(other);
        let _ = ID.ne_col(other);
    }
}
