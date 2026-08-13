//! `ORDER BY` terms, including the `NULLS FIRST` / `NULLS LAST` placement that
//! keyset pagination depends on.

use crate::expr::Expr;

/// Ascending or descending.
///
/// ```
/// use moso_sql::Order;
///
/// assert_eq!(Order::Asc.reversed(), Order::Desc);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Order {
    /// Smallest first. The SQL default.
    #[default]
    Asc,
    /// Largest first.
    Desc,
}

impl Order {
    /// The opposite direction.
    ///
    /// Keyset pagination walks a page backwards by reversing every term and
    /// then reversing the rows, so this is on the hot path of `Page::prev`.
    ///
    /// ```
    /// use moso_sql::Order;
    ///
    /// assert_eq!(Order::Desc.reversed(), Order::Asc);
    /// ```
    #[must_use]
    pub const fn reversed(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }
}

/// Where `NULL`s sort.
///
/// PostgreSQL defaults to `NULLS LAST` for `ASC` and `NULLS FIRST` for `DESC`;
/// SQLite sorts `NULL` first in both directions. A query that paginates over a
/// nullable column and does not say which it wants will return different pages
/// on the two backends, so `moso-orm` always says.
///
/// ```
/// use moso_sql::Nulls;
///
/// assert_eq!(Nulls::First.reversed(), Nulls::Last);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Nulls {
    /// `NULL`s sort before every value.
    First,
    /// `NULL`s sort after every value.
    Last,
}

impl Nulls {
    /// The opposite placement.
    ///
    /// ```
    /// assert_eq!(moso_sql::Nulls::Last.reversed(), moso_sql::Nulls::First);
    /// ```
    #[must_use]
    pub const fn reversed(self) -> Self {
        match self {
            Self::First => Self::Last,
            Self::Last => Self::First,
        }
    }
}

/// One term of an `ORDER BY` clause: an expression, a direction, and an
/// optional `NULLS` placement.
///
/// ```
/// use moso_sql::{Expr, Ident, Nulls, Order, OrderTerm};
///
/// let newest_first = OrderTerm::desc(Expr::col(Ident::from_static("created_at")))
///     .nulls_last();
/// assert_eq!(newest_first.order(), Order::Desc);
/// assert_eq!(newest_first.nulls(), Some(Nulls::Last));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct OrderTerm {
    expr: Expr,
    order: Order,
    nulls: Option<Nulls>,
}

impl OrderTerm {
    /// A term with an explicit direction and no `NULLS` placement.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, Order, OrderTerm};
    ///
    /// let term = OrderTerm::new(Expr::col(Ident::from_static("id")), Order::Asc);
    /// assert!(term.nulls().is_none());
    /// ```
    #[must_use]
    pub const fn new(expr: Expr, order: Order) -> Self {
        Self {
            expr,
            order,
            nulls: None,
        }
    }

    /// An ascending term.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, Order, OrderTerm};
    ///
    /// assert_eq!(OrderTerm::asc(Expr::col(Ident::from_static("id"))).order(), Order::Asc);
    /// ```
    #[must_use]
    pub const fn asc(expr: Expr) -> Self {
        Self::new(expr, Order::Asc)
    }

    /// A descending term.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, Order, OrderTerm};
    ///
    /// assert_eq!(OrderTerm::desc(Expr::col(Ident::from_static("id"))).order(), Order::Desc);
    /// ```
    #[must_use]
    pub const fn desc(expr: Expr) -> Self {
        Self::new(expr, Order::Desc)
    }

    /// Sorts `NULL`s before every value.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, Nulls, OrderTerm};
    ///
    /// let term = OrderTerm::asc(Expr::col(Ident::from_static("x"))).nulls_first();
    /// assert_eq!(term.nulls(), Some(Nulls::First));
    /// ```
    #[must_use]
    pub fn nulls_first(mut self) -> Self {
        self.nulls = Some(Nulls::First);
        self
    }

    /// Sorts `NULL`s after every value.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, Nulls, OrderTerm};
    ///
    /// let term = OrderTerm::asc(Expr::col(Ident::from_static("x"))).nulls_last();
    /// assert_eq!(term.nulls(), Some(Nulls::Last));
    /// ```
    #[must_use]
    pub fn nulls_last(mut self) -> Self {
        self.nulls = Some(Nulls::Last);
        self
    }

    /// Sets the `NULLS` placement, or clears it with `None`.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, OrderTerm};
    ///
    /// let term = OrderTerm::asc(Expr::col(Ident::from_static("x"))).with_nulls(None);
    /// assert!(term.nulls().is_none());
    /// ```
    #[must_use]
    pub fn with_nulls(mut self, nulls: Option<Nulls>) -> Self {
        self.nulls = nulls;
        self
    }

    /// The expression being ordered by.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, OrderTerm};
    ///
    /// let expr = Expr::col(Ident::from_static("x"));
    /// assert_eq!(OrderTerm::asc(expr.clone()).expr(), &expr);
    /// ```
    #[must_use]
    pub const fn expr(&self) -> &Expr {
        &self.expr
    }

    /// The direction.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, Order, OrderTerm};
    ///
    /// assert_eq!(OrderTerm::desc(Expr::col(Ident::from_static("x"))).order(), Order::Desc);
    /// ```
    #[must_use]
    pub const fn order(&self) -> Order {
        self.order
    }

    /// The `NULLS` placement, if one was asked for.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, OrderTerm};
    ///
    /// assert!(OrderTerm::asc(Expr::col(Ident::from_static("x"))).nulls().is_none());
    /// ```
    #[must_use]
    pub const fn nulls(&self) -> Option<Nulls> {
        self.nulls
    }

    /// Flips both the direction and the `NULLS` placement.
    ///
    /// Reversing only the direction moves `NULL`s to the other end of the
    /// result, which silently breaks a backwards page; reversing both is the
    /// correct mirror.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, Nulls, Order, OrderTerm};
    ///
    /// let term = OrderTerm::asc(Expr::col(Ident::from_static("x"))).nulls_last().reversed();
    /// assert_eq!(term.order(), Order::Desc);
    /// assert_eq!(term.nulls(), Some(Nulls::First));
    /// ```
    #[must_use]
    pub fn reversed(mut self) -> Self {
        self.order = self.order.reversed();
        self.nulls = self.nulls.map(Nulls::reversed);
        self
    }

    /// Consumes the term and returns its expression.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, OrderTerm};
    ///
    /// let expr = Expr::col(Ident::from_static("x"));
    /// assert_eq!(OrderTerm::asc(expr.clone()).into_expr(), expr);
    /// ```
    #[must_use]
    pub fn into_expr(self) -> Expr {
        self.expr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::Ident;

    #[test]
    fn reversing_a_term_mirrors_the_page() {
        let term = OrderTerm::desc(Expr::col(Ident::from_static("created_at"))).nulls_first();
        let back = term.clone().reversed();
        assert_eq!(back.order(), Order::Asc);
        assert_eq!(back.nulls(), Some(Nulls::Last));
        assert_eq!(back.reversed(), term);
    }
}
