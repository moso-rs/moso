//! The expression tree: everything that can appear in a `WHERE`, a `SELECT`
//! list, a `SET`, an `ORDER BY`, a `HAVING`, or a `CHECK` constraint.
//!
//! [`Expr`] is `#[non_exhaustive]` and is documented as opaque. Build it with
//! the constructors and combinators on `Expr` rather than by naming variants:
//! new variants arrive in minor releases, and `moso-orm`'s `Column<E, T>` is
//! the type-checked front door that most code should use instead.

use core::ops::{Add, BitAnd, BitOr, Div, Mul, Not, Rem, Sub};

use crate::ident::{ColumnRef, Ident};
use crate::order::OrderTerm;
use crate::select::Select;
use crate::types::DataType;
use crate::value::{Bindable, Value, ValueKind};

/// A SQL expression.
///
/// # Treat this as opaque
///
/// The variants are public because `moso-orm` and the dialects live in other
/// modules and have to build and walk the tree, not because matching on them
/// is a supported way to use the crate. The enum and several of its payloads
/// grow in minor releases.
///
/// ```
/// use moso_sql::{Expr, Ident};
///
/// let adults = Expr::col(Ident::from_static("age")).ge(Expr::value(18));
/// let named = Expr::col(Ident::from_static("name")).is_not_null();
/// let both = adults & named;
/// assert!(matches!(both, Expr::Binary { .. }));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Expr {
    /// A bound parameter.
    Value(Value),
    /// A column reference.
    Column(ColumnRef),
    /// A parenthesised list: `(a, b)`, the left-hand side of a row comparison
    /// and the shape keyset pagination compares against.
    Tuple(Vec<Expr>),
    /// An array constructor: `ARRAY[a, b]`.
    Array(Vec<Expr>),
    /// Explicit parentheses. Dialects add their own where precedence requires
    /// it; this is for expressions the caller wants grouped regardless.
    Nested(Box<Expr>),
    /// An infix operator applied to two operands.
    Binary {
        /// The left operand.
        lhs: Box<Expr>,
        /// The operator.
        op: BinOp,
        /// The right operand.
        rhs: Box<Expr>,
    },
    /// A prefix operator applied to one operand.
    Unary {
        /// The operator.
        op: UnOp,
        /// The operand.
        operand: Box<Expr>,
    },
    /// `IS NULL` / `IS NOT NULL`.
    IsNull {
        /// The operand.
        operand: Box<Expr>,
        /// `true` for `IS NOT NULL`.
        negated: bool,
    },
    /// `BETWEEN` / `NOT BETWEEN`, inclusive at both ends.
    Between {
        /// The operand.
        operand: Box<Expr>,
        /// The lower bound, inclusive.
        low: Box<Expr>,
        /// The upper bound, inclusive.
        high: Box<Expr>,
        /// `true` for `NOT BETWEEN`.
        negated: bool,
    },
    /// `LIKE`, `NOT LIKE`, `ILIKE`, `NOT ILIKE`.
    Like {
        /// The operand.
        operand: Box<Expr>,
        /// The pattern. Always an expression, so it is always bound rather
        /// than interpolated.
        pattern: Box<Expr>,
        /// `true` for `ILIKE`. SQLite has no `ILIKE`; its dialect lowers both
        /// sides instead, which is documented divergence, not silence.
        case_insensitive: bool,
        /// `true` for the negated form.
        negated: bool,
        /// An `ESCAPE` character, for patterns that contain a literal `%`.
        escape: Option<char>,
    },
    /// `IN (a, b, c)` / `NOT IN (…)`.
    InList {
        /// The operand.
        operand: Box<Expr>,
        /// The candidate values. An empty list is rendered as a constant
        /// `false` (or `true` when negated) rather than as invalid SQL.
        items: Vec<Expr>,
        /// `true` for `NOT IN`.
        negated: bool,
    },
    /// `IN (SELECT …)` / `NOT IN (SELECT …)`.
    InSubquery {
        /// The operand.
        operand: Box<Expr>,
        /// The subquery.
        query: Box<Select>,
        /// `true` for `NOT IN`.
        negated: bool,
    },
    /// `x = ANY(…)` / `x > ALL(…)`.
    Quantified {
        /// The left operand.
        lhs: Box<Expr>,
        /// The comparison operator.
        op: BinOp,
        /// `ANY` or `ALL`.
        quantifier: Quantifier,
        /// An array expression or a subquery.
        rhs: Box<Expr>,
    },
    /// `EXISTS (SELECT …)` / `NOT EXISTS (…)`.
    Exists {
        /// The subquery.
        query: Box<Select>,
        /// `true` for `NOT EXISTS`.
        negated: bool,
    },
    /// A scalar subquery used as a value: `(SELECT count(*) FROM …)`.
    Scalar(Box<Select>),
    /// `CASE … WHEN … THEN … ELSE … END`.
    Case(Box<Case>),
    /// `CAST(expr AS type)`.
    Cast {
        /// The operand.
        operand: Box<Expr>,
        /// The target type.
        data_type: DataType,
    },
    /// A scalar function call.
    Function(Function),
    /// An aggregate, optionally with `DISTINCT`, `FILTER` and `ORDER BY`.
    Aggregate(Box<Aggregate>),
    /// A window function: `f(…) OVER (…)`.
    Window(Box<WindowExpr>),
    /// A JSON operator: `->`, `->>`, `@>`, `?`, and friends.
    Json {
        /// The document.
        lhs: Box<Expr>,
        /// The operator.
        op: JsonOp,
        /// The key, path or operand.
        rhs: Box<Expr>,
    },
    /// A raw SQL fragment with bound parameters — the expression-level half of
    /// non-negotiable N8.
    Raw(RawExpr),
    /// The `DEFAULT` keyword, valid only in an `INSERT` value list or an
    /// `UPDATE ... SET`.
    Default,
}

impl Expr {
    /// Binds a value as a parameter.
    ///
    /// ```
    /// use moso_sql::{Expr, Value};
    ///
    /// assert_eq!(Expr::value(42_i32), Expr::Value(Value::I32(42)));
    /// ```
    #[must_use]
    pub fn value(value: impl Bindable) -> Self {
        Self::Value(value.into_value())
    }

    /// Wraps an already-built [`Value`].
    ///
    /// ```
    /// use moso_sql::{Expr, Value};
    ///
    /// assert_eq!(Expr::bound(Value::Bool(true)), Expr::Value(Value::Bool(true)));
    /// ```
    #[must_use]
    pub const fn bound(value: Value) -> Self {
        Self::Value(value)
    }

    /// An unqualified column reference.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident};
    ///
    /// let e = Expr::col(Ident::from_static("email"));
    /// assert!(matches!(e, Expr::Column(_)));
    /// ```
    #[must_use]
    pub const fn col(name: Ident) -> Self {
        Self::Column(ColumnRef::new(name))
    }

    /// A column reference, possibly qualified by a table name or alias.
    ///
    /// ```
    /// use moso_sql::{ColumnRef, Expr, Ident};
    ///
    /// let e = Expr::column(ColumnRef::qualified(Ident::from_static("u"), Ident::from_static("id")));
    /// assert!(matches!(e, Expr::Column(_)));
    /// ```
    #[must_use]
    pub const fn column(column: ColumnRef) -> Self {
        Self::Column(column)
    }

    /// The `excluded` pseudo-row's version of a column, for the `SET` list of
    /// an `ON CONFLICT DO UPDATE`.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident};
    ///
    /// assert!(matches!(Expr::excluded(Ident::from_static("name")), Expr::Column(_)));
    /// ```
    #[must_use]
    pub const fn excluded(name: Ident) -> Self {
        Self::Column(ColumnRef::excluded(name))
    }

    /// A row constructor: `(a, b, c)`.
    ///
    /// ```
    /// use moso_sql::Expr;
    ///
    /// let row = Expr::tuple([Expr::value(1), Expr::value(2)]);
    /// assert!(matches!(row, Expr::Tuple(_)));
    /// ```
    #[must_use]
    pub fn tuple(items: impl IntoIterator<Item = Expr>) -> Self {
        Self::Tuple(items.into_iter().collect())
    }

    /// An array constructor: `ARRAY[a, b, c]`.
    ///
    /// ```
    /// use moso_sql::Expr;
    ///
    /// assert!(matches!(Expr::array([Expr::value(1)]), Expr::Array(_)));
    /// ```
    #[must_use]
    pub fn array(items: impl IntoIterator<Item = Expr>) -> Self {
        Self::Array(items.into_iter().collect())
    }

    /// A `NULL` of unknown type.
    ///
    /// ```
    /// use moso_sql::{Expr, Value, ValueKind};
    ///
    /// assert_eq!(Expr::null(), Expr::Value(Value::Null(ValueKind::Unknown)));
    /// ```
    #[must_use]
    pub const fn null() -> Self {
        Self::Value(Value::Null(ValueKind::Unknown))
    }

