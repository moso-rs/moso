//! [`Predicate`] — an expression that remembers which entities it mentions.
//!
//! # This is the joined-set decision
//!
//! `docs/adr/README.md` records an open question: should `Select<E, J>` carry
//! the joined-entity set **in the type**, so that filtering on an unjoined
//! entity's column is a compile error, or should it be checked at runtime?
//!
//! **Decided: at build time, with a diagnostic-quality message.** `J` is kept
//! as a type parameter and used for the one obligation that has no runtime
//! equivalent worth having — the tenant scope, whose failure mode is a silent
//! cross-tenant read rather than a loud SQL error.
//!
//! The reasoning, in the order it mattered:
//!
//! 1. **A type-level joined set breaks conditional joins.** `if wants_tags {
//!    query = query.join(Post::TAGS); }` cannot type-check when `.join()`
//!    changes the type, and there is no clean way around it. Non-negotiable N4
//!    — ergonomic dynamic queries — outranks a compile-time check for an error
//!    that always fails loudly.
//! 2. **It would infect the expression API.** Tracking the set in the type
//!    means `Column::eq` returns a type parameterised by the entity, and every
//!    combinator, every helper function and every stored predicate a user
//!    writes grows a parameter. That is the "forty lines of generic type
//!    vomit" ADR-0007 exists to prevent, arriving through a side door.
//! 3. **The failure is not silent.** An unjoined column is a query that can
//!    never return a row. It fails on the first execution, in the first test.
//!    Compare the tenant case, where forgetting the scope returns *other
//!    people's rows* — which is why that one is worth the type complexity, and
//!    the design documents say so themselves.
//! 4. **`Select<Post, (User, Tag, Comment)>` violates the 80-character rule**
//!    in `41-diagnostics.md` for any realistic module path.
//!
//! So the check is precise rather than type-level: a [`Predicate`] records the
//! entities whose columns went into it, [`Select::filter`](crate::Select::filter)
//! captures the caller's line with `#[track_caller]`, and building the
//! statement compares the two. Nothing is sent to the server.
//!
//! ```
//! use moso_orm::{Predicate, Unjoined};
//! use moso_sql::{Expr, Ident};
//!
//! let mentions_user = Predicate::of(["User"], Expr::col(Ident::from_static("is_admin")));
//! assert_eq!(mentions_user.entities(), ["User"]);
//!
//! // Combining two predicates unions their entity sets, so the check stays
//! // exact through `&`, `|` and `!`.
//! let mentions_both = mentions_user & Predicate::of(["Post"], Expr::value(true));
//! assert_eq!(mentions_both.entities(), ["User", "Post"]);
//! ```

use core::ops::{BitAnd, BitOr, Not};

use moso_sql::Expr;

/// A boolean expression, plus the entities whose columns are inside it.
///
/// Returned by every comparison on [`Column`](crate::Column). Combining two
/// predicates unions their entity sets, so the set stays exact however the
/// expression is assembled.
///
/// A bare [`Expr`] converts in with an **empty** set, which means "do not
/// check": that is the honest answer for a raw fragment, whose contents Moso
/// cannot see.
///
/// ```
/// use moso_orm::Predicate;
/// use moso_sql::Expr;
///
/// let unchecked: Predicate = Expr::value(true).into();
/// assert!(unchecked.entities().is_empty());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Predicate {
    expr: Expr,
    entities: Vec<&'static str>,
}

impl Predicate {
    /// A predicate that mentions `entities`.
    ///
    /// ```
    /// use moso_orm::Predicate;
    /// use moso_sql::Expr;
    ///
    /// let p = Predicate::of(["User"], Expr::value(true));
    /// assert_eq!(p.entities(), ["User"]);
    /// ```
    #[must_use]
    pub fn of(entities: impl IntoIterator<Item = &'static str>, expr: Expr) -> Self {
        let mut predicate = Self {
            expr,
            entities: Vec::new(),
        };
        for entity in entities {
            predicate.add_entity(entity);
        }
        predicate
    }