    /// A raw SQL fragment.
    ///
    /// ```
    /// use moso_sql::{Expr, RawExpr};
    ///
    /// let e = Expr::raw(RawExpr::new("now() - interval '1 day'"));
    /// assert!(matches!(e, Expr::Raw(_)));
    /// ```
    #[must_use]
    pub const fn raw(raw: RawExpr) -> Self {
        Self::Raw(raw)
    }

    /// Wraps the expression in explicit parentheses.
    ///
    /// ```
    /// use moso_sql::Expr;
    ///
    /// assert!(matches!(Expr::value(1).nested(), Expr::Nested(_)));
    /// ```
    #[must_use]
    pub fn nested(self) -> Self {
        Self::Nested(Box::new(self))
    }

    /// Applies an infix operator.
    ///
    /// ```
    /// use moso_sql::{BinOp, Expr, Ident};
    ///
    /// let e = Expr::col(Ident::from_static("a")).binary(BinOp::Add, Expr::value(1));
    /// assert!(matches!(e, Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn binary(self, op: BinOp, rhs: Expr) -> Self {
        Self::Binary {
            lhs: Box::new(self),
            op,
            rhs: Box::new(rhs),
        }
    }

    /// Applies a prefix operator.
    ///
    /// ```
    /// use moso_sql::{Expr, UnOp};
    ///
    /// assert!(matches!(Expr::value(1).unary(UnOp::Neg), Expr::Unary { .. }));
    /// ```
    #[must_use]
    pub fn unary(self, op: UnOp) -> Self {
        Self::Unary {
            op,
            operand: Box::new(self),
        }
    }

    /// `self = rhs`.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident};
    ///
    /// let e = Expr::col(Ident::from_static("id")).eq(Expr::value(1));
    /// assert!(matches!(e, Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn eq(self, rhs: Expr) -> Self {
        self.binary(BinOp::Eq, rhs)
    }

    /// `self <> rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("id")).ne(Expr::value(1));
    /// assert!(matches!(e, Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn ne(self, rhs: Expr) -> Self {
        self.binary(BinOp::NotEq, rhs)
    }

    /// `self < rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(matches!(Expr::col(Ident::from_static("n")).lt(Expr::value(1)), Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn lt(self, rhs: Expr) -> Self {
        self.binary(BinOp::Lt, rhs)
    }

    /// `self <= rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(matches!(Expr::col(Ident::from_static("n")).le(Expr::value(1)), Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn le(self, rhs: Expr) -> Self {
        self.binary(BinOp::LtEq, rhs)
    }

    /// `self > rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(matches!(Expr::col(Ident::from_static("n")).gt(Expr::value(1)), Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn gt(self, rhs: Expr) -> Self {
        self.binary(BinOp::Gt, rhs)
    }

    /// `self >= rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(matches!(Expr::col(Ident::from_static("n")).ge(Expr::value(1)), Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn ge(self, rhs: Expr) -> Self {
        self.binary(BinOp::GtEq, rhs)
    }

    /// `self IS DISTINCT FROM rhs` — equality that treats `NULL` as a value.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("a")).is_distinct_from(Expr::null());
    /// assert!(matches!(e, Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn is_distinct_from(self, rhs: Expr) -> Self {
        self.binary(BinOp::IsDistinctFrom, rhs)
    }

    /// `self IS NOT DISTINCT FROM rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("a")).is_not_distinct_from(Expr::null());
    /// assert!(matches!(e, Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn is_not_distinct_from(self, rhs: Expr) -> Self {
        self.binary(BinOp::IsNotDistinctFrom, rhs)
    }

    /// `self + rhs`.
    ///
    /// Spelled out rather than only as the `+` operator so that a chain built
    /// by a macro reads the same as one written by hand; both exist, and
    /// [`Add`] is implemented too.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// // `login_count = login_count + 1`, the atomic-increment idiom.
    /// let bump = Expr::col(Ident::from_static("login_count")).plus(Expr::value(1));
    /// assert!(matches!(bump, Expr::Binary { .. }));
    /// assert_eq!(bump, Expr::col(Ident::from_static("login_count")) + Expr::value(1));
    /// ```
    #[must_use]
    pub fn plus(self, rhs: Expr) -> Self {
        self.binary(BinOp::Add, rhs)
    }

    /// `self - rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(matches!(Expr::col(Ident::from_static("n")).minus(Expr::value(1)), Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn minus(self, rhs: Expr) -> Self {
        self.binary(BinOp::Sub, rhs)
    }

    /// `self * rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(matches!(Expr::col(Ident::from_static("n")).times(Expr::value(2)), Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn times(self, rhs: Expr) -> Self {
        self.binary(BinOp::Mul, rhs)
    }

    /// `self / rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(matches!(Expr::col(Ident::from_static("n")).over(Expr::value(2)), Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn over(self, rhs: Expr) -> Self {
        self.binary(BinOp::Div, rhs)
    }

    /// `self % rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(matches!(Expr::col(Ident::from_static("n")).modulo(Expr::value(2)), Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn modulo(self, rhs: Expr) -> Self {
        self.binary(BinOp::Mod, rhs)
    }

    /// String concatenation, `self || rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("first")).concat(Expr::value(" "));
    /// assert!(matches!(e, Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn concat(self, rhs: Expr) -> Self {
        self.binary(BinOp::Concat, rhs)
    }

    /// `self AND rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("a")).is_null()
    ///     .and(Expr::col(Ident::from_static("b")).is_null());
    /// assert!(matches!(e, Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn and(self, rhs: Expr) -> Self {
        self.binary(BinOp::And, rhs)
    }

    /// `self OR rhs`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("a")).is_null()
    ///     .or(Expr::col(Ident::from_static("b")).is_null());
    /// assert!(matches!(e, Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn or(self, rhs: Expr) -> Self {
        self.binary(BinOp::Or, rhs)
    }

    /// `NOT self`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(matches!(Expr::col(Ident::from_static("ok")).negate(), Expr::Unary { .. }));
    /// ```
    #[must_use]
    pub fn negate(self) -> Self {
        self.unary(UnOp::Not)
    }

    /// `self IS NULL`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(matches!(Expr::col(Ident::from_static("x")).is_null(), Expr::IsNull { .. }));
    /// ```
    #[must_use]
    pub fn is_null(self) -> Self {
        Self::IsNull {
            operand: Box::new(self),
            negated: false,
        }
    }

    /// `self IS NOT NULL`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(matches!(Expr::col(Ident::from_static("x")).is_not_null(), Expr::IsNull { .. }));
    /// ```
    #[must_use]
    pub fn is_not_null(self) -> Self {
        Self::IsNull {
            operand: Box::new(self),
            negated: true,
        }
    }

    /// `self BETWEEN low AND high`, inclusive at both ends.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("n")).between(Expr::value(1), Expr::value(9));
    /// assert!(matches!(e, Expr::Between { .. }));
    /// ```
    #[must_use]
    pub fn between(self, low: Expr, high: Expr) -> Self {
        Self::Between {
            operand: Box::new(self),
            low: Box::new(low),
            high: Box::new(high),
            negated: false,
        }
    }

    /// `self NOT BETWEEN low AND high`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("n")).not_between(Expr::value(1), Expr::value(9));
    /// assert!(matches!(e, Expr::Between { .. }));
    /// ```
    #[must_use]
    pub fn not_between(self, low: Expr, high: Expr) -> Self {
        Self::Between {
            operand: Box::new(self),
            low: Box::new(low),
            high: Box::new(high),
            negated: true,
        }
    }

    /// `self IN (…)`.
    ///
    /// An empty list renders as a constant `false` rather than as `IN ()`,
    /// which is a syntax error in every dialect.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("id")).in_list([Expr::value(1), Expr::value(2)]);
    /// assert!(matches!(e, Expr::InList { .. }));
    /// ```
    #[must_use]
    pub fn in_list(self, items: impl IntoIterator<Item = Expr>) -> Self {
        Self::InList {
            operand: Box::new(self),
            items: items.into_iter().collect(),
            negated: false,
        }
    }

    /// `self NOT IN (…)`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("id")).not_in_list([Expr::value(1)]);
    /// assert!(matches!(e, Expr::InList { .. }));
    /// ```
    #[must_use]
    pub fn not_in_list(self, items: impl IntoIterator<Item = Expr>) -> Self {
        Self::InList {
            operand: Box::new(self),
            items: items.into_iter().collect(),
            negated: true,
        }
    }

    /// `self IN (SELECT …)`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, Select, TableRef};
    /// let ids = Select::from_table(TableRef::from_static("bans")).select_column(
    ///     moso_sql::ColumnRef::from_static("user_id"),
    /// );
    /// let e = Expr::col(Ident::from_static("id")).in_subquery(ids);
    /// assert!(matches!(e, Expr::InSubquery { .. }));
    /// ```
    #[must_use]
    pub fn in_subquery(self, query: Select) -> Self {
        Self::InSubquery {
            operand: Box::new(self),
            query: Box::new(query),
            negated: false,
        }
    }

    /// `self NOT IN (SELECT …)`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, Select, TableRef};
    /// let e = Expr::col(Ident::from_static("id"))
    ///     .not_in_subquery(Select::from_table(TableRef::from_static("bans")));
    /// assert!(matches!(e, Expr::InSubquery { .. }));
    /// ```
    #[must_use]
    pub fn not_in_subquery(self, query: Select) -> Self {
        Self::InSubquery {
            operand: Box::new(self),
            query: Box::new(query),
            negated: true,
        }
    }

    /// `self <op> ANY(rhs)`, where `rhs` is an array expression or a subquery.
    ///
    /// This is the form that survives a long `IN` list: one array parameter
    /// instead of a placeholder per element, so the statement text stays the
    /// same and the plan stays cached.
    ///
    /// ```
    /// # use moso_sql::{Array, BinOp, Expr, Ident};
    /// let e = Expr::col(Ident::from_static("id"))
    ///     .any(BinOp::Eq, Expr::value(Array::of([1_i64, 2, 3])));
    /// assert!(matches!(e, Expr::Quantified { .. }));
    /// ```
    #[must_use]
    pub fn any(self, op: BinOp, rhs: Expr) -> Self {
        Self::Quantified {
            lhs: Box::new(self),
            op,
            quantifier: Quantifier::Any,
            rhs: Box::new(rhs),
        }
    }

    /// `self <op> ALL(rhs)`.
    ///
    /// ```
    /// # use moso_sql::{Array, BinOp, Expr, Ident};
    /// let e = Expr::col(Ident::from_static("n"))
    ///     .all(BinOp::Gt, Expr::value(Array::of([1_i64])));
    /// assert!(matches!(e, Expr::Quantified { .. }));
    /// ```
    #[must_use]
    pub fn all(self, op: BinOp, rhs: Expr) -> Self {
        Self::Quantified {
            lhs: Box::new(self),
            op,
            quantifier: Quantifier::All,
            rhs: Box::new(rhs),
        }
    }

    /// `self LIKE pattern`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("name")).like(Expr::value("a%"));
    /// assert!(matches!(e, Expr::Like { .. }));
    /// ```
    #[must_use]
    pub fn like(self, pattern: Expr) -> Self {
        Self::Like {
            operand: Box::new(self),
            pattern: Box::new(pattern),
            case_insensitive: false,
            negated: false,
            escape: None,
        }
    }

    /// `self NOT LIKE pattern`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("name")).not_like(Expr::value("a%"));
    /// assert!(matches!(e, Expr::Like { .. }));
    /// ```
    #[must_use]
    pub fn not_like(self, pattern: Expr) -> Self {
        Self::Like {
            operand: Box::new(self),
            pattern: Box::new(pattern),
            case_insensitive: false,
            negated: true,
            escape: None,
        }
    }

    /// `self ILIKE pattern` — case-insensitive on PostgreSQL, lowered on both
    /// sides on SQLite.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("name")).ilike(Expr::value("a%"));
    /// assert!(matches!(e, Expr::Like { case_insensitive: true, .. }));
    /// ```
    #[must_use]
    pub fn ilike(self, pattern: Expr) -> Self {
        Self::Like {
            operand: Box::new(self),
            pattern: Box::new(pattern),
            case_insensitive: true,
            negated: false,
            escape: None,
        }
    }

    /// `self NOT ILIKE pattern`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("name")).not_ilike(Expr::value("a%"));
    /// assert!(matches!(e, Expr::Like { .. }));
    /// ```
    #[must_use]
    pub fn not_ilike(self, pattern: Expr) -> Self {
        Self::Like {
            operand: Box::new(self),
            pattern: Box::new(pattern),
            case_insensitive: true,
            negated: true,
            escape: None,
        }
    }

    /// Sets the `ESCAPE` character of a `LIKE`, for patterns that need to
    /// match a literal `%` or `_`.
    ///
    /// Has no effect on an expression that is not a `LIKE`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("path"))
    ///     .like(Expr::value(r"100\%"))
    ///     .escape('\\');
    /// assert!(matches!(e, Expr::Like { escape: Some('\\'), .. }));
    /// ```
    #[must_use]
    pub fn escape(mut self, character: char) -> Self {
        if let Self::Like { escape, .. } = &mut self {
            *escape = Some(character);
        }
        self
    }

    /// `CAST(self AS data_type)`.
    ///
    /// ```
    /// # use moso_sql::{DataType, Expr, Ident};
    /// let e = Expr::col(Ident::from_static("n")).cast(DataType::Text);
    /// assert!(matches!(e, Expr::Cast { .. }));
    /// ```
    #[must_use]
    pub fn cast(self, data_type: DataType) -> Self {
        Self::Cast {
            operand: Box::new(self),
            data_type,
        }
    }

    /// A JSON operator applied to this expression.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, JsonOp};
    /// let e = Expr::col(Ident::from_static("prefs")).json(JsonOp::GetText, Expr::value("theme"));
    /// assert!(matches!(e, Expr::Json { .. }));
    /// ```
    #[must_use]
    pub fn json(self, op: JsonOp, rhs: Expr) -> Self {
        Self::Json {
            lhs: Box::new(self),
            op,
            rhs: Box::new(rhs),
        }
    }

    /// `EXISTS (SELECT …)`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Select, TableRef};
    /// let e = Expr::exists(Select::from_table(TableRef::from_static("posts")));
    /// assert!(matches!(e, Expr::Exists { negated: false, .. }));
    /// ```
    #[must_use]
    pub fn exists(query: Select) -> Self {
        Self::Exists {
            query: Box::new(query),
            negated: false,
        }
    }

    /// `NOT EXISTS (SELECT …)`.
    ///
    /// ```
    /// # use moso_sql::{Expr, Select, TableRef};
    /// let e = Expr::not_exists(Select::from_table(TableRef::from_static("posts")));
    /// assert!(matches!(e, Expr::Exists { negated: true, .. }));
    /// ```
    #[must_use]
    pub fn not_exists(query: Select) -> Self {
        Self::Exists {
            query: Box::new(query),
            negated: true,
        }
    }

    /// A scalar subquery used as a value.
    ///
    /// ```
    /// # use moso_sql::{Expr, Select, TableRef};
    /// let e = Expr::scalar(Select::from_table(TableRef::from_static("posts")));
    /// assert!(matches!(e, Expr::Scalar(_)));
    /// ```
    #[must_use]
    pub fn scalar(query: Select) -> Self {
        Self::Scalar(Box::new(query))
    }

    /// `to_tsvector(document) @@ <query>` — a full-text match.
    ///
    /// The query text is always a bound parameter, never spliced into the
    /// statement, so a user's search box cannot become syntax.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, TextQuery};
    /// let e = Expr::text_match(
    ///     Expr::col(Ident::from_static("search")),
    ///     TextQuery::Websearch("rust orm".into()),
    ///     None,
    /// );
    /// assert!(matches!(e, Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn text_match(document: Expr, query: TextQuery, config: Option<Ident>) -> Self {
        let vector = Self::Function(Function::ToTsVector {
            config: config.clone(),
            document: Box::new(document),
        });
        vector.binary(
            BinOp::TextMatch,
            Self::Function(Function::ToTsQuery { config, query }),
        )
    }

    /// `<tsvector column> @@ <query>` — a full-text match against a column
    /// that already holds a materialised `tsvector`, which is the shape a
    /// generated-column plus GIN index wants.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, TextQuery};
    /// let e = Expr::text_match_vector(
    ///     Expr::col(Ident::from_static("search")),
    ///     TextQuery::Websearch("rust orm".into()),
    ///     None,
    /// );
    /// assert!(matches!(e, Expr::Binary { .. }));
    /// ```
    #[must_use]
    pub fn text_match_vector(vector: Expr, query: TextQuery, config: Option<Ident>) -> Self {
        vector.binary(
            BinOp::TextMatch,
            Self::Function(Function::ToTsQuery { config, query }),
        )
    }

    /// `AND`s a sequence of expressions, returning `None` for an empty one.
    ///
    /// This is the shape a dynamic filter list wants: `filter_opt` pushes into
    /// a `Vec<Expr>` and the builder folds it once.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(Expr::all_of(Vec::new()).is_none());
    /// let both = Expr::all_of([
    ///     Expr::col(Ident::from_static("a")).is_null(),
    ///     Expr::col(Ident::from_static("b")).is_null(),
    /// ]);
    /// assert!(matches!(both, Some(Expr::Binary { .. })));
    /// ```
    #[must_use]
    pub fn all_of(exprs: impl IntoIterator<Item = Expr>) -> Option<Self> {
        exprs.into_iter().reduce(Expr::and)
    }

    /// `OR`s a sequence of expressions, returning `None` for an empty one.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// assert!(Expr::any_of(Vec::new()).is_none());
    /// assert!(Expr::any_of([Expr::col(Ident::from_static("a")).is_null()]).is_some());
    /// ```
    #[must_use]
    pub fn any_of(exprs: impl IntoIterator<Item = Expr>) -> Option<Self> {
        exprs.into_iter().reduce(Expr::or)
    }

    /// The column this expression refers to, if it is a bare column
    /// reference.
    ///
    /// Used by `moso-orm` to recognise the `ORDER BY` terms that a keyset
    /// cursor can encode.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident};
    /// let e = Expr::col(Ident::from_static("id"));
    /// assert_eq!(e.as_column().map(|c| c.name().as_str()), Some("id"));
    /// assert!(Expr::value(1).as_column().is_none());
    /// ```
    #[must_use]
    pub const fn as_column(&self) -> Option<&ColumnRef> {
        match self {
            Self::Column(column) => Some(column),
            _ => None,
        }
    }
}

impl BitAnd for Expr {
    type Output = Expr;

    fn bitand(self, rhs: Expr) -> Expr {
        self.and(rhs)
    }
}

impl Add for Expr {
    type Output = Expr;

    fn add(self, rhs: Expr) -> Expr {
        self.plus(rhs)
    }
}

impl Sub for Expr {
    type Output = Expr;

    fn sub(self, rhs: Expr) -> Expr {
        self.minus(rhs)
    }
}

impl Mul for Expr {
    type Output = Expr;

    fn mul(self, rhs: Expr) -> Expr {
        self.times(rhs)
    }
}

impl Div for Expr {
    type Output = Expr;

    fn div(self, rhs: Expr) -> Expr {
        self.over(rhs)
    }
}

impl Rem for Expr {
    type Output = Expr;

    fn rem(self, rhs: Expr) -> Expr {
        self.modulo(rhs)
    }
}

impl BitOr for Expr {
    type Output = Expr;

    fn bitor(self, rhs: Expr) -> Expr {
        self.or(rhs)
    }
}

impl Not for Expr {
    type Output = Expr;

    fn not(self) -> Expr {
        self.negate()
    }
}

impl From<Value> for Expr {
    fn from(value: Value) -> Self {
        Expr::Value(value)
    }
}

impl From<ColumnRef> for Expr {
    fn from(column: ColumnRef) -> Self {
        Expr::Column(column)
    }
}