    /// A predicate whose entity set is unknown, so it is not checked.
    ///
    /// ```
    /// use moso_orm::Predicate;
    /// use moso_sql::Expr;
    ///
    /// assert!(Predicate::unchecked(Expr::value(true)).entities().is_empty());
    /// ```
    #[must_use]
    pub const fn unchecked(expr: Expr) -> Self {
        Self {
            expr,
            entities: Vec::new(),
        }
    }

    /// The entities whose columns appear in the expression, in first-seen
    /// order and without duplicates.
    ///
    /// ```
    /// use moso_orm::Predicate;
    /// use moso_sql::Expr;
    ///
    /// let p = Predicate::of(["User", "User"], Expr::value(true));
    /// assert_eq!(p.entities(), ["User"]);
    /// ```
    #[must_use]
    pub fn entities(&self) -> &[&'static str] {
        &self.entities
    }

    /// The expression.
    ///
    /// ```
    /// use moso_orm::Predicate;
    /// use moso_sql::Expr;
    ///
    /// assert_eq!(Predicate::unchecked(Expr::value(true)).expr(), &Expr::value(true));
    /// ```
    #[must_use]
    pub const fn expr(&self) -> &Expr {
        &self.expr
    }

    /// The expression, consuming the predicate.
    ///
    /// ```
    /// use moso_orm::Predicate;
    /// use moso_sql::Expr;
    ///
    /// assert_eq!(Predicate::unchecked(Expr::value(true)).into_expr(), Expr::value(true));
    /// ```
    #[must_use]
    pub fn into_expr(self) -> Expr {
        self.expr
    }