impl From<RawExpr> for Expr {
    fn from(raw: RawExpr) -> Self {
        Expr::Raw(raw)
    }
}

/// An infix operator.
///
/// ```
/// use moso_sql::BinOp;
///
/// assert_eq!(BinOp::Eq.negated(), Some(BinOp::NotEq));
/// assert_eq!(BinOp::Add.negated(), None);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BinOp {
    /// `=`.
    Eq,
    /// `<>`.
    NotEq,
    /// `<`.
    Lt,
    /// `<=`.
    LtEq,
    /// `>`.
    Gt,
    /// `>=`.
    GtEq,
    /// `IS DISTINCT FROM` — `NULL`-aware inequality.
    IsDistinctFrom,
    /// `IS NOT DISTINCT FROM` — `NULL`-aware equality.
    IsNotDistinctFrom,
    /// `AND`.
    And,
    /// `OR`.
    Or,
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
    /// `%`.
    Mod,
    /// `^` — exponentiation on PostgreSQL.
    Exp,
    /// `||` — string and array concatenation.
    Concat,
    /// `&` — bitwise and.
    BitAnd,
    /// `|` — bitwise or.
    BitOr,
    /// `#` on PostgreSQL — bitwise exclusive or.
    BitXor,
    /// `<<`.
    ShiftLeft,
    /// `>>`.
    ShiftRight,
    /// `~` — POSIX regular-expression match. PostgreSQL only.
    Regex,
    /// `~*` — case-insensitive regular-expression match. PostgreSQL only.
    RegexCaseInsensitive,
    /// `!~`.
    NotRegex,
    /// `!~*`.
    NotRegexCaseInsensitive,
    /// `@@` — full-text match.
    TextMatch,
    /// `@>` — array containment.
    ArrayContains,
    /// `<@` — array containment, reversed.
    ArrayContainedBy,
    /// `&&` — arrays overlap.
    ArrayOverlaps,
}

impl BinOp {
    /// The operator that means the opposite, for the comparison operators that
    /// have one.
    ///
    /// ```
    /// use moso_sql::BinOp;
    ///
    /// assert_eq!(BinOp::Lt.negated(), Some(BinOp::GtEq));
    /// assert_eq!(BinOp::Concat.negated(), None);
    /// ```
    #[must_use]
    pub const fn negated(self) -> Option<Self> {
        Some(match self {
            Self::Eq => Self::NotEq,
            Self::NotEq => Self::Eq,
            Self::Lt => Self::GtEq,
            Self::LtEq => Self::Gt,
            Self::Gt => Self::LtEq,
            Self::GtEq => Self::Lt,
            Self::IsDistinctFrom => Self::IsNotDistinctFrom,
            Self::IsNotDistinctFrom => Self::IsDistinctFrom,
            Self::Regex => Self::NotRegex,
            Self::NotRegex => Self::Regex,
            Self::RegexCaseInsensitive => Self::NotRegexCaseInsensitive,
            Self::NotRegexCaseInsensitive => Self::RegexCaseInsensitive,
            _ => return None,
        })
    }

    /// Whether the operator yields a boolean and may therefore be the whole of
    /// a `WHERE` clause.
    ///
    /// ```
    /// use moso_sql::BinOp;
    ///
    /// assert!(BinOp::Eq.is_predicate());
    /// assert!(!BinOp::Add.is_predicate());
    /// ```
    #[must_use]
    pub const fn is_predicate(self) -> bool {
        matches!(
            self,
            Self::Eq
                | Self::NotEq
                | Self::Lt
                | Self::LtEq
                | Self::Gt
                | Self::GtEq
                | Self::IsDistinctFrom
                | Self::IsNotDistinctFrom
                | Self::And
                | Self::Or
                | Self::Regex
                | Self::RegexCaseInsensitive
                | Self::NotRegex
                | Self::NotRegexCaseInsensitive
                | Self::TextMatch
                | Self::ArrayContains
                | Self::ArrayContainedBy
                | Self::ArrayOverlaps
        )
    }
}

/// A prefix operator.
///
/// ```
/// use moso_sql::UnOp;
///
/// assert_eq!(UnOp::Not, UnOp::Not);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UnOp {
    /// `NOT`.
    Not,
    /// Unary `-`.
    Neg,
    /// `~` — bitwise complement.
    BitNot,
}

/// `ANY` or `ALL`, the quantifier of a quantified comparison.
///
/// ```
/// use moso_sql::Quantifier;
///
/// assert_ne!(Quantifier::Any, Quantifier::All);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Quantifier {
    /// True if the comparison holds for at least one element.
    Any,
    /// True if the comparison holds for every element.
    All,
}

/// A `jsonb` operator.
///
/// SQLite has none of these natively; its dialect lowers the ones it can to
/// `json_extract` and reports [`Error::Unsupported`](crate::Error::Unsupported)
/// for the rest rather than producing SQL that means something different.
///
/// ```
/// use moso_sql::JsonOp;
///
/// assert_ne!(JsonOp::Get, JsonOp::GetText);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JsonOp {
    /// `->` — get a field or element, as JSON.
    Get,
    /// `->>` — get a field or element, as text.
    GetText,
    /// `#>` — get at a path, as JSON.
    GetPath,
    /// `#>>` — get at a path, as text.
    GetPathText,
    /// `@>` — does the left document contain the right one?
    Contains,
    /// `<@` — is the left document contained in the right one?
    ContainedBy,
    /// `?` — does the document have this top-level key?
    HasKey,
    /// `?|` — does it have any of these keys?
    HasAnyKey,
    /// `?&` — does it have all of these keys?
    HasAllKeys,
    /// `||` — concatenate two documents.
    Concat,
    /// `-` — delete a key or element.
    Remove,
    /// `#-` — delete at a path.
    RemovePath,
}

/// A full-text query, as the user typed it.
///
/// Every variant's payload is bound as a parameter and parsed by the server,
/// never concatenated into the statement.
///
/// ```
/// use moso_sql::TextQuery;
///
/// // The forgiving parser: `"exact phrase" -excluded or this`.
/// let q = TextQuery::Websearch("rust -python".into());
/// assert_eq!(q.text(), "rust -python");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextQuery {
    /// `plainto_tsquery` — every word must appear.
    Plain(String),
    /// `phraseto_tsquery` — the words must appear in order.
    Phrase(String),
    /// `websearch_to_tsquery` — the forgiving search-box syntax. The right
    /// default for user input, because it cannot fail to parse.
    Websearch(String),
    /// `to_tsquery` — the strict operator syntax. Rejects malformed input with
    /// a database error, so only pass text your own code produced.
    Tsquery(String),
}

impl TextQuery {
    /// The query text.
    ///
    /// ```
    /// assert_eq!(moso_sql::TextQuery::Plain("hi".into()).text(), "hi");
    /// ```
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Plain(text)
            | Self::Phrase(text)
            | Self::Websearch(text)
            | Self::Tsquery(text) => text,
        }
    }
}

/// A scalar function call.
///
/// [`Function::Custom`] takes an [`Ident`], so a function name can never be an
/// unvalidated string.
///
/// ```
/// use moso_sql::{Expr, Function, Ident};
///
/// let coalesced = Expr::Function(Function::Coalesce(vec![
///     Expr::col(Ident::from_static("nickname")),
///     Expr::col(Ident::from_static("name")),
/// ]));
/// assert!(matches!(coalesced, Expr::Function(_)));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Function {
    /// `coalesce(a, b, …)`.
    Coalesce(Vec<Expr>),
    /// `nullif(a, b)`.
    NullIf(Box<Expr>, Box<Expr>),
    /// `greatest(a, b, …)`.
    Greatest(Vec<Expr>),
    /// `least(a, b, …)`.
    Least(Vec<Expr>),
    /// `abs(x)`.
    Abs(Box<Expr>),
    /// `round(x)` or `round(x, n)`.
    Round {
        /// The value to round.
        operand: Box<Expr>,
        /// How many decimal places to keep.
        decimals: Option<Box<Expr>>,
    },
    /// `floor(x)`.
    Floor(Box<Expr>),
    /// `ceil(x)`.
    Ceil(Box<Expr>),
    /// `lower(s)`.
    Lower(Box<Expr>),
    /// `upper(s)`.
    Upper(Box<Expr>),
    /// `length(s)`.
    Length(Box<Expr>),
    /// `trim(… from s)`.
    Trim {
        /// The string to trim.
        operand: Box<Expr>,
        /// Which end or ends to trim.
        mode: TrimMode,
        /// The characters to trim, defaulting to whitespace.
        characters: Option<Box<Expr>>,
    },
    /// `substring(s from a for b)`.
    Substring {
        /// The string.
        operand: Box<Expr>,
        /// The one-based start offset.
        from: Option<Box<Expr>>,
        /// How many characters to take.
        length: Option<Box<Expr>>,
    },
    /// `replace(s, from, to)`.
    Replace {
        /// The string.
        operand: Box<Expr>,
        /// The substring to find.
        from: Box<Expr>,
        /// What to put in its place.
        to: Box<Expr>,
    },
    /// `concat(a, b, …)`.
    Concat(Vec<Expr>),
    /// `concat_ws(sep, a, b, …)`.
    ConcatWs {
        /// The separator.
        separator: Box<Expr>,
        /// The parts.
        items: Vec<Expr>,
    },
    /// `now()`.
    Now,
    /// `current_date`.
    CurrentDate,
    /// `current_time`.
    CurrentTime,
    /// `current_timestamp`.
    CurrentTimestamp,
    /// `random()`.
    Random,
    /// `to_tsvector(config, document)`.
    ToTsVector {
        /// The text-search configuration, defaulting to the server's.
        config: Option<Ident>,
        /// The document.
        document: Box<Expr>,
    },
    /// `websearch_to_tsquery(config, query)` and its siblings.
    ToTsQuery {
        /// The text-search configuration, defaulting to the server's.
        config: Option<Ident>,
        /// The query, which is always bound as a parameter.
        query: TextQuery,
    },
    /// `ts_rank(vector, query)` — the relevance score to sort a search by.
    TsRank {
        /// The document vector.
        vector: Box<Expr>,
        /// The parsed query.
        query: Box<Expr>,
        /// PostgreSQL's normalisation bitmask.
        normalization: Option<i32>,
    },
    /// `ts_headline(config, document, query)` — the highlighted excerpt.
    TsHeadline {
        /// The text-search configuration.
        config: Option<Ident>,
        /// The document.
        document: Box<Expr>,
        /// The parsed query.
        query: Box<Expr>,
        /// The options string, bound as a parameter.
        options: Option<String>,
    },
    /// Any other function, named by a validated identifier.
    Custom {
        /// The function name.
        name: Ident,
        /// Its arguments.
        args: Vec<Expr>,
    },
}

impl Function {
    /// Wraps the call as an expression.
    ///
    /// ```
    /// use moso_sql::{Expr, Function};
    ///
    /// assert!(matches!(Function::Now.into_expr(), Expr::Function(_)));
    /// ```
    #[must_use]
    pub fn into_expr(self) -> Expr {
        Expr::Function(self)
    }

    /// A call to a function named by a validated identifier.
    ///
    /// ```
    /// use moso_sql::{Expr, Function, Ident};
    ///
    /// let call = Function::custom(Ident::from_static("gen_random_uuid"), []);
    /// assert!(matches!(call, Function::Custom { .. }));
    /// ```
    #[must_use]
    pub fn custom(name: Ident, args: impl IntoIterator<Item = Expr>) -> Self {
        Self::Custom {
            name,
            args: args.into_iter().collect(),
        }
    }
}

/// Which end of a string `trim` removes characters from.
///
/// ```
/// use moso_sql::TrimMode;
///
/// assert_ne!(TrimMode::Both, TrimMode::Leading);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrimMode {
    /// `BOTH` — the SQL default.
    #[default]
    Both,
    /// `LEADING`.
    Leading,
    /// `TRAILING`.
    Trailing,
}

/// An aggregate call, with the modifiers PostgreSQL allows on one.
///
/// ```
/// use moso_sql::{Aggregate, AggregateFunc, Expr, Ident};
///
/// // `count(*) filter (where published)`
/// let published = Aggregate::count_star()
///     .filter(Expr::col(Ident::from_static("published")));
/// assert_eq!(published.func(), AggregateFunc::Count);
/// assert!(published.is_star());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Aggregate {
    func: AggregateFunc,
    args: Vec<Expr>,
    star: bool,
    distinct: bool,
    filter: Option<Expr>,
    order_by: Vec<OrderTerm>,
}

impl Aggregate {
    /// An aggregate over the given arguments.
    ///
    /// ```
    /// use moso_sql::{Aggregate, AggregateFunc, Expr, Ident};
    ///
    /// let total = Aggregate::new(AggregateFunc::Sum, [Expr::col(Ident::from_static("amount"))]);
    /// assert_eq!(total.args().len(), 1);
    /// ```
    #[must_use]
    pub fn new(func: AggregateFunc, args: impl IntoIterator<Item = Expr>) -> Self {
        Self {
            func,
            args: args.into_iter().collect(),
            star: false,
            distinct: false,
            filter: None,
            order_by: Vec::new(),
        }
    }

    /// `count(*)`.
    ///
    /// ```
    /// assert!(moso_sql::Aggregate::count_star().is_star());
    /// ```
    #[must_use]
    pub fn count_star() -> Self {
        Self {
            func: AggregateFunc::Count,
            args: Vec::new(),
            star: true,
            distinct: false,
            filter: None,
            order_by: Vec::new(),
        }
    }

    /// Adds `DISTINCT` to the argument list.
    ///
    /// ```
    /// use moso_sql::{Aggregate, AggregateFunc, Expr, Ident};
    ///
    /// let unique = Aggregate::new(AggregateFunc::Count, [Expr::col(Ident::from_static("id"))])
    ///     .distinct();
    /// assert!(unique.is_distinct());
    /// ```
    #[must_use]
    pub fn distinct(mut self) -> Self {
        self.distinct = true;
        self
    }

    /// Adds a `FILTER (WHERE …)` clause, which is how one query counts several
    /// disjoint groups in one pass.
    ///
    /// ```
    /// use moso_sql::{Aggregate, Expr, Ident};
    ///
    /// let filtered = Aggregate::count_star().filter(Expr::col(Ident::from_static("ok")));
    /// assert!(filtered.filter_expr().is_some());
    /// ```
    #[must_use]
    pub fn filter(mut self, expr: Expr) -> Self {
        self.filter = Some(match self.filter.take() {
            Some(existing) => existing.and(expr),
            None => expr,
        });
        self
    }