    /// Whether every entity this predicate mentions is in `scope`.
    ///
    /// This is the check, in one line. `Select` calls it once per filter when
    /// it builds the statement.
    ///
    /// ```
    /// use moso_orm::Predicate;
    /// use moso_sql::Expr;
    ///
    /// let p = Predicate::of(["User"], Expr::value(true));
    /// assert!(p.is_in_scope(&["Post", "User"]));
    /// assert!(!p.is_in_scope(&["Post"]));
    /// ```
    #[must_use]
    pub fn is_in_scope(&self, scope: &[&'static str]) -> bool {
        self.missing_from(scope).is_none()
    }

    /// The first entity this predicate mentions that is not in `scope`.
    ///
    /// ```
    /// use moso_orm::Predicate;
    /// use moso_sql::Expr;
    ///
    /// let p = Predicate::of(["User"], Expr::value(true));
    /// assert_eq!(p.missing_from(&["Post"]), Some("User"));
    /// assert_eq!(p.missing_from(&["Post", "User"]), None);
    /// ```
    #[must_use]
    pub fn missing_from(&self, scope: &[&'static str]) -> Option<&'static str> {
        self.entities
            .iter()
            .find(|entity| !scope.contains(entity))
            .copied()
    }

    /// `self AND other`, unioning the entity sets.
    ///
    /// ```
    /// use moso_orm::Predicate;
    /// use moso_sql::Expr;
    ///
    /// let both = Predicate::of(["A"], Expr::value(true))
    ///     .and(Predicate::of(["B"], Expr::value(false)));
    /// assert_eq!(both.entities(), ["A", "B"]);
    /// ```
    #[must_use]
    pub fn and(mut self, other: Self) -> Self {
        self.expr = self.expr.and(other.expr);
        for entity in other.entities {
            self.add_entity(entity);
        }
        self
    }

    /// `self OR other`, unioning the entity sets.
    ///
    /// ```
    /// use moso_orm::Predicate;
    /// use moso_sql::Expr;
    ///
    /// let either = Predicate::of(["A"], Expr::value(true))
    ///     .or(Predicate::of(["B"], Expr::value(false)));
    /// assert_eq!(either.entities().len(), 2);
    /// ```
    #[must_use]
    pub fn or(mut self, other: Self) -> Self {
        self.expr = self.expr.or(other.expr);
        for entity in other.entities {
            self.add_entity(entity);
        }
        self
    }

    /// `NOT self`, keeping the entity set.
    ///
    /// ```
    /// use moso_orm::Predicate;
    /// use moso_sql::Expr;
    ///
    /// let negated = Predicate::of(["A"], Expr::value(true)).negate();
    /// assert_eq!(negated.entities(), ["A"]);
    /// ```
    #[must_use]
    pub fn negate(mut self) -> Self {
        self.expr = self.expr.negate();
        self
    }

    /// Adds an entity to the set, keeping it duplicate-free.
    fn add_entity(&mut self, entity: &'static str) {
        if !self.entities.contains(&entity) {
            self.entities.push(entity);
        }
    }
}

impl From<Expr> for Predicate {
    /// With an empty entity set: a raw expression's contents are not visible,
    /// so the scope check declines to guess.
    fn from(expr: Expr) -> Self {
        Self::unchecked(expr)
    }
}

impl From<Predicate> for Expr {
    fn from(predicate: Predicate) -> Self {
        predicate.expr
    }
}

impl BitAnd for Predicate {
    type Output = Self;

    fn bitand(self, other: Self) -> Self {
        self.and(other)
    }
}

impl BitOr for Predicate {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        self.or(other)
    }
}

impl Not for Predicate {
    type Output = Self;

    fn not(self) -> Self {
        self.negate()
    }
}

/// `a AND b AND …`, or `None` for an empty iterator.
///
/// ```
/// use moso_orm::{Predicate, all};
/// use moso_sql::Expr;
///
/// let combined = all([
///     Predicate::of(["A"], Expr::value(true)),
///     Predicate::of(["B"], Expr::value(false)),
/// ]);
/// assert_eq!(combined.expect("two predicates").entities().len(), 2);
/// assert!(all(Vec::new()).is_none());
/// ```
#[must_use]
pub fn all(predicates: impl IntoIterator<Item = Predicate>) -> Option<Predicate> {
    predicates.into_iter().reduce(Predicate::and)
}

/// `a OR b OR …`, or `None` for an empty iterator.
///
/// ```
/// use moso_orm::{Predicate, any};
/// use moso_sql::Expr;
///
/// let combined = any([Predicate::of(["A"], Expr::value(true))]);
/// assert!(combined.is_some());
/// assert!(any(Vec::new()).is_none());
/// ```
#[must_use]
pub fn any(predicates: impl IntoIterator<Item = Predicate>) -> Option<Predicate> {
    predicates.into_iter().reduce(Predicate::or)
}

/// `NOT predicate`.
///
/// ```
/// use moso_orm::{Predicate, not};
/// use moso_sql::Expr;
///
/// assert_eq!(not(Predicate::of(["A"], Expr::value(true))).entities(), ["A"]);
/// ```
#[must_use]
pub fn not(predicate: Predicate) -> Predicate {
    predicate.negate()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(entity: &'static str) -> Predicate {
        Predicate::of([entity], Expr::value(true))
    }

    #[test]
    fn combining_unions_the_entity_sets() {
        let combined = p("User") & p("Post") | p("Tag");
        assert_eq!(combined.entities(), ["User", "Post", "Tag"]);
    }

    #[test]
    fn the_set_is_duplicate_free_and_ordered() {
        let combined = p("User") & p("User") & p("Post");
        assert_eq!(combined.entities(), ["User", "Post"]);
    }

    #[test]
    fn negation_keeps_the_set() {
        assert_eq!((!p("User")).entities(), ["User"]);
        assert_eq!(not(p("User")).entities(), ["User"]);
    }

    #[test]
    fn a_raw_expression_is_not_checked() {
        let raw: Predicate = Expr::value(true).into();
        assert!(raw.entities().is_empty());
        // …which means it is in scope everywhere, deliberately.
        assert!(raw.is_in_scope(&[]));
    }

    #[test]
    fn the_check_names_the_first_missing_entity() {
        let combined = p("User") & p("Tag");
        assert_eq!(combined.missing_from(&["Post"]), Some("User"));
        assert_eq!(combined.missing_from(&["Post", "User"]), Some("Tag"));
        assert_eq!(combined.missing_from(&["Post", "User", "Tag"]), None);
    }

    #[test]
    fn all_and_any_fold_or_answer_none() {
        assert!(all(Vec::new()).is_none());
        assert!(any(Vec::new()).is_none());
        assert_eq!(all([p("A"), p("B")]).expect("two").entities().len(), 2);
        assert_eq!(any([p("A"), p("B")]).expect("two").entities().len(), 2);
    }
}