    /// Adds an `ORDER BY` inside the aggregate, which `string_agg` and
    /// `array_agg` need to be deterministic.
    ///
    /// ```
    /// use moso_sql::{Aggregate, AggregateFunc, Expr, Ident, OrderTerm};
    ///
    /// let names = Aggregate::new(AggregateFunc::ArrayAgg, [Expr::col(Ident::from_static("name"))])
    ///     .order_by(OrderTerm::asc(Expr::col(Ident::from_static("name"))));
    /// assert_eq!(names.order_terms().len(), 1);
    /// ```
    #[must_use]
    pub fn order_by(mut self, term: OrderTerm) -> Self {
        self.order_by.push(term);
        self
    }

    /// The aggregate function.
    ///
    /// ```
    /// use moso_sql::{Aggregate, AggregateFunc};
    ///
    /// assert_eq!(Aggregate::count_star().func(), AggregateFunc::Count);
    /// ```
    #[must_use]
    pub fn func(&self) -> AggregateFunc {
        self.func.clone()
    }

    /// The arguments.
    ///
    /// ```
    /// assert!(moso_sql::Aggregate::count_star().args().is_empty());
    /// ```
    #[must_use]
    pub fn args(&self) -> &[Expr] {
        &self.args
    }

    /// Whether the argument is `*`.
    ///
    /// ```
    /// assert!(moso_sql::Aggregate::count_star().is_star());
    /// ```
    #[must_use]
    pub const fn is_star(&self) -> bool {
        self.star
    }

    /// Whether `DISTINCT` was asked for.
    ///
    /// ```
    /// assert!(!moso_sql::Aggregate::count_star().is_distinct());
    /// ```
    #[must_use]
    pub const fn is_distinct(&self) -> bool {
        self.distinct
    }

    /// The `FILTER (WHERE …)` predicate, if any.
    ///
    /// ```
    /// assert!(moso_sql::Aggregate::count_star().filter_expr().is_none());
    /// ```
    #[must_use]
    pub const fn filter_expr(&self) -> Option<&Expr> {
        self.filter.as_ref()
    }

    /// The aggregate's internal `ORDER BY`.
    ///
    /// ```
    /// assert!(moso_sql::Aggregate::count_star().order_terms().is_empty());
    /// ```
    #[must_use]
    pub fn order_terms(&self) -> &[OrderTerm] {
        &self.order_by
    }

    /// Wraps the aggregate as an expression.
    ///
    /// ```
    /// use moso_sql::{Aggregate, Expr};
    ///
    /// assert!(matches!(Aggregate::count_star().into_expr(), Expr::Aggregate(_)));
    /// ```
    #[must_use]
    pub fn into_expr(self) -> Expr {
        Expr::Aggregate(Box::new(self))
    }
}

/// The aggregate functions Moso names directly.
///
/// ```
/// use moso_sql::AggregateFunc;
///
/// assert_eq!(AggregateFunc::Count, AggregateFunc::Count);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AggregateFunc {
    /// `count`.
    Count,
    /// `sum`.
    Sum,
    /// `avg`.
    Avg,
    /// `min`.
    Min,
    /// `max`.
    Max,
    /// `array_agg`.
    ArrayAgg,
    /// `string_agg`. Takes a separator as its second argument.
    StringAgg,
    /// `json_agg`.
    JsonAgg,
    /// `jsonb_agg`.
    JsonbAgg,
    /// `json_object_agg`.
    JsonObjectAgg,
    /// `jsonb_object_agg`.
    JsonbObjectAgg,
    /// `bool_and`.
    BoolAnd,
    /// `bool_or`.
    BoolOr,
    /// `stddev`.
    StdDev,
    /// `variance`.
    Variance,
    /// Any other aggregate, named by a validated identifier.
    Custom(Ident),
}

/// A window-function call: `f(…) OVER (…)`.
///
/// `ROW_NUMBER() OVER (PARTITION BY … ORDER BY …)` is how a batched preload
/// takes the first *n* children of every parent in one statement, which is
/// non-negotiable N3.
///
/// ```
/// use moso_sql::{Expr, Ident, OrderTerm, WindowExpr, WindowFunc, WindowSpec};
///
/// let ranked = WindowExpr::new(
///     WindowFunc::RowNumber,
///     [],
///     WindowSpec::new()
///         .partition_by(Expr::col(Ident::from_static("author_id")))
///         .order_by(OrderTerm::desc(Expr::col(Ident::from_static("created_at")))),
/// );
/// assert!(matches!(ranked.into_expr(), Expr::Window(_)));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct WindowExpr {
    func: WindowFunc,
    args: Vec<Expr>,
    window: WindowRef,
}

impl WindowExpr {
    /// A window call over an inline window specification.
    ///
    /// ```
    /// use moso_sql::{WindowExpr, WindowFunc, WindowSpec};
    ///
    /// let e = WindowExpr::new(WindowFunc::Rank, [], WindowSpec::new());
    /// assert_eq!(e.args().len(), 0);
    /// ```
    #[must_use]
    pub fn new(func: WindowFunc, args: impl IntoIterator<Item = Expr>, spec: WindowSpec) -> Self {
        Self {
            func,
            args: args.into_iter().collect(),
            window: WindowRef::Spec(spec),
        }
    }

    /// A window call over a window declared once in the query's `WINDOW`
    /// clause.
    ///
    /// ```
    /// use moso_sql::{Ident, WindowExpr, WindowFunc};
    ///
    /// let e = WindowExpr::over_named(WindowFunc::Rank, [], Ident::from_static("w"));
    /// assert!(matches!(e.window(), moso_sql::WindowRef::Named(_)));
    /// ```
    #[must_use]
    pub fn over_named(func: WindowFunc, args: impl IntoIterator<Item = Expr>, name: Ident) -> Self {
        Self {
            func,
            args: args.into_iter().collect(),
            window: WindowRef::Named(name),
        }
    }

    /// The function being windowed.
    ///
    /// ```
    /// use moso_sql::{WindowExpr, WindowFunc, WindowSpec};
    ///
    /// let e = WindowExpr::new(WindowFunc::RowNumber, [], WindowSpec::new());
    /// assert!(matches!(e.func(), WindowFunc::RowNumber));
    /// ```
    #[must_use]
    pub const fn func(&self) -> &WindowFunc {
        &self.func
    }

    /// The function's arguments.
    ///
    /// ```
    /// # use moso_sql::{WindowExpr, WindowFunc, WindowSpec};
    /// assert!(WindowExpr::new(WindowFunc::Rank, [], WindowSpec::new()).args().is_empty());
    /// ```
    #[must_use]
    pub fn args(&self) -> &[Expr] {
        &self.args
    }

    /// The window the call runs over.
    ///
    /// ```
    /// # use moso_sql::{WindowExpr, WindowFunc, WindowRef, WindowSpec};
    /// let e = WindowExpr::new(WindowFunc::Rank, [], WindowSpec::new());
    /// assert!(matches!(e.window(), WindowRef::Spec(_)));
    /// ```
    #[must_use]
    pub const fn window(&self) -> &WindowRef {
        &self.window
    }

    /// Wraps the call as an expression.
    ///
    /// ```
    /// # use moso_sql::{Expr, WindowExpr, WindowFunc, WindowSpec};
    /// let e = WindowExpr::new(WindowFunc::Rank, [], WindowSpec::new()).into_expr();
    /// assert!(matches!(e, Expr::Window(_)));
    /// ```
    #[must_use]
    pub fn into_expr(self) -> Expr {
        Expr::Window(Box::new(self))
    }
}

/// The function of a window call.
///
/// ```
/// use moso_sql::WindowFunc;
///
/// assert!(matches!(WindowFunc::RowNumber, WindowFunc::RowNumber));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum WindowFunc {
    /// `row_number()`.
    RowNumber,
    /// `rank()`.
    Rank,
    /// `dense_rank()`.
    DenseRank,
    /// `percent_rank()`.
    PercentRank,
    /// `cume_dist()`.
    CumeDist,
    /// `ntile(n)`.
    Ntile,
    /// `lag(expr, offset, default)`.
    Lag,
    /// `lead(expr, offset, default)`.
    Lead,
    /// `first_value(expr)`.
    FirstValue,
    /// `last_value(expr)`.
    LastValue,
    /// `nth_value(expr, n)`.
    NthValue,
    /// An ordinary aggregate used as a window function: `sum(x) OVER (…)`.
    Aggregate(Box<Aggregate>),
    /// Any other window function, named by a validated identifier.
    Custom(Ident),
}

/// Where a window call gets its window from.
///
/// ```
/// use moso_sql::{Ident, WindowRef};
///
/// assert!(matches!(WindowRef::Named(Ident::from_static("w")), WindowRef::Named(_)));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum WindowRef {
    /// A window declared in the query's `WINDOW` clause.
    Named(Ident),
    /// A window written inline.
    Spec(WindowSpec),
}

/// A window specification: how to partition, how to order, and which rows are
/// in the frame.
///
/// ```
/// use moso_sql::{Expr, Ident, WindowSpec};
///
/// let per_author = WindowSpec::new().partition_by(Expr::col(Ident::from_static("author_id")));
/// assert_eq!(per_author.partitions().len(), 1);
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowSpec {
    partition_by: Vec<Expr>,
    order_by: Vec<OrderTerm>,
    frame: Option<Frame>,
}

impl WindowSpec {
    /// An empty window: the whole result set, in no particular order.
    ///
    /// ```
    /// assert!(moso_sql::WindowSpec::new().partitions().is_empty());
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            partition_by: Vec::new(),
            order_by: Vec::new(),
            frame: None,
        }
    }

    /// Adds a `PARTITION BY` expression.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, WindowSpec};
    /// let w = WindowSpec::new().partition_by(Expr::col(Ident::from_static("a")));
    /// assert_eq!(w.partitions().len(), 1);
    /// ```
    #[must_use]
    pub fn partition_by(mut self, expr: Expr) -> Self {
        self.partition_by.push(expr);
        self
    }

    /// Adds an `ORDER BY` term.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, OrderTerm, WindowSpec};
    /// let w = WindowSpec::new().order_by(OrderTerm::asc(Expr::col(Ident::from_static("a"))));
    /// assert_eq!(w.order_terms().len(), 1);
    /// ```
    #[must_use]
    pub fn order_by(mut self, term: OrderTerm) -> Self {
        self.order_by.push(term);
        self
    }

    /// Sets the frame.
    ///
    /// ```
    /// # use moso_sql::{Frame, FrameBound, FrameUnits, WindowSpec};
    /// let w = WindowSpec::new().frame(Frame::new(FrameUnits::Rows, FrameBound::UnboundedPreceding));
    /// assert!(w.frame_spec().is_some());
    /// ```
    #[must_use]
    pub fn frame(mut self, frame: Frame) -> Self {
        self.frame = Some(frame);
        self
    }

    /// The `PARTITION BY` expressions.
    ///
    /// ```
    /// assert!(moso_sql::WindowSpec::new().partitions().is_empty());
    /// ```
    #[must_use]
    pub fn partitions(&self) -> &[Expr] {
        &self.partition_by
    }

    /// The `ORDER BY` terms.
    ///
    /// ```
    /// assert!(moso_sql::WindowSpec::new().order_terms().is_empty());
    /// ```
    #[must_use]
    pub fn order_terms(&self) -> &[OrderTerm] {
        &self.order_by
    }

    /// The frame, if one was given.
    ///
    /// ```
    /// assert!(moso_sql::WindowSpec::new().frame_spec().is_none());
    /// ```
    #[must_use]
    pub const fn frame_spec(&self) -> Option<&Frame> {
        self.frame.as_ref()
    }
}

/// A window frame: which rows around the current one the function sees.
///
/// ```
/// use moso_sql::{Frame, FrameBound, FrameUnits};
///
/// let running_total = Frame::new(FrameUnits::Rows, FrameBound::UnboundedPreceding)
///     .to(FrameBound::CurrentRow);
/// assert_eq!(running_total.units(), FrameUnits::Rows);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    units: FrameUnits,
    start: FrameBound,
    end: Option<FrameBound>,
    exclusion: Option<FrameExclusion>,
}

impl Frame {
    /// A frame starting at `start` and ending at the current row.
    ///
    /// ```
    /// # use moso_sql::{Frame, FrameBound, FrameUnits};
    /// let f = Frame::new(FrameUnits::Range, FrameBound::CurrentRow);
    /// assert_eq!(f.start(), &FrameBound::CurrentRow);
    /// ```
    #[must_use]
    pub const fn new(units: FrameUnits, start: FrameBound) -> Self {
        Self {
            units,
            start,
            end: None,
            exclusion: None,
        }
    }

    /// Sets the frame's end bound, making it a `BETWEEN … AND …` frame.
    ///
    /// ```
    /// # use moso_sql::{Frame, FrameBound, FrameUnits};
    /// let f = Frame::new(FrameUnits::Rows, FrameBound::Preceding(1)).to(FrameBound::Following(1));
    /// assert_eq!(f.end(), Some(&FrameBound::Following(1)));
    /// ```
    #[must_use]
    pub fn to(mut self, end: FrameBound) -> Self {
        self.end = Some(end);
        self
    }

    /// Sets the `EXCLUDE` clause.
    ///
    /// ```
    /// # use moso_sql::{Frame, FrameBound, FrameExclusion, FrameUnits};
    /// let f = Frame::new(FrameUnits::Rows, FrameBound::CurrentRow)
    ///     .exclude(FrameExclusion::CurrentRow);
    /// assert_eq!(f.exclusion(), Some(FrameExclusion::CurrentRow));
    /// ```
    #[must_use]
    pub fn exclude(mut self, exclusion: FrameExclusion) -> Self {
        self.exclusion = Some(exclusion);
        self
    }

    /// The frame's unit of measurement.
    ///
    /// ```
    /// # use moso_sql::{Frame, FrameBound, FrameUnits};
    /// assert_eq!(Frame::new(FrameUnits::Groups, FrameBound::CurrentRow).units(), FrameUnits::Groups);
    /// ```
    #[must_use]
    pub const fn units(&self) -> FrameUnits {
        self.units
    }

    /// The start bound.
    ///
    /// ```
    /// # use moso_sql::{Frame, FrameBound, FrameUnits};
    /// let f = Frame::new(FrameUnits::Rows, FrameBound::CurrentRow);
    /// assert_eq!(f.start(), &FrameBound::CurrentRow);
    /// ```
    #[must_use]
    pub const fn start(&self) -> &FrameBound {
        &self.start
    }

    /// The end bound, if the frame has one.
    ///
    /// ```
    /// # use moso_sql::{Frame, FrameBound, FrameUnits};
    /// assert!(Frame::new(FrameUnits::Rows, FrameBound::CurrentRow).end().is_none());
    /// ```
    #[must_use]
    pub const fn end(&self) -> Option<&FrameBound> {
        self.end.as_ref()
    }

    /// The `EXCLUDE` clause, if the frame has one.
    ///
    /// ```
    /// # use moso_sql::{Frame, FrameBound, FrameUnits};
    /// assert!(Frame::new(FrameUnits::Rows, FrameBound::CurrentRow).exclusion().is_none());
    /// ```
    #[must_use]
    pub const fn exclusion(&self) -> Option<FrameExclusion> {
        self.exclusion
    }
}

/// What a frame bound counts.
///
/// ```
/// use moso_sql::FrameUnits;
///
/// assert_ne!(FrameUnits::Rows, FrameUnits::Range);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameUnits {
    /// Physical rows.
    Rows,
    /// Values of the ordering expression.
    Range,
    /// Peer groups of the ordering expression.
    Groups,
}

/// One end of a window frame.
///
/// ```
/// use moso_sql::FrameBound;
///
/// assert_eq!(FrameBound::Preceding(3), FrameBound::Preceding(3));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameBound {
    /// From the start of the partition.
    UnboundedPreceding,
    /// This many units before the current row.
    Preceding(u64),
    /// The current row.
    CurrentRow,
    /// This many units after the current row.
    Following(u64),
    /// To the end of the partition.
    UnboundedFollowing,
}

/// A frame's `EXCLUDE` clause.
///
/// ```
/// use moso_sql::FrameExclusion;
///
/// assert_ne!(FrameExclusion::CurrentRow, FrameExclusion::Ties);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameExclusion {
    /// `EXCLUDE CURRENT ROW`.
    CurrentRow,
    /// `EXCLUDE GROUP`.
    Group,
    /// `EXCLUDE TIES`.
    Ties,
    /// `EXCLUDE NO OTHERS`.
    NoOthers,
}

/// `CASE … WHEN … THEN … ELSE … END`.
///
/// ```
/// use moso_sql::{Case, Expr, Ident};
///
/// let label = Case::new()
///     .when(Expr::col(Ident::from_static("score")).ge(Expr::value(90)), Expr::value("a"))
///     .when(Expr::col(Ident::from_static("score")).ge(Expr::value(80)), Expr::value("b"))
///     .otherwise(Expr::value("c"));
/// assert_eq!(label.branches().len(), 2);
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Case {
    operand: Option<Expr>,
    branches: Vec<(Expr, Expr)>,
    otherwise: Option<Expr>,
}

impl Case {
    /// A searched `CASE`: every branch is its own predicate.
    ///
    /// ```
    /// assert!(moso_sql::Case::new().branches().is_empty());
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            operand: None,
            branches: Vec::new(),
            otherwise: None,
        }
    }

    /// A simple `CASE operand WHEN value THEN …`.
    ///
    /// ```
    /// # use moso_sql::{Case, Expr, Ident};
    /// let c = Case::on(Expr::col(Ident::from_static("status")));
    /// assert!(c.operand().is_some());
    /// ```
    #[must_use]
    pub fn on(operand: Expr) -> Self {
        Self {
            operand: Some(operand),
            branches: Vec::new(),
            otherwise: None,
        }
    }

    /// Adds a `WHEN … THEN …` branch.
    ///
    /// ```
    /// # use moso_sql::{Case, Expr};
    /// let c = Case::new().when(Expr::value(true), Expr::value(1));
    /// assert_eq!(c.branches().len(), 1);
    /// ```
    #[must_use]
    pub fn when(mut self, condition: Expr, result: Expr) -> Self {
        self.branches.push((condition, result));
        self
    }

    /// Sets the `ELSE` result.
    ///
    /// ```
    /// # use moso_sql::{Case, Expr};
    /// assert!(Case::new().otherwise(Expr::null()).default_result().is_some());
    /// ```
    #[must_use]
    pub fn otherwise(mut self, result: Expr) -> Self {
        self.otherwise = Some(result);
        self
    }

    /// The operand of a simple `CASE`.
    ///
    /// ```
    /// assert!(moso_sql::Case::new().operand().is_none());
    /// ```
    #[must_use]
    pub const fn operand(&self) -> Option<&Expr> {
        self.operand.as_ref()
    }

    /// The `WHEN … THEN …` branches.
    ///
    /// ```
    /// assert!(moso_sql::Case::new().branches().is_empty());
    /// ```
    #[must_use]
    pub fn branches(&self) -> &[(Expr, Expr)] {
        &self.branches
    }

    /// The `ELSE` result.
    ///
    /// ```
    /// assert!(moso_sql::Case::new().default_result().is_none());
    /// ```
    #[must_use]
    pub const fn default_result(&self) -> Option<&Expr> {
        self.otherwise.as_ref()
    }

    /// Wraps the `CASE` as an expression.
    ///
    /// ```
    /// # use moso_sql::{Case, Expr};
    /// assert!(matches!(Case::new().into_expr(), Expr::Case(_)));
    /// ```
    #[must_use]
    pub fn into_expr(self) -> Expr {
        Expr::Case(Box::new(self))
    }
}

/// A raw SQL fragment with bound parameters.
///
/// # The placeholder convention
///
/// Inside a fragment, `?` is a placeholder and `??` is a literal question
/// mark. The dialect renumbers placeholders into its own spelling, so the same
/// fragment works on PostgreSQL and SQLite. The number of placeholders must
/// match the number of bound values or
/// [`Error::RawArity`](crate::Error::RawArity) is returned at build time.
///
/// This is the expression half of non-negotiable N8. It is an escape hatch, not
/// a shortcut: everything inside the fragment is emitted verbatim, so never
/// build one by formatting user input into a string.
///
/// ```
/// use moso_sql::RawExpr;
///
/// let recent = RawExpr::new("created_at > now() - ?::interval").bind("1 day");
/// assert_eq!(recent.placeholder_count(), 1);
/// assert_eq!(recent.args().len(), 1);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct RawExpr {
    fragment: String,
    args: Vec<Value>,
}

impl RawExpr {
    /// A fragment with no bound values yet.
    ///
    /// ```
    /// assert_eq!(moso_sql::RawExpr::new("now()").fragment(), "now()");
    /// ```
    #[must_use]
    pub fn new(fragment: impl Into<String>) -> Self {
        Self {
            fragment: fragment.into(),
            args: Vec::new(),
        }
    }

    /// A fragment with its values.
    ///
    /// ```
    /// use moso_sql::{RawExpr, Value};
    ///
    /// let e = RawExpr::with_args("a = ? and b = ?", [Value::I32(1), Value::I32(2)]);
    /// assert_eq!(e.args().len(), 2);
    /// ```
    #[must_use]
    pub fn with_args(fragment: impl Into<String>, args: impl IntoIterator<Item = Value>) -> Self {
        Self {
            fragment: fragment.into(),
            args: args.into_iter().collect(),
        }
    }

    /// Binds one more value, in placeholder order.
    ///
    /// ```
    /// assert_eq!(moso_sql::RawExpr::new("? + ?").bind(1).bind(2).args().len(), 2);
    /// ```
    #[must_use]
    pub fn bind(mut self, value: impl Bindable) -> Self {
        self.args.push(value.into_value());
        self
    }

    /// Binds one more already-built [`Value`].
    ///
    /// ```
    /// use moso_sql::{RawExpr, Value};
    ///
    /// assert_eq!(RawExpr::new("?").bind_value(Value::Bool(true)).args().len(), 1);
    /// ```
    #[must_use]
    pub fn bind_value(mut self, value: Value) -> Self {
        self.args.push(value);
        self
    }

    /// The fragment text, with its placeholders unexpanded.
    ///
    /// ```
    /// assert_eq!(moso_sql::RawExpr::new("x = ?").fragment(), "x = ?");
    /// ```
    #[must_use]
    pub fn fragment(&self) -> &str {
        &self.fragment
    }

    /// The bound values, in placeholder order.
    ///
    /// ```
    /// assert!(moso_sql::RawExpr::new("now()").args().is_empty());
    /// ```
    #[must_use]
    pub fn args(&self) -> &[Value] {
        &self.args
    }

    /// How many placeholders the fragment has, counting `??` as a literal
    /// question mark rather than as one.
    ///
    /// ```
    /// use moso_sql::RawExpr;
    ///
    /// assert_eq!(RawExpr::new("a = ? and b ?? c").placeholder_count(), 1);
    /// assert_eq!(RawExpr::new("????").placeholder_count(), 0);
    /// ```
    #[must_use]
    pub fn placeholder_count(&self) -> usize {
        let bytes = self.fragment.as_bytes();
        let mut count = 0;
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'?' {
                if bytes.get(index + 1) == Some(&b'?') {
                    index += 2;
                    continue;
                }
                count += 1;
            }
            index += 1;
        }
        count
    }

    /// Wraps the fragment as an expression.
    ///
    /// ```
    /// # use moso_sql::{Expr, RawExpr};
    /// assert!(matches!(RawExpr::new("1").into_expr(), Expr::Raw(_)));
    /// ```
    #[must_use]
    pub fn into_expr(self) -> Expr {
        Expr::Raw(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operators_build_the_tree_the_docs_promise() {
        let expr = Expr::col(Ident::from_static("a")).eq(Expr::value(1))
            & Expr::col(Ident::from_static("b")).eq(Expr::value(2));
        match expr {
            Expr::Binary { op, .. } => assert_eq!(op, BinOp::And),
            other => panic!("expected an AND, got {other:?}"),
        }
    }

    #[test]
    fn folding_an_empty_filter_list_is_none_not_true() {
        assert!(Expr::all_of(Vec::new()).is_none());
        assert!(Expr::any_of(Vec::new()).is_none());
        let single = Expr::value(1);
        assert_eq!(Expr::all_of([single.clone()]), Some(single));
    }

    #[test]
    fn negating_a_comparison_is_the_mirror_operator() {
        for op in [
            BinOp::Eq,
            BinOp::NotEq,
            BinOp::Lt,
            BinOp::LtEq,
            BinOp::Gt,
            BinOp::GtEq,
        ] {
            let back = op.negated().and_then(BinOp::negated);
            assert_eq!(back, Some(op));
        }
        assert_eq!(BinOp::Concat.negated(), None);
    }

    #[test]
    fn a_doubled_question_mark_is_not_a_placeholder() {
        assert_eq!(RawExpr::new("a ? b").placeholder_count(), 1);
        assert_eq!(RawExpr::new("a ?? b").placeholder_count(), 0);
        assert_eq!(RawExpr::new("a ??? b").placeholder_count(), 1);
        assert_eq!(RawExpr::new("").placeholder_count(), 0);
    }

    #[test]
    fn escape_only_touches_a_like() {
        let like = Expr::col(Ident::from_static("p"))
            .like(Expr::value("a"))
            .escape('!');
        assert!(matches!(
            like,
            Expr::Like {
                escape: Some('!'),
                ..
            }
        ));
        let value = Expr::value(1).escape('!');
        assert!(matches!(value, Expr::Value(_)));
    }

    #[test]
    fn an_aggregate_filter_accumulates_with_and() {
        let aggregate = Aggregate::count_star()
            .filter(Expr::col(Ident::from_static("a")))
            .filter(Expr::col(Ident::from_static("b")));
        match aggregate.filter_expr() {
            Some(Expr::Binary { op, .. }) => assert_eq!(*op, BinOp::And),
            other => panic!("expected an AND, got {other:?}"),
        }
    }
}
