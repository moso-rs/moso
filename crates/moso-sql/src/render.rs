//! The renderer: one walk over a [`StatementRef`], driven by the dialect's
//! [`Capabilities`].
//!
//! # Why the walk is here and not in `sea-query`
//!
//! ADR-0005 borrows an engine so that Moso does not write one. `sea-query` is
//! that engine, and it is genuinely good — but its *statement AST* cannot
//! express a large part of the surface `moso-sql`'s spine froze, and its value
//! type cannot round-trip Moso's. The two problems, concretely:
//!
//! * **Coverage.** `IS DISTINCT FROM`, `jsonb`'s `?` / `?|` / `?&` / `#>` /
//!   `#>>` / `#-`, `FILTER (WHERE …)`, an aggregate's internal `ORDER BY`,
//!   `LATERAL`, a `VALUES` list as a `FROM` item, `WITH ORDINALITY`,
//!   `EXCLUDE` on a window frame, `CREATE INDEX CONCURRENTLY`, partial and
//!   covering indexes, operator classes, `ADD CONSTRAINT … NOT VALID`,
//!   `VALIDATE CONSTRAINT`, `ADD … USING INDEX`, `ATTACH PARTITION`,
//!   `PARTITION BY`, `TRUNCATE`, `CREATE SCHEMA`, `COMMENT ON` and
//!   `EXCLUDE USING` have no representation in it. Every one of them is in the
//!   frozen surface, and four of them are named in `23-migrations.md` as the
//!   difference between a schema change that takes a lock for a millisecond and
//!   one that takes the site down.
//! * **Values.** Round-tripping a bound parameter through `sea_query::Value`
//!   would be lossy for two of Moso's own scalars: [`Decimal`](crate::Decimal)
//!   carries an `i128` mantissa where `rust_decimal` carries 96 bits, and
//!   [`Timestamp`]
//!   carries an `i64` second count where `chrono` carries a narrower range.
//!   Silently truncating a `numeric` is exactly the class of bug the crate
//!   exists to prevent.
//!
//! So the walk is Moso's. `sea-query` stays a declared dependency and stays
//! *useful*: `crate::engine` renders the constructs it does cover through it and
//! asserts, byte for byte, that this renderer agrees with it. A mature engine as
//! a differential oracle is a stronger correctness argument than delegation
//! would have been, and it keeps the ADR-0005 reversal cheap — the day someone
//! swaps the oracle, the tests say whether the swap changed anything.
//!
//! # Two invariants this file must never break
//!
//! 1. **An identifier reaches the output only through [`Renderer::ident`]**,
//!    which delegates to [`Dialect::quote_ident`]. There is no other path.
//! 2. **A value reaches the output only through [`Renderer::value`]**, which in
//!    [`Binding::Parameter`] mode emits a placeholder and pushes onto
//!    [`Sql::args`]. The one exception is [`Binding::Literal`], used for DDL,
//!    where the protocol has no parameters at all — and it quotes by doubling,
//!    which is the only escape both PostgreSQL (with
//!    `standard_conforming_strings`, the default since 9.1) and SQLite accept.

use core::fmt::Write as _;

use crate::ddl::{
    AlterTable, AlterTableAction, ColumnSpec, CommentOn, CommentTarget, CreateExtension,
    CreateIndex, CreateSchema, CreateTable, CreateType, Ddl, DropIndex, DropSchema, DropTable,
    DropType, ForeignKey, Generated, Identity, IndexMethod, IndexTarget, Partitioning,
    ReferentialAction, RenameIndex, RenameTable, TableConstraint, Truncate, TypeBody,
};
use crate::ddl::{AlterType, AlterTypeAction, PartitionStrategy};
use crate::delete::Delete;
use crate::dialect::{Capabilities, Dialect, Postgres, Sqlite};
use crate::error::Error;
use crate::expr::{
    Aggregate, AggregateFunc, BinOp, Case, Expr, Frame, FrameBound, FrameExclusion, FrameUnits,
    Function, JsonOp, Quantifier, RawExpr, TextQuery, TrimMode, UnOp, WindowExpr, WindowFunc,
    WindowRef, WindowSpec,
};
use crate::ident::{ColumnRef, Ident, IdentError, TableRef, TypeRef};
use crate::insert::{ConflictAction, ConflictTarget, Insert, OnConflict};
use crate::order::{Nulls, Order, OrderTerm};
use crate::select::{
    Cte, Distinct, FromItem, Join, JoinCondition, JoinKind, Lock, LockBehavior, LockStrength,
    Select, SelectItem, SetOp,
};
use crate::sql::Sql;
use crate::statement::{Assignment, RawStatement, Returning, Statement, StatementRef};
use crate::types::DataType;
use crate::update::Update;
use crate::value::{Array, Timestamp, Value, ValueKind};

/// Renders `statement` for `dialect`.
///
/// This is the whole of [`Dialect::build`] for both built-in dialects, and the
/// function a third-party dialect should call from its own `build` unless it
/// needs a different grammar.
pub(crate) fn build(dialect: &dyn Dialect, statement: StatementRef<'_>) -> Result<Sql, Error> {
    let mut renderer = Renderer::new(dialect);
    match statement {
        StatementRef::Select(select) => renderer.select(select)?,
        StatementRef::Insert(insert) => renderer.insert(insert)?,
        StatementRef::Update(update) => renderer.update(update)?,
        StatementRef::Delete(delete) => renderer.delete(delete)?,
        StatementRef::Ddl(ddl) => renderer.ddl(ddl)?,
        StatementRef::Raw(raw) => renderer.raw_statement(raw)?,
    }
    renderer.check_parameter_budget(statement)?;
    Ok(Sql {
        text: renderer.out,
        args: renderer.args,
    })
}

// ── precedence ──────────────────────────────────────────────────────────────

/// The loosest binding, used when the context already supplies parentheses.
const P_MIN: u8 = 0;
/// `OR`.
const P_OR: u8 = 1;
/// `AND`.
const P_AND: u8 = 2;
/// `NOT`.
const P_NOT: u8 = 3;
/// `IS NULL`, `IS DISTINCT FROM`.
const P_IS: u8 = 4;
/// `=`, `<>`, `<`, `<=`, `>`, `>=`.
const P_CMP: u8 = 5;
/// `BETWEEN`, `IN`, `LIKE`.
const P_MATCH: u8 = 6;
/// Everything PostgreSQL's grammar calls "any other operator": `||`, the
/// `jsonb` operators, the bitwise operators, the shifts, `~`, `@@`, `@>`.
const P_OTHER: u8 = 7;
/// `+`, `-`.
const P_ADD: u8 = 8;
/// `*`, `/`, `%`.
const P_MUL: u8 = 9;
/// `^`.
const P_EXP: u8 = 10;
/// Prefix `-` and `~`.
const P_UNARY: u8 = 11;
/// Something that needs no parentheses anywhere: a value, a column, a function
/// call, a parenthesised subquery.
const P_ATOM: u8 = 12;

/// How tightly `expr` binds, so that [`Renderer::expr`] can drop the
/// parentheses a reader would not have written.
fn precedence(expr: &Expr) -> u8 {
    match expr {
        Expr::Value(_)
        | Expr::Column(_)
        | Expr::Tuple(_)
        | Expr::Array(_)
        | Expr::Nested(_)
        | Expr::Exists { .. }
        | Expr::Scalar(_)
        | Expr::Case(_)
        | Expr::Cast { .. }
        | Expr::Function(_)
        | Expr::Aggregate(_)
        | Expr::Window(_)
        | Expr::Default => P_ATOM,
        // A raw fragment is opaque, so it is always parenthesised: assuming it
        // binds tightly is how `a AND raw("b OR c")` silently changes meaning.
        Expr::Raw(_) => P_MIN,
        Expr::Binary { op, .. } => binary_precedence(*op),
        Expr::Unary { op, .. } => match op {
            UnOp::Not => P_NOT,
            UnOp::Neg | UnOp::BitNot => P_UNARY,
        },
        Expr::IsNull { .. } => P_IS,
        Expr::Between { .. }
        | Expr::Like { .. }
        | Expr::InList { .. }
        | Expr::InSubquery { .. } => P_MATCH,
        Expr::Quantified { .. } => P_CMP,
        Expr::Json { .. } => P_OTHER,
    }
}

/// How tightly an infix operator binds.
const fn binary_precedence(op: BinOp) -> u8 {
    match op {
        BinOp::Or => P_OR,
        BinOp::And => P_AND,
        BinOp::IsDistinctFrom | BinOp::IsNotDistinctFrom => P_IS,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => P_CMP,
        BinOp::Add | BinOp::Sub => P_ADD,
        BinOp::Mul | BinOp::Div | BinOp::Mod => P_MUL,
        BinOp::Exp => P_EXP,
        BinOp::Concat
        | BinOp::BitAnd
        | BinOp::BitOr
        | BinOp::BitXor
        | BinOp::ShiftLeft
        | BinOp::ShiftRight
        | BinOp::Regex
        | BinOp::RegexCaseInsensitive
        | BinOp::NotRegex
        | BinOp::NotRegexCaseInsensitive
        | BinOp::TextMatch
        | BinOp::ArrayContains
        | BinOp::ArrayContainedBy
        | BinOp::ArrayOverlaps => P_OTHER,
    }
}

// ── the renderer ────────────────────────────────────────────────────────────

/// Whether a [`Value`] becomes a placeholder or a literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Binding {
    /// The value is bound as a parameter. Every DML statement.
    Parameter,
    /// The value is written into the text. DDL only: `DEFAULT`, `CHECK`, a
    /// generated column's expression and a partial index's predicate are parsed
    /// once by the server and stored in the catalogue, so the protocol has no
    /// parameter to bind them to.
    Literal,
}

/// Which spelling of standard SQL to emit where the two backends disagree and
/// [`Capabilities`] has no field for it.
///
/// Three differences are pure vocabulary rather than capability —
/// `substring(s FROM a FOR b)` against `substr(s, a, b)`,
/// `trim(LEADING c FROM s)` against `ltrim(s, c)`, and `now()` against
/// `CURRENT_TIMESTAMP` — and [`Capabilities`] is frozen, so they key off
/// [`Dialect::name`] instead. An unrecognised dialect gets the standard
/// spelling, gated by its capability table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    /// [`Postgres`].
    Postgres,
    /// [`Sqlite`].
    Sqlite,
    /// A third-party dialect. Standard SQL, gated by capabilities.
    Other,
}

impl Flavor {
    /// Recognises the two built-in dialects by the name they report.
    fn of(dialect: &dyn Dialect) -> Self {
        let name = dialect.name();
        if name == Postgres::NAME {
            Self::Postgres
        } else if name == Sqlite::NAME {
            Self::Sqlite
        } else {
            Self::Other
        }
    }

    /// Whether SQLite's vocabulary applies.
    const fn is_sqlite(self) -> bool {
        matches!(self, Self::Sqlite)
    }
}

/// The output buffer, the bound parameters, and everything the walk needs to
/// know about the target.
struct Renderer<'d> {
    dialect: &'d dyn Dialect,
    caps: Capabilities,
    flavor: Flavor,
    binding: Binding,
    out: String,
    args: Vec<Value>,
}

impl<'d> Renderer<'d> {
    /// A renderer targeting `dialect`, in parameter-binding mode.
    fn new(dialect: &'d dyn Dialect) -> Self {
        Self {
            dialect,
            caps: dialect.capabilities(),
            flavor: Flavor::of(dialect),
            binding: Binding::Parameter,
            out: String::with_capacity(128),
            args: Vec::new(),
        }
    }

    /// The dialect's name, for an [`Error::Unsupported`].
    fn name(&self) -> &'static str {
        self.dialect.name()
    }

    /// Builds an [`Error::Unsupported`] naming this dialect.
    fn no(&self, construct: &'static str, help: &'static str) -> Error {
        Error::unsupported(self.name(), construct, help)
    }

    /// Refuses unless `supported` holds.
    fn require(
        &self,
        supported: bool,
        construct: &'static str,
        help: &'static str,
    ) -> Result<(), Error> {
        if supported {
            Ok(())
        } else {
            Err(self.no(construct, help))
        }
    }

    // ── primitives ──────────────────────────────────────────────────────────

    /// Appends a keyword or punctuation verbatim. Never used for anything that
    /// came from outside the crate.
    fn kw(&mut self, text: &str) {
        self.out.push_str(text);
    }

    /// Appends a number.
    fn num(&mut self, value: impl core::fmt::Display) {
        // Writing to a `String` cannot fail; the `Result` exists only because
        // `fmt::Write` is generic over sinks that can.
        let _ = write!(self.out, "{value}");
    }

    /// Writes `separator` before every item but the first.
    fn sep(&mut self, first: &mut bool, separator: &str) {
        if *first {
            *first = false;
        } else {
            self.out.push_str(separator);
        }
    }

    /// Appends an identifier, quoted by the dialect.
    ///
    /// # Errors
    ///
    /// [`Error::Ident`] if the identifier is longer than the *server's* limit,
    /// which can be shorter than [`Ident::MAX_LEN`].
    fn ident(&mut self, ident: &Ident) -> Result<(), Error> {
        let max = self.dialect.max_ident_len();
        if ident.byte_len() > max {
            return Err(Error::Ident(IdentError::TooLong {
                identifier: ident.as_str().to_owned(),
                len: ident.byte_len(),
                max,
            }));
        }
        let mut quoted = String::with_capacity(ident.byte_len() + 2);
        self.dialect.quote_ident(ident, &mut quoted);
        self.out.push_str(&quoted);
        Ok(())
    }

    /// Appends `schema.table`, both quoted.
    fn table(&mut self, table: &TableRef) -> Result<(), Error> {
        if let Some(schema) = table.schema() {
            self.ident(schema)?;
            self.kw(".");
        }
        self.ident(table.name())
    }

    /// Appends `qualifier.column`, both quoted.
    fn column(&mut self, column: &ColumnRef) -> Result<(), Error> {
        if let Some(qualifier) = column.qualifier() {
            self.ident(qualifier)?;
            self.kw(".");
        }
        self.ident(column.name())
    }

    /// Appends a user-defined type name, schema-qualified when it is.
    fn type_ref(&mut self, name: &TypeRef) -> Result<(), Error> {
        if let Some(schema) = name.schema() {
            self.ident(schema)?;
            self.kw(".");
        }
        self.ident(name.name())
    }

    /// Appends the dialect's spelling of a data type.
    fn data_type(&mut self, data_type: &DataType) -> Result<(), Error> {
        let mut rendered = String::new();
        self.dialect.type_name(data_type, &mut rendered)?;
        self.out.push_str(&rendered);
        Ok(())
    }

    /// Appends a single-quoted string literal, doubling every embedded quote.
    ///
    /// This is the one escape both PostgreSQL and SQLite accept unconditionally:
    /// PostgreSQL's backslash escapes need `E'…'` and depend on
    /// `standard_conforming_strings`, and SQLite has no backslash escapes at all.
    /// Doubling needs neither.
    fn quoted_literal(&mut self, text: &str) {
        self.out.push('\'');
        for character in text.chars() {
            if character == '\'' {
                self.out.push('\'');
            }
            self.out.push(character);
        }
        self.out.push('\'');
    }

    /// Ends one statement and starts another.
    ///
    /// Only DDL uses this: a `CREATE TABLE` with comments, and a SQLite
    /// `ALTER TABLE` with more than one action, are several statements. DDL
    /// binds no parameters, so the result is safe to send over the simple query
    /// protocol.
    fn statement_break(&mut self) {
        self.kw(";\n");
    }

    // ── values ──────────────────────────────────────────────────────────────

    /// Binds a value, or writes it as a literal in DDL.
    fn value(&mut self, value: &Value) -> Result<(), Error> {
        if matches!(value.kind(), ValueKind::Array) {
            self.require(
                self.caps.arrays,
                "array parameters",
                "bind the elements one by one with `is_in(..)`, or store the list as JSON text",
            )?;
        }
        match self.binding {
            Binding::Parameter => {
                let index = self.args.len();
                let mut placeholder = String::new();
                self.dialect.placeholder(index, &mut placeholder);
                self.out.push_str(&placeholder);
                self.args.push(value.clone());
                Ok(())
            }
            Binding::Literal => self.literal(value),
        }
    }

    /// Writes a value into the statement text.
    ///
    /// Reachable only from DDL, where there is no parameter to bind to.
    #[allow(clippy::too_many_lines)]
    fn literal(&mut self, value: &Value) -> Result<(), Error> {
        match value {
            Value::Null(_) => self.kw("NULL"),
            Value::Bool(true) => self.kw("TRUE"),
            Value::Bool(false) => self.kw("FALSE"),
            Value::I8(v) => self.num(v),
            Value::I16(v) => self.num(v),
            Value::I32(v) => self.num(v),
            Value::I64(v) => self.num(v),
            Value::U8(v) => self.num(v),
            Value::U16(v) => self.num(v),
            Value::U32(v) => self.num(v),
            Value::U64(v) => self.num(v),
            Value::F32(v) => self.float_literal(f64::from(*v))?,
            Value::F64(v) => self.float_literal(*v)?,
            Value::Decimal(v) => self.num(v),
            Value::Text(v) => self.quoted_literal(v),
            Value::Bytes(v) => self.bytes_literal(v),
            Value::Uuid(v) => self.quoted_literal(&v.to_string()),
            Value::Json(v) => self.quoted_literal(v.as_json_str()),
            Value::Timestamp(v) => self.quoted_literal(&format_timestamp(*v)),
            Value::DateTime(v) => self.quoted_literal(&v.to_string()),
            Value::Date(v) => self.quoted_literal(&v.to_string()),
            Value::Time(v) => self.quoted_literal(&v.to_string()),
            Value::Interval(v) => self.quoted_literal(&v.to_string()),
            Value::Array(array) => self.array_literal(array)?,
        }
        Ok(())
    }

    /// Writes a floating-point literal, keeping the decimal point so that
    /// `1.0` does not become an `integer`.
    fn float_literal(&mut self, value: f64) -> Result<(), Error> {
        if value.is_finite() {
            // `{:?}` keeps the `.0` that `{}` drops, which is what stops the
            // server from typing the literal as an integer.
            let _ = write!(self.out, "{value:?}");
            return Ok(());
        }
        self.require(
            !self.flavor.is_sqlite(),
            "a non-finite floating-point literal",
            "SQLite has no `Infinity` or `NaN` literal; store the value as text, or use NULL",
        )?;
        let text = if value.is_nan() {
            "NaN"
        } else if value > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        };
        self.quoted_literal(text);
        Ok(())
    }

    /// Writes a byte-string literal in the dialect's syntax.
    fn bytes_literal(&mut self, bytes: &[u8]) {
        if self.flavor.is_sqlite() {
            self.kw("X'");
        } else {
            // `bytea`'s hex input format. With `standard_conforming_strings` on
            // — the default since 9.1 — the backslash is a literal backslash,
            // which is exactly what the format wants.
            self.kw("'\\x");
        }
        for byte in bytes {
            let _ = write!(self.out, "{byte:02X}");
        }
        self.kw("'");
    }

    /// Writes an array literal, `ARRAY[…]`, with an element cast when it is
    /// empty and would otherwise have no type.
    fn array_literal(&mut self, array: &Array) -> Result<(), Error> {
        self.require(
            self.caps.arrays,
            "array literals",
            "store the list as a JSON `text` column, or normalise it into its own table",
        )?;
        self.kw("ARRAY[");
        let mut first = true;
        for item in array.items() {
            self.sep(&mut first, ", ");
            self.literal(item)?;
        }
        self.kw("]");
        if array.is_empty() {
            let Some(element) = data_type_of(array.element_kind()) else {
                return Err(Error::InvalidClause {
                    clause: "an empty array literal",
                    reason: "the server cannot infer the element type of `ARRAY[]`",
                    help: "build the array with `Array::empty(kind)` for a kind other than \
                           `ValueKind::Unknown`",
                });
            };
            self.kw("::");
            self.data_type(&element)?;
            self.kw("[]");
        }
        Ok(())
    }

    // ── expressions ─────────────────────────────────────────────────────────

    /// Writes an expression, parenthesising it when the surrounding operator
    /// binds more tightly than it does.
    fn expr(&mut self, expr: &Expr, min_prec: u8) -> Result<(), Error> {
        let parenthesise = precedence(expr) < min_prec;
        if parenthesise {
            self.kw("(");
        }
        self.expr_inner(expr)?;
        if parenthesise {
            self.kw(")");
        }
        Ok(())
    }

    /// The body of [`Renderer::expr`], without the precedence parentheses.
    #[allow(clippy::too_many_lines)]
    fn expr_inner(&mut self, expr: &Expr) -> Result<(), Error> {
        match expr {
            Expr::Value(value) => self.value(value)?,
            Expr::Column(column) => self.column(column)?,
            Expr::Tuple(items) => {
                self.kw("(");
                let mut first = true;
                for item in items {
                    self.sep(&mut first, ", ");
                    self.expr(item, P_MIN)?;
                }
                self.kw(")");
            }
            Expr::Array(items) => {
                self.require(
                    self.caps.arrays,
                    "the `ARRAY[…]` constructor",
                    "store the list as a JSON `text` column, or normalise it into its own table",
                )?;
                if items.is_empty() {
                    return Err(Error::InvalidClause {
                        clause: "`ARRAY[]`",
                        reason: "the server cannot infer the element type of an empty constructor",
                        help: "bind a typed empty array instead: `Expr::value(Array::empty(kind))`",
                    });
                }
                self.kw("ARRAY[");
                let mut first = true;
                for item in items {
                    self.sep(&mut first, ", ");
                    self.expr(item, P_MIN)?;
                }
                self.kw("]");
            }
            Expr::Nested(inner) => {
                self.kw("(");
                self.expr(inner, P_MIN)?;
                self.kw(")");
            }
            Expr::Binary { lhs, op, rhs } => self.binary(lhs, *op, rhs)?,
            Expr::Unary { op, operand } => match op {
                UnOp::Not => {
                    self.kw("NOT ");
                    self.expr(operand, P_NOT + 1)?;
                }
                UnOp::Neg => {
                    self.kw("-");
                    self.expr(operand, P_UNARY)?;
                }
                UnOp::BitNot => {
                    self.kw("~");
                    self.expr(operand, P_UNARY)?;
                }
            },
            Expr::IsNull { operand, negated } => {
                self.expr(operand, P_IS + 1)?;
                self.kw(if *negated { " IS NOT NULL" } else { " IS NULL" });
            }
            Expr::Between {
                operand,
                low,
                high,
                negated,
            } => {
                self.expr(operand, P_MATCH + 1)?;
                self.kw(if *negated {
                    " NOT BETWEEN "
                } else {
                    " BETWEEN "
                });
                // The `AND` below is grammar, not an operator, so both bounds
                // are written above `AND`'s precedence to keep a boolean bound
                // from being swallowed.
                self.expr(low, P_OTHER)?;
                self.kw(" AND ");
                self.expr(high, P_OTHER)?;
            }
            Expr::Like {
                operand,
                pattern,
                case_insensitive,
                negated,
                escape,
            } => self.like(operand, pattern, *case_insensitive, *negated, *escape)?,
            Expr::InList {
                operand,
                items,
                negated,
            } => {
                if items.is_empty() {
                    // `IN ()` is a syntax error everywhere. An empty list can
                    // never match, and a negated one always can.
                    self.kw(if *negated { "TRUE" } else { "FALSE" });
                } else {
                    self.expr(operand, P_MATCH + 1)?;
                    self.kw(if *negated { " NOT IN (" } else { " IN (" });
                    let mut first = true;
                    for item in items {
                        self.sep(&mut first, ", ");
                        self.expr(item, P_MIN)?;
                    }
                    self.kw(")");
                }
            }
            Expr::InSubquery {
                operand,
                query,
                negated,
            } => {
                self.expr(operand, P_MATCH + 1)?;
                self.kw(if *negated { " NOT IN (" } else { " IN (" });
                self.select(query)?;
                self.kw(")");
            }
            Expr::Quantified {
                lhs,
                op,
                quantifier,
                rhs,
            } => {
                self.require(
                    self.caps.arrays,
                    "the `ANY` / `ALL` quantified comparison",
                    "use `is_in(..)` for a list, or `EXISTS (SELECT …)` for a subquery",
                )?;
                self.expr(lhs, P_CMP + 1)?;
                self.kw(" ");
                self.binary_operator(*op)?;
                self.kw(match quantifier {
                    Quantifier::Any => " ANY (",
                    Quantifier::All => " ALL (",
                });
                match rhs.as_ref() {
                    Expr::Scalar(query) => self.select(query)?,
                    other => self.expr(other, P_MIN)?,
                }
                self.kw(")");
            }
            Expr::Exists { query, negated } => {
                self.kw(if *negated { "NOT EXISTS (" } else { "EXISTS (" });
                self.select(query)?;
                self.kw(")");
            }
            Expr::Scalar(query) => {
                self.kw("(");
                self.select(query)?;
                self.kw(")");
            }
            Expr::Case(case) => self.case(case)?,
            Expr::Cast { operand, data_type } => {
                self.kw("CAST(");
                self.expr(operand, P_MIN)?;
                self.kw(" AS ");
                self.data_type(data_type)?;
                self.kw(")");
            }
            Expr::Function(function) => self.function(function)?,
            Expr::Aggregate(aggregate) => self.aggregate(aggregate)?,
            Expr::Window(window) => self.window(window)?,
            Expr::Json { lhs, op, rhs } => self.json(lhs, *op, rhs)?,
            Expr::Raw(raw) => self.raw_fragment(raw.fragment(), raw.args())?,
            Expr::Default => self.kw("DEFAULT"),
        }
        Ok(())
    }

    /// `lhs <op> rhs`.
    fn binary(&mut self, lhs: &Expr, op: BinOp, rhs: &Expr) -> Result<(), Error> {
        let prec = binary_precedence(op);
        self.expr(lhs, prec)?;
        self.kw(" ");
        self.binary_operator(op)?;
        self.kw(" ");
        // `prec + 1` on the right keeps left associativity visible: `a - b - c`
        // stays flat, and `a - (b - c)` keeps its parentheses.
        self.expr(rhs, prec + 1)
    }

    /// Writes an infix operator, refusing the ones this dialect does not have.
    fn binary_operator(&mut self, op: BinOp) -> Result<(), Error> {
        let text = match op {
            BinOp::Eq => "=",
            BinOp::NotEq => "<>",
            BinOp::Lt => "<",
            BinOp::LtEq => "<=",
            BinOp::Gt => ">",
            BinOp::GtEq => ">=",
            BinOp::IsDistinctFrom => {
                self.require(
                    self.caps.is_distinct_from,
                    "`IS DISTINCT FROM`",
                    "write it out: `(a <> b) OR (a IS NULL) <> (b IS NULL)`",
                )?;
                "IS DISTINCT FROM"
            }
            BinOp::IsNotDistinctFrom => {
                self.require(
                    self.caps.is_distinct_from,
                    "`IS NOT DISTINCT FROM`",
                    "write it out: `(a = b) OR (a IS NULL AND b IS NULL)`",
                )?;
                "IS NOT DISTINCT FROM"
            }
            BinOp::And => "AND",
            BinOp::Or => "OR",
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Exp => {
                self.require(
                    !self.flavor.is_sqlite(),
                    "the `^` exponentiation operator",
                    "call `power(a, b)` instead: `Function::custom(Ident::from_static(\"power\"), \
                     [a, b])`",
                )?;
                "^"
            }
            BinOp::Concat => "||",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => {
                self.require(
                    !self.flavor.is_sqlite(),
                    "a bitwise exclusive-or operator",
                    "write it out: `(a | b) - (a & b)`",
                )?;
                "#"
            }
            BinOp::ShiftLeft => "<<",
            BinOp::ShiftRight => ">>",
            BinOp::Regex
            | BinOp::RegexCaseInsensitive
            | BinOp::NotRegex
            | BinOp::NotRegexCaseInsensitive => {
                self.require(
                    !self.flavor.is_sqlite(),
                    "POSIX regular-expression operators",
                    "SQLite's `REGEXP` needs a user function on the connection; use `like`, or \
                     register one",
                )?;
                match op {
                    BinOp::Regex => "~",
                    BinOp::RegexCaseInsensitive => "~*",
                    BinOp::NotRegex => "!~",
                    _ => "!~*",
                }
            }
            BinOp::TextMatch => {
                self.require(
                    self.caps.full_text_search,
                    "full-text search",
                    "use an FTS5 virtual table, or keep full-text search on PostgreSQL only",
                )?;
                "@@"
            }
            BinOp::ArrayContains => {
                self.require(
                    self.caps.arrays,
                    "the array containment operator `@>`",
                    "store the list as JSON and use `json_each`, or normalise it into its own table",
                )?;
                "@>"
            }
            BinOp::ArrayContainedBy => {
                self.require(
                    self.caps.arrays,
                    "the array containment operator `<@`",
                    "store the list as JSON and use `json_each`, or normalise it into its own table",
                )?;
                "<@"
            }
            BinOp::ArrayOverlaps => {
                self.require(
                    self.caps.arrays,
                    "the array overlap operator `&&`",
                    "store the list as JSON and use `json_each`, or normalise it into its own table",
                )?;
                "&&"
            }
        };
        self.kw(text);
        Ok(())
    }

    /// `a LIKE b`, `a ILIKE b`, and SQLite's documented substitute for the
    /// second.
    fn like(
        &mut self,
        operand: &Expr,
        pattern: &Expr,
        case_insensitive: bool,
        negated: bool,
        escape: Option<char>,
    ) -> Result<(), Error> {
        let lower = case_insensitive && !self.caps.ilike;
        if lower {
            // The documented SQLite divergence (ADR-0010): no `ILIKE`, so both
            // sides are lowered. `lower()` is ASCII-only in SQLite, which is the
            // same fold `ILIKE` gives for a non-`C` collation on ASCII text and
            // is weaker for everything else. Said out loud rather than hidden.
            self.kw("lower(");
            self.expr(operand, P_MIN)?;
            self.kw(")");
        } else {
            self.expr(operand, P_MATCH + 1)?;
        }
        self.kw(match (case_insensitive && self.caps.ilike, negated) {
            (true, false) => " ILIKE ",
            (true, true) => " NOT ILIKE ",
            (false, false) => " LIKE ",
            (false, true) => " NOT LIKE ",
        });
        if lower {
            self.kw("lower(");
            self.expr(pattern, P_MIN)?;
            self.kw(")");
        } else {
            self.expr(pattern, P_MATCH + 1)?;
        }
        if let Some(character) = escape {
            self.kw(" ESCAPE ");
            let mut buffer = [0_u8; 4];
            let text = character.encode_utf8(&mut buffer).to_owned();
            self.quoted_literal(&text);
        }
        Ok(())
    }

    /// `CASE … WHEN … THEN … ELSE … END`.
    fn case(&mut self, case: &Case) -> Result<(), Error> {
        if case.branches().is_empty() {
            return Err(Error::InvalidClause {
                clause: "CASE",
                reason: "a CASE with no WHEN branch is a syntax error",
                help: "add at least one branch: `Case::new().when(condition, result)`",
            });
        }
        self.kw("CASE");
        if let Some(operand) = case.operand() {
            self.kw(" ");
            self.expr(operand, P_MIN)?;
        }
        for (condition, result) in case.branches() {
            self.kw(" WHEN ");
            self.expr(condition, P_MIN)?;
            self.kw(" THEN ");
            self.expr(result, P_MIN)?;
        }
        if let Some(otherwise) = case.default_result() {
            self.kw(" ELSE ");
            self.expr(otherwise, P_MIN)?;
        }
        self.kw(" END");
        Ok(())
    }

    /// A scalar function call.
    #[allow(clippy::too_many_lines)]
    fn function(&mut self, function: &Function) -> Result<(), Error> {
        match function {
            Function::Coalesce(args) => self.call("coalesce", args, 1)?,
            Function::NullIf(lhs, rhs) => {
                self.kw("nullif(");
                self.expr(lhs, P_MIN)?;
                self.kw(", ");
                self.expr(rhs, P_MIN)?;
                self.kw(")");
            }
            Function::Greatest(args) => {
                let name = if self.flavor.is_sqlite() {
                    "max"
                } else {
                    "greatest"
                };
                self.variadic_extremum(name, args)?;
            }
            Function::Least(args) => {
                let name = if self.flavor.is_sqlite() {
                    "min"
                } else {
                    "least"
                };
                self.variadic_extremum(name, args)?;
            }
            Function::Abs(operand) => self.call1("abs", operand)?,
            Function::Round { operand, decimals } => {
                self.kw("round(");
                self.expr(operand, P_MIN)?;
                if let Some(decimals) = decimals {
                    self.kw(", ");
                    self.expr(decimals, P_MIN)?;
                }
                self.kw(")");
            }
            Function::Floor(operand) => self.call1("floor", operand)?,
            Function::Ceil(operand) => self.call1("ceil", operand)?,
            Function::Lower(operand) => self.call1("lower", operand)?,
            Function::Upper(operand) => self.call1("upper", operand)?,
            Function::Length(operand) => self.call1("length", operand)?,
            Function::Trim {
                operand,
                mode,
                characters,
            } => self.trim(operand, *mode, characters.as_deref())?,
            Function::Substring {
                operand,
                from,
                length,
            } => self.substring(operand, from.as_deref(), length.as_deref())?,
            Function::Replace { operand, from, to } => {
                self.kw("replace(");
                self.expr(operand, P_MIN)?;
                self.kw(", ");
                self.expr(from, P_MIN)?;
                self.kw(", ");
                self.expr(to, P_MIN)?;
                self.kw(")");
            }
            Function::Concat(items) => self.call("concat", items, 1)?,
            Function::ConcatWs { separator, items } => {
                self.kw("concat_ws(");
                self.expr(separator, P_MIN)?;
                for item in items {
                    self.kw(", ");
                    self.expr(item, P_MIN)?;
                }
                self.kw(")");
            }
            Function::Now => {
                // SQLite has no `now()`; `CURRENT_TIMESTAMP` is the portable
                // spelling and is what both servers evaluate at statement start.
                if self.flavor.is_sqlite() {
                    self.kw("CURRENT_TIMESTAMP");
                } else {
                    self.kw("now()");
                }
            }
            Function::CurrentDate => self.kw("CURRENT_DATE"),
            Function::CurrentTime => self.kw("CURRENT_TIME"),
            Function::CurrentTimestamp => self.kw("CURRENT_TIMESTAMP"),
            Function::Random => self.kw("random()"),
            Function::ToTsVector { config, document } => {
                self.require(
                    self.caps.full_text_search,
                    "`to_tsvector`",
                    "use an FTS5 virtual table, or keep full-text search on PostgreSQL only",
                )?;
                self.kw("to_tsvector(");
                if let Some(config) = config {
                    self.text_search_config(config);
                    self.kw(", ");
                }
                self.expr(document, P_MIN)?;
                self.kw(")");
            }
            Function::ToTsQuery { config, query } => {
                self.require(
                    self.caps.full_text_search,
                    "`to_tsquery`",
                    "use an FTS5 virtual table, or keep full-text search on PostgreSQL only",
                )?;
                self.kw(match query {
                    TextQuery::Plain(_) => "plainto_tsquery(",
                    TextQuery::Phrase(_) => "phraseto_tsquery(",
                    TextQuery::Websearch(_) => "websearch_to_tsquery(",
                    TextQuery::Tsquery(_) => "to_tsquery(",
                });
                if let Some(config) = config {
                    self.text_search_config(config);
                    self.kw(", ");
                }
                // The user's search box is always a parameter, never syntax.
                self.value(&Value::Text(query.text().to_owned()))?;
                self.kw(")");
            }
            Function::TsRank {
                vector,
                query,
                normalization,
            } => {
                self.require(
                    self.caps.full_text_search,
                    "`ts_rank`",
                    "use an FTS5 virtual table's `rank`, or keep ranking on PostgreSQL only",
                )?;
                self.kw("ts_rank(");
                self.expr(vector, P_MIN)?;
                self.kw(", ");
                self.expr(query, P_MIN)?;
                if let Some(normalization) = normalization {
                    self.kw(", ");
                    self.value(&Value::I32(*normalization))?;
                }
                self.kw(")");
            }
            Function::TsHeadline {
                config,
                document,
                query,
                options,
            } => {
                self.require(
                    self.caps.full_text_search,
                    "`ts_headline`",
                    "use FTS5's `snippet()`, or keep highlighting on PostgreSQL only",
                )?;
                self.kw("ts_headline(");
                if let Some(config) = config {
                    self.text_search_config(config);
                    self.kw(", ");
                }
                self.expr(document, P_MIN)?;
                self.kw(", ");
                self.expr(query, P_MIN)?;
                if let Some(options) = options {
                    self.kw(", ");
                    self.value(&Value::Text(options.clone()))?;
                }
                self.kw(")");
            }
            Function::Custom { name, args } => {
                self.ident(name)?;
                self.kw("(");
                let mut first = true;
                for arg in args {
                    self.sep(&mut first, ", ");
                    self.expr(arg, P_MIN)?;
                }
                self.kw(")");
            }
        }
        Ok(())
    }

    /// A text-search configuration is a `regconfig`, which is written as a
    /// string literal rather than as an identifier.
    fn text_search_config(&mut self, config: &Ident) {
        let text = config.as_str().to_owned();
        self.quoted_literal(&text);
    }

    /// `name(a, b, …)`, refusing fewer than `minimum` arguments.
    fn call(&mut self, name: &'static str, args: &[Expr], minimum: usize) -> Result<(), Error> {
        if args.len() < minimum {
            return Err(Error::InvalidClause {
                clause: name,
                reason: "the function was called with too few arguments",
                help: "pass at least one argument; an empty call is a syntax error",
            });
        }
        self.kw(name);
        self.kw("(");
        let mut first = true;
        for arg in args {
            self.sep(&mut first, ", ");
            self.expr(arg, P_MIN)?;
        }
        self.kw(")");
        Ok(())
    }

    /// `name(x)`.
    fn call1(&mut self, name: &'static str, arg: &Expr) -> Result<(), Error> {
        self.kw(name);
        self.kw("(");
        self.expr(arg, P_MIN)?;
        self.kw(")");
        Ok(())
    }

    /// `greatest`/`least`, whose SQLite spelling (`max`/`min`) is the *aggregate*
    /// when it is given one argument, so a one-argument call renders as the
    /// argument itself.
    fn variadic_extremum(&mut self, name: &'static str, args: &[Expr]) -> Result<(), Error> {
        match args {
            [] => Err(Error::InvalidClause {
                clause: name,
                reason: "the function was called with no arguments",
                help: "pass at least two values, or drop the call",
            }),
            [only] if self.flavor.is_sqlite() => self.expr(only, P_ATOM),
            _ => self.call(name, args, 1),
        }
    }

    /// `trim`, in each dialect's grammar.
    fn trim(
        &mut self,
        operand: &Expr,
        mode: TrimMode,
        characters: Option<&Expr>,
    ) -> Result<(), Error> {
        if self.flavor.is_sqlite() {
            // SQLite has no `trim(LEADING … FROM …)` grammar; it has three
            // functions instead, with the same meaning.
            self.kw(match mode {
                TrimMode::Both => "trim(",
                TrimMode::Leading => "ltrim(",
                TrimMode::Trailing => "rtrim(",
            });
            self.expr(operand, P_MIN)?;
            if let Some(characters) = characters {
                self.kw(", ");
                self.expr(characters, P_MIN)?;
            }
            self.kw(")");
            return Ok(());
        }
        self.kw("trim(");
        self.kw(match mode {
            TrimMode::Both => "BOTH ",
            TrimMode::Leading => "LEADING ",
            TrimMode::Trailing => "TRAILING ",
        });
        if let Some(characters) = characters {
            self.expr(characters, P_MIN)?;
            self.kw(" ");
        }
        self.kw("FROM ");
        self.expr(operand, P_MIN)?;
        self.kw(")");
        Ok(())
    }

    /// `substring`, in each dialect's grammar.
    fn substring(
        &mut self,
        operand: &Expr,
        from: Option<&Expr>,
        length: Option<&Expr>,
    ) -> Result<(), Error> {
        if self.flavor.is_sqlite() {
            self.kw("substr(");
            self.expr(operand, P_MIN)?;
            self.kw(", ");
            // SQLite's `substr` has no "from the beginning" form, so an absent
            // start becomes the explicit `1` the SQL standard implies.
            match from {
                Some(from) => self.expr(from, P_MIN)?,
                None => self.kw("1"),
            }
            if let Some(length) = length {
                self.kw(", ");
                self.expr(length, P_MIN)?;
            }
            self.kw(")");
            return Ok(());
        }
        self.kw("substring(");
        self.expr(operand, P_MIN)?;
        if let Some(from) = from {
            self.kw(" FROM ");
            self.expr(from, P_MIN)?;
        }
        if let Some(length) = length {
            self.kw(" FOR ");
            self.expr(length, P_MIN)?;
        }
        self.kw(")");
        Ok(())
    }

    /// An aggregate call, with `DISTINCT`, an internal `ORDER BY` and
    /// `FILTER (WHERE …)`.
    fn aggregate(&mut self, aggregate: &Aggregate) -> Result<(), Error> {
        self.aggregate_name(&aggregate.func())?;
        self.kw("(");
        if aggregate.is_distinct() {
            self.kw("DISTINCT ");
        }
        if aggregate.is_star() {
            self.kw("*");
        } else {
            let mut first = true;
            for arg in aggregate.args() {
                self.sep(&mut first, ", ");
                self.expr(arg, P_MIN)?;
            }
        }
        if !aggregate.order_terms().is_empty() {
            self.kw(" ORDER BY ");
            self.order_terms(aggregate.order_terms())?;
        }
        self.kw(")");
        if let Some(filter) = aggregate.filter_expr() {
            self.require(
                self.caps.aggregate_filter,
                "`FILTER (WHERE …)` on an aggregate",
                "move the condition into a `CASE` inside the aggregate's argument",
            )?;
            self.kw(" FILTER (WHERE ");
            self.expr(filter, P_MIN)?;
            self.kw(")");
        }
        Ok(())
    }

    /// The dialect's name for an aggregate.
    fn aggregate_name(&mut self, func: &AggregateFunc) -> Result<(), Error> {
        let sqlite = self.flavor.is_sqlite();
        let name = match func {
            AggregateFunc::Count => "count",
            AggregateFunc::Sum => "sum",
            AggregateFunc::Avg => "avg",
            AggregateFunc::Min => "min",
            AggregateFunc::Max => "max",
            AggregateFunc::ArrayAgg => {
                if sqlite {
                    return Err(self.no(
                        "`array_agg`",
                        "collect the rows as JSON with `AggregateFunc::JsonAgg`, which SQLite \
                         spells `json_group_array`",
                    ));
                }
                "array_agg"
            }
            AggregateFunc::StringAgg => {
                if sqlite {
                    "group_concat"
                } else {
                    "string_agg"
                }
            }
            AggregateFunc::JsonAgg | AggregateFunc::JsonbAgg => {
                if sqlite {
                    "json_group_array"
                } else if matches!(func, AggregateFunc::JsonAgg) {
                    "json_agg"
                } else {
                    "jsonb_agg"
                }
            }
            AggregateFunc::JsonObjectAgg | AggregateFunc::JsonbObjectAgg => {
                if sqlite {
                    "json_group_object"
                } else if matches!(func, AggregateFunc::JsonObjectAgg) {
                    "json_object_agg"
                } else {
                    "jsonb_object_agg"
                }
            }
            AggregateFunc::BoolAnd => {
                if sqlite {
                    return Err(self.no(
                        "`bool_and`",
                        "SQLite stores booleans as integers: use `min(flag) = 1`",
                    ));
                }
                "bool_and"
            }
            AggregateFunc::BoolOr => {
                if sqlite {
                    return Err(self.no(
                        "`bool_or`",
                        "SQLite stores booleans as integers: use `max(flag) = 1`",
                    ));
                }
                "bool_or"
            }
            AggregateFunc::StdDev => {
                if sqlite {
                    return Err(self.no(
                        "`stddev`",
                        "load SQLite's `extension-functions` module, or compute the deviation in \
                         Rust from `count`, `sum` and `sum(x * x)`",
                    ));
                }
                "stddev"
            }
            AggregateFunc::Variance => {
                if sqlite {
                    return Err(self.no(
                        "`variance`",
                        "load SQLite's `extension-functions` module, or compute it in Rust from \
                         `count`, `sum` and `sum(x * x)`",
                    ));
                }
                "variance"
            }
            AggregateFunc::Custom(name) => return self.ident(name),
        };
        self.kw(name);
        Ok(())
    }

    /// `f(…) OVER (…)`.
    fn window(&mut self, window: &WindowExpr) -> Result<(), Error> {
        self.require(
            self.caps.window_functions,
            "window functions",
            "rewrite the query with a correlated subquery, or a `GROUP BY` and a join",
        )?;
        match window.func() {
            WindowFunc::Aggregate(aggregate) => self.aggregate(aggregate)?,
            other => {
                let name = match other {
                    WindowFunc::RowNumber => "row_number",
                    WindowFunc::Rank => "rank",
                    WindowFunc::DenseRank => "dense_rank",
                    WindowFunc::PercentRank => "percent_rank",
                    WindowFunc::CumeDist => "cume_dist",
                    WindowFunc::Ntile => "ntile",
                    WindowFunc::Lag => "lag",
                    WindowFunc::Lead => "lead",
                    WindowFunc::FirstValue => "first_value",
                    WindowFunc::LastValue => "last_value",
                    WindowFunc::NthValue => "nth_value",
                    WindowFunc::Custom(name) => {
                        self.ident(name)?;
                        ""
                    }
                    WindowFunc::Aggregate(_) => unreachable!("handled above"),
                };
                self.kw(name);
                self.kw("(");
                let mut first = true;
                for arg in window.args() {
                    self.sep(&mut first, ", ");
                    self.expr(arg, P_MIN)?;
                }
                self.kw(")");
            }
        }
        self.kw(" OVER ");
        match window.window() {
            WindowRef::Named(name) => self.ident(name)?,
            WindowRef::Spec(spec) => {
                self.kw("(");
                self.window_spec(spec)?;
                self.kw(")");
            }
        }
        Ok(())
    }

    /// The inside of an `OVER (…)` or of a `WINDOW name AS (…)`.
    fn window_spec(&mut self, spec: &WindowSpec) -> Result<(), Error> {
        let mut wrote = false;
        if !spec.partitions().is_empty() {
            self.kw("PARTITION BY ");
            let mut first = true;
            for partition in spec.partitions() {
                self.sep(&mut first, ", ");
                self.expr(partition, P_MIN)?;
            }
            wrote = true;
        }
        if !spec.order_terms().is_empty() {
            if wrote {
                self.kw(" ");
            }
            self.kw("ORDER BY ");
            self.order_terms(spec.order_terms())?;
            wrote = true;
        }
        if let Some(frame) = spec.frame_spec() {
            if wrote {
                self.kw(" ");
            }
            self.frame(frame)?;
        }
        Ok(())
    }

    /// A window frame.
    fn frame(&mut self, frame: &Frame) -> Result<(), Error> {
        if matches!(frame.units(), FrameUnits::Groups) || frame.exclusion().is_some() {
            self.require(
                self.caps.advanced_window_frames,
                "`GROUPS` frames and `EXCLUDE` on a window frame",
                "use a `ROWS` or `RANGE` frame without an `EXCLUDE` clause",
            )?;
        }
        self.kw(match frame.units() {
            FrameUnits::Rows => "ROWS ",
            FrameUnits::Range => "RANGE ",
            FrameUnits::Groups => "GROUPS ",
        });
        match frame.end() {
            None => {
                if matches!(frame.start(), FrameBound::UnboundedFollowing) {
                    return Err(Error::InvalidClause {
                        clause: "the window frame",
                        reason: "a frame that starts at UNBOUNDED FOLLOWING contains no rows",
                        help: "give the frame an end bound, or start it at UNBOUNDED PRECEDING",
                    });
                }
                self.frame_bound(frame.start());
            }
            Some(end) => {
                if matches!(frame.start(), FrameBound::UnboundedFollowing)
                    || matches!(end, FrameBound::UnboundedPreceding)
                {
                    return Err(Error::InvalidClause {
                        clause: "the window frame",
                        reason: "the start bound comes after the end bound",
                        help: "order them: `Frame::new(units, start).to(end)` with `start` no \
                               later than `end`",
                    });
                }
                self.kw("BETWEEN ");
                self.frame_bound(frame.start());
                self.kw(" AND ");
                self.frame_bound(end);
            }
        }
        if let Some(exclusion) = frame.exclusion() {
            self.kw(match exclusion {
                FrameExclusion::CurrentRow => " EXCLUDE CURRENT ROW",
                FrameExclusion::Group => " EXCLUDE GROUP",
                FrameExclusion::Ties => " EXCLUDE TIES",
                FrameExclusion::NoOthers => " EXCLUDE NO OTHERS",
            });
        }
        Ok(())
    }

    /// One end of a window frame.
    fn frame_bound(&mut self, bound: &FrameBound) {
        match bound {
            FrameBound::UnboundedPreceding => self.kw("UNBOUNDED PRECEDING"),
            FrameBound::Preceding(offset) => {
                self.num(offset);
                self.kw(" PRECEDING");
            }
            FrameBound::CurrentRow => self.kw("CURRENT ROW"),
            FrameBound::Following(offset) => {
                self.num(offset);
                self.kw(" FOLLOWING");
            }
            FrameBound::UnboundedFollowing => self.kw("UNBOUNDED FOLLOWING"),
        }
    }

    /// A `jsonb` operator.
    fn json(&mut self, lhs: &Expr, op: JsonOp, rhs: &Expr) -> Result<(), Error> {
        let operator = if self.caps.jsonb {
            match op {
                JsonOp::Get => "->",
                JsonOp::GetText => "->>",
                JsonOp::GetPath => "#>",
                JsonOp::GetPathText => "#>>",
                JsonOp::Contains => "@>",
                JsonOp::ContainedBy => "<@",
                JsonOp::HasKey => "?",
                JsonOp::HasAnyKey => "?|",
                JsonOp::HasAllKeys => "?&",
                JsonOp::Concat => "||",
                JsonOp::Remove => "-",
                JsonOp::RemovePath => "#-",
            }
        } else {
            // SQLite has `->` and `->>` (3.38+), which are `json_extract` with
            // the same key-or-index abbreviation PostgreSQL's operators use.
            // Everything else has no equivalent that means the same thing.
            match op {
                JsonOp::Get => "->",
                JsonOp::GetText => "->>",
                JsonOp::GetPath | JsonOp::GetPathText => {
                    return Err(self.no(
                        "the `jsonb` path operators `#>` and `#>>`",
                        "chain `get(..)` one key at a time, or call `json_extract(doc, '$.a.b')` \
                         through `Function::custom`",
                    ));
                }
                JsonOp::Contains | JsonOp::ContainedBy => {
                    return Err(self.no(
                        "the `jsonb` containment operators `@>` and `<@`",
                        "compare the extracted keys, or keep containment queries on PostgreSQL",
                    ));
                }
                JsonOp::HasKey | JsonOp::HasAnyKey | JsonOp::HasAllKeys => {
                    return Err(self.no(
                        "the `jsonb` key-existence operators `?`, `?|` and `?&`",
                        "test the extracted value instead: `get_text(key).is_not_null()`",
                    ));
                }
                JsonOp::Concat => {
                    return Err(self.no(
                        "`jsonb` concatenation",
                        "build the merged document in Rust and bind it, or call `json_patch` \
                         through `Function::custom`",
                    ));
                }
                JsonOp::Remove | JsonOp::RemovePath => {
                    return Err(self.no(
                        "`jsonb` key removal",
                        "call `json_remove(doc, '$.key')` through `Function::custom`",
                    ));
                }
            }
        };
        self.expr(lhs, P_OTHER)?;
        self.kw(" ");
        self.kw(operator);
        self.kw(" ");
        self.expr(rhs, P_OTHER + 1)
    }

    /// A raw fragment: everything outside a placeholder is emitted verbatim,
    /// `??` collapses to a literal question mark, and `?` becomes the dialect's
    /// placeholder with the statement's running parameter number.
    fn raw_fragment(&mut self, fragment: &str, args: &[Value]) -> Result<(), Error> {
        let expected = RawExpr::new(fragment.to_owned()).placeholder_count();
        if expected != args.len() {
            return Err(Error::RawArity {
                fragment: fragment.to_owned(),
                expected,
                found: args.len(),
            });
        }
        let bytes = fragment.as_bytes();
        let mut copied_to = 0;
        let mut index = 0;
        let mut next_arg = 0;
        while index < bytes.len() {
            if bytes[index] != b'?' {
                index += 1;
                continue;
            }
            // `?` is ASCII, so slicing at its offset is always on a character
            // boundary.
            self.out.push_str(&fragment[copied_to..index]);
            if bytes.get(index + 1) == Some(&b'?') {
                self.out.push('?');
                index += 2;
            } else {
                self.value(&args[next_arg])?;
                next_arg += 1;
                index += 1;
            }
            copied_to = index;
        }
        self.out.push_str(&fragment[copied_to..]);
        Ok(())
    }

    // ── SELECT ──────────────────────────────────────────────────────────────

    /// A whole `SELECT`, including its `WITH`, set operations, ordering, limit
    /// and lock.
    fn select(&mut self, select: &Select) -> Result<(), Error> {
        self.with_clause(select.ctes(), select.is_recursive())?;
        self.select_core(select)?;

        for (op, other) in select.set_operations() {
            self.set_operator(*op)?;
            if other.limit_value().is_some()
                || other.offset_value().is_some()
                || !other.order_terms().is_empty()
                || other.lock_mode().is_some()
            {
                return Err(Error::InvalidClause {
                    clause: "ORDER BY / LIMIT on a set-operation operand",
                    reason: "they belong to the whole compound query, not to one branch of it",
                    help: "move `.order_by(..)` and `.limit(..)` onto the query you call \
                           `.union(..)` on, or wrap the branch in a CTE",
                });
            }
            self.with_clause(other.ctes(), other.is_recursive())?;
            self.select_core(other)?;
        }

        if !select.order_terms().is_empty() {
            self.kw(" ORDER BY ");
            self.order_terms(select.order_terms())?;
        }
        self.limit_offset(select.limit_value(), select.offset_value());
        if let Some(lock) = select.lock_mode() {
            self.lock(select, lock)?;
        }
        Ok(())
    }

    /// `SELECT … FROM … WHERE … GROUP BY … HAVING … WINDOW …`: the part of a
    /// query that a set operation combines.
    fn select_core(&mut self, select: &Select) -> Result<(), Error> {
        if select.items().is_empty() {
            return Err(Error::incomplete(
                "SELECT",
                "a projection",
                "call `.select_all()`, `.select_column(..)` or `.select_expr(..)`",
            ));
        }
        self.kw("SELECT");
        match select.distinct_mode() {
            Distinct::All => {}
            Distinct::Distinct => self.kw(" DISTINCT"),
            Distinct::On(exprs) => {
                self.require(
                    self.caps.distinct_on,
                    "`DISTINCT ON (…)`",
                    "use `row_number() OVER (PARTITION BY …)` in a subquery and keep the first row",
                )?;
                if exprs.is_empty() {
                    return Err(Error::InvalidClause {
                        clause: "DISTINCT ON",
                        reason: "the expression list is empty",
                        help: "pass the columns that identify a group: `.distinct_on([..])`",
                    });
                }
                self.kw(" DISTINCT ON (");
                let mut first = true;
                for expr in exprs {
                    self.sep(&mut first, ", ");
                    self.expr(expr, P_MIN)?;
                }
                self.kw(")");
            }
        }
        self.kw(" ");
        let mut first = true;
        for item in select.items() {
            self.sep(&mut first, ", ");
            self.select_item(item)?;
        }

        if !select.from_items().is_empty() {
            self.kw(" FROM ");
            let mut first = true;
            for item in select.from_items() {
                self.sep(&mut first, ", ");
                self.source(item)?;
            }
        }
        for join in select.joins() {
            self.join(join)?;
        }
        self.where_clause(select.filters())?;
        if !select.group_by_exprs().is_empty() {
            self.kw(" GROUP BY ");
            let mut first = true;
            for expr in select.group_by_exprs() {
                self.sep(&mut first, ", ");
                self.expr(expr, P_MIN)?;
            }
        }
        if !select.having_exprs().is_empty() {
            self.kw(" HAVING ");
            let mut first = true;
            for expr in select.having_exprs() {
                self.sep(&mut first, " AND ");
                self.expr(expr, P_AND)?;
            }
        }
        if !select.windows().is_empty() {
            self.require(
                self.caps.window_functions,
                "the `WINDOW` clause",
                "rewrite the query with a correlated subquery, or a `GROUP BY` and a join",
            )?;
            self.kw(" WINDOW ");
            let mut first = true;
            for (name, spec) in select.windows() {
                self.sep(&mut first, ", ");
                self.ident(name)?;
                self.kw(" AS (");
                self.window_spec(spec)?;
                self.kw(")");
            }
        }
        Ok(())
    }

    /// One item of a `SELECT` list.
    fn select_item(&mut self, item: &SelectItem) -> Result<(), Error> {
        match item {
            SelectItem::All => self.kw("*"),
            SelectItem::AllFrom(qualifier) => {
                self.ident(qualifier)?;
                self.kw(".*");
            }
            SelectItem::Expr { expr, alias } => {
                self.expr(expr, P_MIN)?;
                if let Some(alias) = alias {
                    self.kw(" AS ");
                    self.ident(alias)?;
                }
            }
        }
        Ok(())
    }

    /// One item of a `FROM` clause.
    fn source(&mut self, item: &FromItem) -> Result<(), Error> {
        match item {
            FromItem::Table { table, alias, only } => {
                if *only {
                    self.require(
                        self.caps.partitioning,
                        "`FROM ONLY`",
                        "SQLite has neither inheritance nor partitions, so there is nothing for \
                         `ONLY` to exclude; drop it",
                    )?;
                    self.kw("ONLY ");
                }
                self.table(table)?;
                if let Some(alias) = alias {
                    self.kw(" AS ");
                    self.ident(alias)?;
                }
            }
            FromItem::Subquery {
                query,
                alias,
                lateral,
            } => {
                if *lateral {
                    self.require(
                        self.caps.lateral_joins,
                        "`LATERAL` subqueries",
                        "use a correlated scalar subquery in the projection, or a window function \
                         in a plain subquery",
                    )?;
                    self.kw("LATERAL ");
                }
                self.kw("(");
                self.select(query)?;
                self.kw(") AS ");
                self.ident(alias)?;
            }
            FromItem::Values {
                rows,
                alias,
                columns,
            } => self.values_item(rows, alias, columns)?,
            FromItem::Function {
                function,
                alias,
                lateral,
                with_ordinality,
            } => {
                if *lateral {
                    self.require(
                        self.caps.lateral_joins,
                        "`LATERAL` function calls in a FROM clause",
                        "call the function in the projection instead",
                    )?;
                    self.kw("LATERAL ");
                }
                self.function(function)?;
                if *with_ordinality {
                    self.require(
                        self.caps.arrays,
                        "`WITH ORDINALITY`",
                        "add the row number with `row_number() OVER ()` in an enclosing query",
                    )?;
                    self.kw(" WITH ORDINALITY");
                }
                if let Some(alias) = alias {
                    self.kw(" AS ");
                    self.ident(alias)?;
                }
            }
        }
        Ok(())
    }

    /// `(VALUES (…), (…)) AS alias(col, col)`.
    fn values_item(
        &mut self,
        rows: &[Vec<Expr>],
        alias: &Ident,
        columns: &[Ident],
    ) -> Result<(), Error> {
        if rows.is_empty() {
            return Err(Error::incomplete(
                "VALUES",
                "any rows",
                "pass at least one row to `FromItem::values(..)`",
            ));
        }
        let width = rows[0].len();
        for (index, row) in rows.iter().enumerate() {
            if row.len() != width {
                return Err(Error::RowArity {
                    row: index,
                    expected: width,
                    found: row.len(),
                });
            }
        }
        if !columns.is_empty() && columns.len() != width {
            return Err(Error::RowArity {
                row: 0,
                expected: columns.len(),
                found: width,
            });
        }
        self.kw("(VALUES ");
        let mut first = true;
        for row in rows {
            self.sep(&mut first, ", ");
            self.kw("(");
            let mut first_value = true;
            for value in row {
                self.sep(&mut first_value, ", ");
                self.expr(value, P_MIN)?;
            }
            self.kw(")");
        }
        self.kw(") AS ");
        self.ident(alias)?;
        if !columns.is_empty() {
            if self.flavor.is_sqlite() {
                return Err(self.no(
                    "column names on a `VALUES` table alias",
                    "name the columns in a CTE instead: `WITH v(a, b) AS (VALUES (..)) SELECT …`",
                ));
            }
            self.kw("(");
            let mut first = true;
            for column in columns {
                self.sep(&mut first, ", ");
                self.ident(column)?;
            }
            self.kw(")");
        }
        Ok(())
    }

    /// One join.
    fn join(&mut self, join: &Join) -> Result<(), Error> {
        match join.kind() {
            JoinKind::Right => self.require(
                self.caps.right_join,
                "`RIGHT JOIN`",
                "swap the two tables and use a `LEFT JOIN`",
            )?,
            JoinKind::Full => self.require(
                self.caps.full_join,
                "`FULL OUTER JOIN`",
                "union a `LEFT JOIN` with the anti-join of the other side",
            )?,
            _ => {}
        }
        if matches!(join.condition(), JoinCondition::Natural) {
            self.kw(" NATURAL");
        }
        self.kw(match join.kind() {
            JoinKind::Inner => " INNER JOIN ",
            JoinKind::Left => " LEFT JOIN ",
            JoinKind::Right => " RIGHT JOIN ",
            JoinKind::Full => " FULL JOIN ",
            JoinKind::Cross => " CROSS JOIN ",
        });
        self.source(join.source())?;
        match join.condition() {
            JoinCondition::On(condition) => {
                self.kw(" ON ");
                self.expr(condition, P_MIN)?;
            }
            JoinCondition::Using(columns) => {
                if columns.is_empty() {
                    return Err(Error::incomplete(
                        "JOIN",
                        "any columns for its USING clause",
                        "pass the shared column names to `Join::using(..)`",
                    ));
                }
                self.kw(" USING (");
                let mut first = true;
                for column in columns {
                    self.sep(&mut first, ", ");
                    self.ident(column)?;
                }
                self.kw(")");
            }
            JoinCondition::Natural | JoinCondition::None => {}
        }
        Ok(())
    }

    /// `WHERE a AND b AND c`, or nothing.
    fn where_clause(&mut self, filters: &[Expr]) -> Result<(), Error> {
        if filters.is_empty() {
            return Ok(());
        }
        self.kw(" WHERE ");
        let mut first = true;
        for filter in filters {
            self.sep(&mut first, " AND ");
            // `P_AND` puts parentheses around any `OR` a caller pushed in —
            // the difference between "admin and (active or trial)" and a query
            // that returns every trial account — while leaving a filter that is
            // itself an `AND` flat, because `AND` is associative.
            self.expr(filter, P_AND)?;
        }
        Ok(())
    }

    /// A comma-separated `ORDER BY` list.
    fn order_terms(&mut self, terms: &[OrderTerm]) -> Result<(), Error> {
        let mut first = true;
        for term in terms {
            self.sep(&mut first, ", ");
            self.expr(term.expr(), P_MIN)?;
            self.kw(match term.order() {
                Order::Asc => " ASC",
                Order::Desc => " DESC",
            });
            if let Some(nulls) = term.nulls() {
                self.require(
                    self.caps.nulls_ordering,
                    "`NULLS FIRST` / `NULLS LAST`",
                    "sort on `x IS NULL` first, then on `x`",
                )?;
                self.kw(match nulls {
                    Nulls::First => " NULLS FIRST",
                    Nulls::Last => " NULLS LAST",
                });
            }
        }
        Ok(())
    }

    /// `LIMIT n OFFSET m`, with SQLite's requirement that an `OFFSET` be
    /// preceded by a `LIMIT`.
    fn limit_offset(&mut self, limit: Option<u64>, offset: Option<u64>) {
        match (limit, offset) {
            (None, None) => {}
            (Some(limit), _) => {
                self.kw(" LIMIT ");
                self.num(limit);
            }
            (None, Some(_)) if self.flavor.is_sqlite() => {
                // SQLite's grammar has no bare `OFFSET`; `-1` is its documented
                // spelling for "no limit".
                self.kw(" LIMIT -1");
            }
            (None, Some(_)) => {}
        }
        if let Some(offset) = offset {
            self.kw(" OFFSET ");
            self.num(offset);
        }
    }

    /// `FOR UPDATE OF … SKIP LOCKED`.
    fn lock(&mut self, select: &Select, lock: &Lock) -> Result<(), Error> {
        self.require(
            self.caps.row_locks,
            "row-level locks (`FOR UPDATE` and friends)",
            "SQLite locks the whole database file for a write transaction, so the lock is already \
             held; drop the clause",
        )?;
        if !select.group_by_exprs().is_empty()
            || !matches!(select.distinct_mode(), Distinct::All)
            || !select.set_operations().is_empty()
        {
            return Err(Error::InvalidClause {
                clause: "FOR UPDATE",
                reason: "the query groups, deduplicates or combines rows, so there is no single \
                         row to lock",
                help: "lock the rows in a separate statement: select the keys, then \
                       `SELECT … WHERE id = ANY(..) FOR UPDATE`",
            });
        }
        self.kw(match lock.strength() {
            LockStrength::Update => " FOR UPDATE",
            LockStrength::NoKeyUpdate => " FOR NO KEY UPDATE",
            LockStrength::Share => " FOR SHARE",
            LockStrength::KeyShare => " FOR KEY SHARE",
        });
        if !lock.tables().is_empty() {
            self.kw(" OF ");
            let mut first = true;
            for table in lock.tables() {
                self.sep(&mut first, ", ");
                self.table(table)?;
            }
        }
        match lock.behavior() {
            LockBehavior::Wait => {}
            LockBehavior::SkipLocked => {
                self.require(
                    self.caps.skip_locked,
                    "`SKIP LOCKED`",
                    "claim the row with a conditional `UPDATE … WHERE claimed_at IS NULL \
                     RETURNING …` instead",
                )?;
                self.kw(" SKIP LOCKED");
            }
            LockBehavior::NoWait => {
                self.require(
                    self.caps.nowait,
                    "`NOWAIT`",
                    "set a statement timeout on the connection instead",
                )?;
                self.kw(" NOWAIT");
            }
        }
        Ok(())
    }

    /// The keyword between two branches of a compound query.
    fn set_operator(&mut self, op: SetOp) -> Result<(), Error> {
        if matches!(op, SetOp::IntersectAll | SetOp::ExceptAll) && self.flavor.is_sqlite() {
            return Err(self.no(
                "`INTERSECT ALL` and `EXCEPT ALL`",
                "use `INTERSECT` or `EXCEPT`, which deduplicate, or count the duplicates with a \
                 `GROUP BY`",
            ));
        }
        self.kw(match op {
            SetOp::Union => " UNION ",
            SetOp::UnionAll => " UNION ALL ",
            SetOp::Intersect => " INTERSECT ",
            SetOp::IntersectAll => " INTERSECT ALL ",
            SetOp::Except => " EXCEPT ",
            SetOp::ExceptAll => " EXCEPT ALL ",
        });
        Ok(())
    }

    /// `WITH [RECURSIVE] name AS (…), …`.
    fn with_clause(&mut self, ctes: &[Cte], recursive: bool) -> Result<(), Error> {
        if ctes.is_empty() {
            return Ok(());
        }
        self.require(
            self.caps.ctes,
            "common table expressions",
            "inline the subquery into the `FROM` clause",
        )?;
        self.kw("WITH ");
        if recursive {
            self.require(
                self.caps.recursive_ctes,
                "`WITH RECURSIVE`",
                "walk the hierarchy in the application, one level per query",
            )?;
            self.kw("RECURSIVE ");
        }
        let mut first = true;
        for cte in ctes {
            self.sep(&mut first, ", ");
            self.cte(cte)?;
        }
        self.kw(" ");
        Ok(())
    }

    /// One common table expression.
    fn cte(&mut self, cte: &Cte) -> Result<(), Error> {
        self.ident(cte.name())?;
        if !cte.column_names().is_empty() {
            self.kw("(");
            let mut first = true;
            for column in cte.column_names() {
                self.sep(&mut first, ", ");
                self.ident(column)?;
            }
            self.kw(")");
        }
        self.kw(" AS ");
        if let Some(materialized) = cte.materialization() {
            self.require(
                self.caps.materialized_ctes,
                "`MATERIALIZED` / `NOT MATERIALIZED` on a CTE",
                "drop the hint and let the planner decide",
            )?;
            self.kw(if materialized {
                "MATERIALIZED "
            } else {
                "NOT MATERIALIZED "
            });
        }
        self.kw("(");
        match cte.query() {
            Statement::Select(select) => self.select(select)?,
            Statement::Ddl(_) => {
                return Err(Error::InvalidClause {
                    clause: "a common table expression",
                    reason: "a CTE's body must be a query, and DDL is not one",
                    help: "run the schema change as its own statement",
                });
            }
            other => {
                self.require(
                    self.caps.data_modifying_ctes,
                    "data-modifying common table expressions",
                    "run the write as its own statement inside the same transaction",
                )?;
                match other {
                    Statement::Insert(insert) => self.insert(insert)?,
                    Statement::Update(update) => self.update(update)?,
                    Statement::Delete(delete) => self.delete(delete)?,
                    Statement::Raw(raw) => self.raw_statement(raw)?,
                    Statement::Select(_) | Statement::Ddl(_) => unreachable!("handled above"),
                }
            }
        }
        self.kw(")");
        Ok(())
    }

    // ── INSERT ──────────────────────────────────────────────────────────────

    /// A whole `INSERT`.
    fn insert(&mut self, insert: &Insert) -> Result<(), Error> {
        self.with_clause(insert.ctes(), false)?;
        self.kw("INSERT INTO ");
        self.table(insert.table())?;
        if !insert.column_names().is_empty() {
            self.kw(" (");
            let mut first = true;
            for column in insert.column_names() {
                self.sep(&mut first, ", ");
                self.ident(column)?;
            }
            self.kw(")");
        }

        if insert.uses_default_values() {
            self.kw(" DEFAULT VALUES");
        } else if let Some(source) = insert.source_query() {
            self.kw(" ");
            self.select(source)?;
        } else if insert.value_rows().is_empty() {
            return Err(Error::incomplete(
                "INSERT",
                "any rows to insert",
                "call `.values(..)`, `.from_select(..)` or `.default_values()`",
            ));
        } else {
            self.insert_rows(insert)?;
        }

        if let Some(conflict) = insert.conflict() {
            self.on_conflict(conflict)?;
        }
        self.returning(insert.returning_clause(), insert.conflict().is_some())
    }

    /// The `VALUES (…), (…)` tail of an `INSERT`, with the arity check that
    /// turns a database error into an [`Error::RowArity`].
    fn insert_rows(&mut self, insert: &Insert) -> Result<(), Error> {
        let rows = insert.value_rows();
        let expected = if insert.column_names().is_empty() {
            rows[0].len()
        } else {
            insert.column_names().len()
        };
        for (index, row) in rows.iter().enumerate() {
            if row.len() != expected {
                return Err(Error::RowArity {
                    row: index,
                    expected,
                    found: row.len(),
                });
            }
        }
        self.kw(" VALUES ");
        let mut first = true;
        for row in rows {
            self.sep(&mut first, ", ");
            self.kw("(");
            let mut first_value = true;
            for value in row {
                self.sep(&mut first_value, ", ");
                self.expr(value, P_MIN)?;
            }
            self.kw(")");
        }
        Ok(())
    }

    /// `ON CONFLICT … DO …`.
    fn on_conflict(&mut self, conflict: &OnConflict) -> Result<(), Error> {
        self.require(
            self.caps.on_conflict_do_update,
            "`ON CONFLICT`",
            "select the row first and branch in the application, inside a transaction",
        )?;
        if matches!(conflict.target(), ConflictTarget::Any)
            && matches!(conflict.action(), ConflictAction::DoUpdate(_))
        {
            return Err(Error::InvalidClause {
                clause: "ON CONFLICT DO UPDATE",
                reason: "the server cannot decide which row to update without a conflict target",
                help: "name the columns — `OnConflict::columns([..])` — or the constraint — \
                       `OnConflict::constraint(..)`",
            });
        }
        self.kw(" ON CONFLICT");
        match conflict.target() {
            ConflictTarget::Any => {}
            ConflictTarget::Columns(columns) => {
                if columns.is_empty() {
                    return Err(Error::incomplete(
                        "ON CONFLICT",
                        "any conflict-target columns",
                        "pass the unique index's columns to `OnConflict::columns([..])`",
                    ));
                }
                self.kw(" (");
                let mut first = true;
                for column in columns {
                    self.sep(&mut first, ", ");
                    self.ident(column)?;
                }
                self.kw(")");
                if let Some(predicate) = conflict.target_predicate() {
                    self.require(
                        self.caps.partial_indexes,
                        "a partial-index conflict target",
                        "drop the `target_where(..)`, or make the unique index total",
                    )?;
                    self.kw(" WHERE ");
                    self.expr(predicate, P_MIN)?;
                }
            }
            ConflictTarget::Constraint(name) => {
                if self.flavor.is_sqlite() {
                    return Err(self.no(
                        "`ON CONFLICT ON CONSTRAINT`",
                        "name the columns instead: `OnConflict::columns([..])`",
                    ));
                }
                self.kw(" ON CONSTRAINT ");
                self.ident(name)?;
            }
        }
        match conflict.action() {
            ConflictAction::DoNothing => self.kw(" DO NOTHING"),
            ConflictAction::DoUpdate(assignments) => {
                if assignments.is_empty() {
                    return Err(Error::incomplete(
                        "ON CONFLICT DO UPDATE",
                        "any assignments",
                        "call `.do_update_columns([..])`, or `.do_nothing()` if that is what you \
                         meant",
                    ));
                }
                self.kw(" DO UPDATE SET ");
                self.assignments(assignments)?;
                if let Some(predicate) = conflict.update_predicate() {
                    self.kw(" WHERE ");
                    self.expr(predicate, P_MIN)?;
                }
            }
        }
        Ok(())
    }

    /// A `SET` list.
    fn assignments(&mut self, assignments: &[Assignment]) -> Result<(), Error> {
        let mut first = true;
        for assignment in assignments {
            self.sep(&mut first, ", ");
            self.ident(assignment.column())?;
            self.kw(" = ");
            self.expr(assignment.value(), P_MIN)?;
        }
        Ok(())
    }

    /// `RETURNING *` or `RETURNING a, b`.
    fn returning(&mut self, returning: &Returning, with_conflict: bool) -> Result<(), Error> {
        match returning {
            Returning::None => return Ok(()),
            Returning::All | Returning::Items(_) => {
                self.require(
                    self.caps.returning,
                    "`RETURNING`",
                    "run the write, then select the row back by its key in the same transaction",
                )?;
                if with_conflict {
                    self.require(
                        self.caps.returning_with_on_conflict,
                        "`RETURNING` together with `ON CONFLICT`",
                        "drop one of the two, or select the row back afterwards",
                    )?;
                }
            }
        }
        self.kw(" RETURNING ");
        match returning {
            Returning::All => self.kw("*"),
            Returning::Items(items) => {
                if items.is_empty() {
                    return Err(Error::incomplete(
                        "RETURNING",
                        "any items",
                        "pass the columns to `Returning::columns([..])`, or use `Returning::All`",
                    ));
                }
                let mut first = true;
                for item in items {
                    self.sep(&mut first, ", ");
                    self.select_item(item)?;
                }
            }
            Returning::None => unreachable!("handled above"),
        }
        Ok(())
    }

    // ── UPDATE and DELETE ───────────────────────────────────────────────────

    /// A whole `UPDATE`.
    fn update(&mut self, update: &Update) -> Result<(), Error> {
        if update.assignments().is_empty() {
            return Err(Error::incomplete(
                "UPDATE",
                "anything to set",
                "call `.set(column, value)` at least once",
            ));
        }
        self.with_clause(update.ctes(), false)?;
        self.kw("UPDATE ");
        self.table(update.target())?;
        if let Some(alias) = update.table_alias() {
            self.kw(" AS ");
            self.ident(alias)?;
        }
        self.kw(" SET ");
        self.assignments(update.assignments())?;
        if !update.from_items().is_empty() {
            self.kw(" FROM ");
            let mut first = true;
            for item in update.from_items() {
                self.sep(&mut first, ", ");
                self.source(item)?;
            }
        }
        self.where_clause(update.filters())?;
        self.returning(update.returning_clause(), false)
    }

    /// A whole `DELETE`.
    fn delete(&mut self, delete: &Delete) -> Result<(), Error> {
        self.with_clause(delete.ctes(), false)?;
        self.kw("DELETE FROM ");
        self.table(delete.target())?;
        if let Some(alias) = delete.table_alias() {
            self.kw(" AS ");
            self.ident(alias)?;
        }
        if !delete.using_items().is_empty() {
            if self.flavor.is_sqlite() {
                return Err(self.no(
                    "`DELETE … USING`",
                    "move the join into the filter: `WHERE id IN (SELECT … FROM other …)`",
                ));
            }
            self.kw(" USING ");
            let mut first = true;
            for item in delete.using_items() {
                self.sep(&mut first, ", ");
                self.source(item)?;
            }
        }
        self.where_clause(delete.filters())?;
        self.returning(delete.returning_clause(), false)
    }

    /// A raw statement: the same placeholder convention as a raw fragment.
    fn raw_statement(&mut self, raw: &RawStatement) -> Result<(), Error> {
        self.raw_fragment(raw.text(), raw.args())
    }

    // ── DDL ─────────────────────────────────────────────────────────────────

    /// A schema change.
    ///
    /// Every DDL statement renders with [`Binding::Literal`]: a `DEFAULT`, a
    /// `CHECK` and a partial index's predicate are parsed once and stored in the
    /// catalogue, so there is no parameter to bind them to.
    fn ddl(&mut self, ddl: &Ddl) -> Result<(), Error> {
        let previous = core::mem::replace(&mut self.binding, Binding::Literal);
        let result = self.ddl_inner(ddl);
        self.binding = previous;
        result
    }

    /// The body of [`Renderer::ddl`], with the binding mode already switched.
    fn ddl_inner(&mut self, ddl: &Ddl) -> Result<(), Error> {
        match ddl {
            Ddl::CreateTable(create) => self.create_table(create),
            Ddl::AlterTable(alter) => self.alter_table(alter),
            Ddl::DropTable(drop) => self.drop_table(drop),
            Ddl::RenameTable(rename) => self.rename_table(rename),
            Ddl::Truncate(truncate) => self.truncate(truncate),
            Ddl::CreateIndex(index) => self.create_index(index),
            Ddl::DropIndex(index) => self.drop_index(index),
            Ddl::RenameIndex(rename) => self.rename_index(rename),
            Ddl::CreateType(create) => self.create_type(create),
            Ddl::AlterType(alter) => self.alter_type(alter),
            Ddl::DropType(drop) => self.drop_type(drop),
            Ddl::CreateSchema(create) => self.create_schema(create),
            Ddl::DropSchema(drop) => self.drop_schema(drop),
            Ddl::CreateExtension(extension) => self.create_extension(extension),
            Ddl::Comment(comment) => self.comment_on(comment),
            Ddl::Raw(raw) => {
                // An explicit raw statement is the one place inside DDL where
                // the caller may still bind parameters: it is their SQL, not
                // ours, and `moso::sql!` produces exactly this shape.
                let previous = core::mem::replace(&mut self.binding, Binding::Parameter);
                let result = self.raw_statement(raw);
                self.binding = previous;
                result
            }
        }
    }

    /// `CREATE TABLE`, followed by its comments.
    fn create_table(&mut self, create: &CreateTable) -> Result<(), Error> {
        self.kw("CREATE ");
        if create.is_temporary() {
            self.kw("TEMPORARY ");
        }
        if create.is_unlogged() {
            self.require(
                !self.flavor.is_sqlite(),
                "`UNLOGGED` tables",
                "SQLite has no write-ahead-log distinction per table; drop the modifier, or use \
                 a `TEMPORARY` table",
            )?;
            self.kw("UNLOGGED ");
        }
        self.kw("TABLE ");
        if create.is_if_not_exists() {
            self.kw("IF NOT EXISTS ");
        }
        self.table(create.table())?;
        self.kw(" (");
        let mut first = true;
        for column in create.columns() {
            self.sep(&mut first, ", ");
            self.column_spec(column)?;
        }
        for constraint in create.constraints() {
            self.sep(&mut first, ", ");
            self.table_constraint(constraint, false)?;
        }
        if first {
            return Err(Error::incomplete(
                "CREATE TABLE",
                "any columns",
                "add at least one: `.column(ColumnSpec::new(name, data_type))`",
            ));
        }
        self.kw(")");
        if let Some(partitioning) = create.partitioning() {
            self.partitioning(partitioning)?;
        }
        self.table_comments(create)
    }

    /// The `COMMENT ON` statements a `CREATE TABLE` carries with it.
    ///
    /// SQLite has no comment catalogue at all, so they are dropped there. A
    /// comment is documentation and carries no semantics, which is the only
    /// reason this is a silent difference rather than an error; it is stated on
    /// [`Sqlite`] and asserted in the tests.
    fn table_comments(&mut self, create: &CreateTable) -> Result<(), Error> {
        if self.flavor.is_sqlite() {
            return Ok(());
        }
        if let Some(text) = create.comment_text() {
            self.statement_break();
            self.comment_on(&CommentOn::new(
                CommentTarget::Table(create.table().clone()),
                Some(text.to_owned()),
            ))?;
        }
        for column in create.columns() {
            if let Some(text) = column.comment_text() {
                self.statement_break();
                self.comment_on(&CommentOn::new(
                    CommentTarget::Column {
                        table: create.table().clone(),
                        column: column.name().clone(),
                    },
                    Some(text.to_owned()),
                ))?;
            }
        }
        Ok(())
    }

    /// One column definition.
    fn column_spec(&mut self, column: &ColumnSpec) -> Result<(), Error> {
        self.ident(column.name())?;
        self.kw(" ");
        self.data_type(column.data_type())?;
        if let Some(collation) = column.collation() {
            self.kw(" COLLATE ");
            self.ident(collation)?;
        }
        if let Some(generated) = column.generation() {
            self.generated(generated)?;
        }
        if let Some(identity) = column.identity_kind() {
            self.require(
                !self.flavor.is_sqlite(),
                "`GENERATED … AS IDENTITY`",
                "SQLite's rowid alias does the same job: declare the column \
                 `DataType::BigInt` and `.primary_key()`",
            )?;
            self.kw(match identity {
                Identity::Always => " GENERATED ALWAYS AS IDENTITY",
                Identity::ByDefault => " GENERATED BY DEFAULT AS IDENTITY",
            });
        }
        if !column.is_nullable() {
            self.kw(" NOT NULL");
        }
        if let Some(default) = column.default_value() {
            self.kw(" DEFAULT ");
            // SQLite requires parentheses around any non-constant default.
            let simple = matches!(default, Expr::Value(_));
            if simple {
                self.expr(default, P_MIN)?;
            } else {
                self.kw("(");
                self.expr(default, P_MIN)?;
                self.kw(")");
            }
        }
        if column.is_primary_key() {
            self.kw(" PRIMARY KEY");
        }
        if column.is_unique() {
            self.kw(" UNIQUE");
        }
        if let Some(check) = column.check_expr() {
            self.kw(" CHECK (");
            self.expr(check, P_MIN)?;
            self.kw(")");
        }
        if let Some(foreign_key) = column.foreign_key() {
            self.inline_foreign_key(column.name(), foreign_key)?;
        }
        Ok(())
    }

    /// `GENERATED ALWAYS AS (…) STORED`.
    fn generated(&mut self, generated: &Generated) -> Result<(), Error> {
        self.kw(" GENERATED ALWAYS AS (");
        self.expr(generated.expr(), P_MIN)?;
        self.kw(if generated.is_stored() {
            ") STORED"
        } else {
            self.require(
                self.flavor.is_sqlite(),
                "`VIRTUAL` generated columns",
                "PostgreSQL only stores them: use `Generated::stored(..)`",
            )?;
            ") VIRTUAL"
        });
        Ok(())
    }

    /// The column-level `REFERENCES` of a [`ColumnSpec`].
    fn inline_foreign_key(&mut self, column: &Ident, key: &ForeignKey) -> Result<(), Error> {
        if key.columns().len() > 1 || key.columns().first().is_some_and(|name| name != column) {
            return Err(Error::InvalidClause {
                clause: "an inline REFERENCES",
                reason: "a column-level foreign key covers exactly the column it is written on",
                help: "move it to a table constraint: \
                       `.constraint(TableConstraint::ForeignKey(..))`",
            });
        }
        if key.is_not_valid() {
            return Err(Error::InvalidClause {
                clause: "NOT VALID",
                reason: "only `ALTER TABLE … ADD CONSTRAINT` may defer validation",
                help: "create the table without the key, then add it with \
                       `AlterTableAction::AddConstraint(..)` and `.not_valid()`",
            });
        }
        if let Some(name) = key.name() {
            self.kw(" CONSTRAINT ");
            self.ident(name)?;
        }
        self.references(key)
    }

    /// `REFERENCES t (c) ON DELETE … ON UPDATE … DEFERRABLE …`.
    fn references(&mut self, key: &ForeignKey) -> Result<(), Error> {
        self.kw(" REFERENCES ");
        self.table(key.target_table())?;
        if !key.target_columns().is_empty() {
            self.kw(" (");
            let mut first = true;
            for column in key.target_columns() {
                self.sep(&mut first, ", ");
                self.ident(column)?;
            }
            self.kw(")");
        }
        if let Some(action) = key.delete_action() {
            self.kw(" ON DELETE ");
            self.kw(referential_action(action));
        }
        if let Some(action) = key.update_action() {
            self.kw(" ON UPDATE ");
            self.kw(referential_action(action));
        }
        if key.is_deferrable() {
            self.kw(" DEFERRABLE");
            if key.is_initially_deferred() {
                self.kw(" INITIALLY DEFERRED");
            }
        }
        Ok(())
    }

    /// One table-level constraint.
    ///
    /// `allow_not_valid` is `true` only inside `ALTER TABLE … ADD CONSTRAINT`,
    /// which is the one place the server accepts a deferred validation.
    fn table_constraint(
        &mut self,
        constraint: &TableConstraint,
        allow_not_valid: bool,
    ) -> Result<(), Error> {
        if let Some(name) = constraint.name() {
            self.kw("CONSTRAINT ");
            self.ident(name)?;
            self.kw(" ");
        }
        match constraint {
            TableConstraint::PrimaryKey { columns, .. } => {
                if columns.is_empty() {
                    return Err(Error::incomplete(
                        "PRIMARY KEY",
                        "any columns",
                        "pass them to `TableConstraint::primary_key(name, [..])`",
                    ));
                }
                self.kw("PRIMARY KEY (");
                self.ident_list(columns)?;
                self.kw(")");
            }
            TableConstraint::Unique {
                columns,
                nulls_not_distinct,
                ..
            } => {
                if columns.is_empty() {
                    return Err(Error::incomplete(
                        "UNIQUE",
                        "any columns",
                        "pass them to `TableConstraint::unique(name, [..])`",
                    ));
                }
                self.kw("UNIQUE ");
                if *nulls_not_distinct {
                    self.require(
                        !self.flavor.is_sqlite(),
                        "`NULLS NOT DISTINCT`",
                        "make the columns `NOT NULL`, or use a partial unique index over the \
                         rows that have a value",
                    )?;
                    self.kw("NULLS NOT DISTINCT ");
                }
                self.kw("(");
                self.ident_list(columns)?;
                self.kw(")");
            }
            TableConstraint::ForeignKey(key) => {
                if key.columns().is_empty() {
                    return Err(Error::incomplete(
                        "FOREIGN KEY",
                        "any referencing columns",
                        "pass them to `ForeignKey::new(name, [..], table, [..])`",
                    ));
                }
                self.kw("FOREIGN KEY (");
                self.ident_list(key.columns())?;
                self.kw(")");
                self.references(key)?;
                if key.is_not_valid() {
                    self.not_valid(allow_not_valid)?;
                }
            }
            TableConstraint::Check {
                expr, not_valid, ..
            } => {
                self.kw("CHECK (");
                self.expr(expr, P_MIN)?;
                self.kw(")");
                if *not_valid {
                    self.not_valid(allow_not_valid)?;
                }
            }
            TableConstraint::Exclude {
                method,
                elements,
                predicate,
                ..
            } => {
                self.require(
                    !self.flavor.is_sqlite(),
                    "`EXCLUDE USING …` constraints",
                    "enforce the rule in the application under an advisory lock, or keep it on \
                     PostgreSQL",
                )?;
                if elements.is_empty() {
                    return Err(Error::incomplete(
                        "EXCLUDE",
                        "any elements",
                        "pass `(expression, operator)` pairs to `TableConstraint::Exclude`",
                    ));
                }
                self.kw("EXCLUDE ");
                if let Some(method) = method {
                    self.kw("USING ");
                    self.ident(method)?;
                    self.kw(" ");
                }
                self.kw("(");
                let mut first = true;
                for (element, operator) in elements {
                    self.sep(&mut first, ", ");
                    self.expr(element, P_MIN)?;
                    self.kw(" WITH ");
                    self.kw(operator.as_str());
                }
                self.kw(")");
                if let Some(predicate) = predicate {
                    self.kw(" WHERE (");
                    self.expr(predicate, P_MIN)?;
                    self.kw(")");
                }
            }
        }
        Ok(())
    }

    /// Writes `NOT VALID`, refusing it where the server would.
    fn not_valid(&mut self, allowed: bool) -> Result<(), Error> {
        if !allowed {
            return Err(Error::InvalidClause {
                clause: "NOT VALID",
                reason: "only `ALTER TABLE … ADD CONSTRAINT` may defer validation",
                help: "create the table without the constraint, then add it with \
                       `AlterTableAction::AddConstraint(..)`",
            });
        }
        self.require(
            self.caps.deferred_constraint_validation,
            "`NOT VALID` constraints",
            "add the constraint normally; SQLite validates it as part of the table rebuild",
        )?;
        self.kw(" NOT VALID");
        Ok(())
    }

    /// A comma-separated list of quoted identifiers.
    fn ident_list(&mut self, idents: &[Ident]) -> Result<(), Error> {
        let mut first = true;
        for ident in idents {
            self.sep(&mut first, ", ");
            self.ident(ident)?;
        }
        Ok(())
    }

    /// `PARTITION BY RANGE (a, b)`.
    fn partitioning(&mut self, partitioning: &Partitioning) -> Result<(), Error> {
        self.require(
            self.caps.partitioning,
            "declarative partitioning",
            "keep the rows in one table and index the would-be partition key",
        )?;
        if partitioning.columns().is_empty() {
            return Err(Error::incomplete(
                "PARTITION BY",
                "a partition key",
                "pass the columns to `Partitioning::new(strategy, [..])`",
            ));
        }
        self.kw(" PARTITION BY ");
        self.kw(match partitioning.strategy() {
            PartitionStrategy::Range => "RANGE (",
            PartitionStrategy::List => "LIST (",
            PartitionStrategy::Hash => "HASH (",
        });
        self.ident_list(partitioning.columns())?;
        self.kw(")");
        Ok(())
    }

    /// `ALTER TABLE`, grouped into as few statements as the grammar allows.
    ///
    /// The grouping is not cosmetic: PostgreSQL takes one lock for the whole
    /// statement, so three actions in one `ALTER TABLE` take the lock once and
    /// three separate statements take it three times. That is the reason
    /// [`AlterTable`] holds a list at all.
    ///
    /// Two rules limit how much can be grouped:
    ///
    /// * **SQLite takes exactly one action per statement**, and only four of
    ///   them at that.
    /// * **PostgreSQL's `RENAME`, `SET SCHEMA` and `ATTACH`/`DETACH PARTITION`
    ///   are separate statement *forms*, not entries in the action list.**
    ///   `ALTER TABLE t ADD COLUMN a text, RENAME COLUMN b TO c` is a syntax
    ///   error, and it is the kind that a snapshot test cannot see.
    ///
    /// So the actions are cut into runs: each standalone action becomes its own
    /// statement, and every maximal run of list-able ones shares a statement,
    /// in the order they were added.
    fn alter_table(&mut self, alter: &AlterTable) -> Result<(), Error> {
        if alter.actions().is_empty() {
            return Err(Error::incomplete(
                "ALTER TABLE",
                "any actions",
                "call `.add_column(..)`, `.drop_column(..)` or `.action(..)`",
            ));
        }

        let mut groups: Vec<Vec<&AlterTableAction>> = Vec::new();
        let mut open = false;
        for action in alter.actions() {
            let standalone = self.flavor.is_sqlite() || is_standalone_alter(action);
            if standalone {
                groups.push(vec![action]);
                open = false;
            } else if open {
                groups
                    .last_mut()
                    .expect("`open` is only set after a group is pushed")
                    .push(action);
            } else {
                groups.push(vec![action]);
                open = true;
            }
        }

        let mut first_group = true;
        for group in groups {
            if first_group {
                first_group = false;
            } else {
                self.statement_break();
            }
            self.kw("ALTER TABLE ");
            self.table(alter.table())?;
            self.kw(" ");
            let mut first = true;
            for action in group {
                self.sep(&mut first, ", ");
                self.alter_table_action(action)?;
            }
        }
        Ok(())
    }

    /// One `ALTER TABLE` action.
    #[allow(clippy::too_many_lines)]
    fn alter_table_action(&mut self, action: &AlterTableAction) -> Result<(), Error> {
        match action {
            AlterTableAction::AddColumn {
                column,
                if_not_exists,
            } => {
                if self.flavor.is_sqlite() {
                    self.check_sqlite_add_column(column)?;
                }
                self.kw("ADD COLUMN ");
                if *if_not_exists {
                    self.require(
                        !self.flavor.is_sqlite(),
                        "`ADD COLUMN IF NOT EXISTS`",
                        "check `pragma table_info` first, or drop the `IF NOT EXISTS`",
                    )?;
                    self.kw("IF NOT EXISTS ");
                }
                self.column_spec(column)?;
            }
            AlterTableAction::DropColumn {
                name,
                if_exists,
                cascade,
            } => {
                self.require(
                    self.caps.drop_column,
                    "`DROP COLUMN`",
                    "rebuild the table without the column",
                )?;
                self.kw("DROP COLUMN ");
                if *if_exists {
                    self.require(
                        !self.flavor.is_sqlite(),
                        "`DROP COLUMN IF EXISTS`",
                        "check `pragma table_info` first, or drop the `IF EXISTS`",
                    )?;
                    self.kw("IF EXISTS ");
                }
                self.ident(name)?;
                if *cascade {
                    self.require(
                        !self.flavor.is_sqlite(),
                        "`DROP COLUMN … CASCADE`",
                        "drop the dependent objects first",
                    )?;
                    self.kw(" CASCADE");
                }
            }
            AlterTableAction::RenameColumn { from, to } => {
                self.require(
                    self.caps.rename_column,
                    "`RENAME COLUMN`",
                    "rebuild the table with the new name",
                )?;
                self.kw("RENAME COLUMN ");
                self.ident(from)?;
                self.kw(" TO ");
                self.ident(to)?;
            }
            AlterTableAction::AlterColumnType {
                name,
                data_type,
                using,
                ..
            } => {
                self.require(
                    self.caps.alter_column_type,
                    "`ALTER COLUMN … TYPE`",
                    "rebuild the table: create it under a new name with the new type, \
                     `INSERT … SELECT` the rows across, drop the old one and rename \
                     (`docs/02-data/23-migrations.md`, the 12-step recipe). `moso-migrate` owns \
                     that plan because it holds the whole target schema; a single `ALTER TABLE` \
                     does not",
                )?;
                self.kw("ALTER COLUMN ");
                self.ident(name)?;
                self.kw(" TYPE ");
                self.data_type(data_type)?;
                if let Some(using) = using {
                    self.kw(" USING ");
                    self.expr(using, P_MIN)?;
                }
            }
            AlterTableAction::SetNotNull(name) => {
                self.alter_column_prefix(name, "SET NOT NULL")?;
            }
            AlterTableAction::DropNotNull(name) => {
                self.alter_column_prefix(name, "DROP NOT NULL")?;
            }
            AlterTableAction::SetDefault { name, value } => {
                self.require_column_alter()?;
                self.kw("ALTER COLUMN ");
                self.ident(name)?;
                self.kw(" SET DEFAULT ");
                self.expr(value, P_MIN)?;
            }
            AlterTableAction::DropDefault(name) => {
                self.alter_column_prefix(name, "DROP DEFAULT")?;
            }
            AlterTableAction::AddConstraint(constraint) => {
                self.require_constraint_alter()?;
                self.kw("ADD ");
                self.table_constraint(constraint, true)?;
            }
            AlterTableAction::DropConstraint {
                name,
                if_exists,
                cascade,
            } => {
                self.require_constraint_alter()?;
                self.kw("DROP CONSTRAINT ");
                if *if_exists {
                    self.kw("IF EXISTS ");
                }
                self.ident(name)?;
                if *cascade {
                    self.kw(" CASCADE");
                }
            }
            AlterTableAction::ValidateConstraint(name) => {
                self.require(
                    self.caps.deferred_constraint_validation,
                    "`VALIDATE CONSTRAINT`",
                    "there is nothing to validate: SQLite checks the constraint when the table \
                     is rebuilt",
                )?;
                self.kw("VALIDATE CONSTRAINT ");
                self.ident(name)?;
            }
            AlterTableAction::RenameConstraint { from, to } => {
                self.require_constraint_alter()?;
                self.kw("RENAME CONSTRAINT ");
                self.ident(from)?;
                self.kw(" TO ");
                self.ident(to)?;
            }
            AlterTableAction::AddPrimaryKeyUsingIndex { name, index } => {
                self.using_index("PRIMARY KEY", name.as_ref(), index)?;
            }
            AlterTableAction::AddUniqueUsingIndex { name, index } => {
                self.using_index("UNIQUE", name.as_ref(), index)?;
            }
            AlterTableAction::SetSchema(schema) => {
                self.require(
                    self.caps.schemas,
                    "`SET SCHEMA`",
                    "SQLite's attached databases are not schemas a table can move between; \
                     copy the rows instead",
                )?;
                self.kw("SET SCHEMA ");
                self.ident(schema)?;
            }
            AlterTableAction::AttachPartition { partition, bounds } => {
                self.require(
                    self.caps.partitioning,
                    "`ATTACH PARTITION`",
                    "keep the rows in one table and index the would-be partition key",
                )?;
                self.kw("ATTACH PARTITION ");
                self.table(partition)?;
                self.kw(" ");
                // The bound clause's grammar depends on the strategy, so it is
                // written out by the caller. It is programmer-authored SQL, the
                // same status as a `RawExpr`.
                self.kw(bounds);
            }
            AlterTableAction::DetachPartition {
                partition,
                concurrently,
            } => {
                self.require(
                    self.caps.partitioning,
                    "`DETACH PARTITION`",
                    "keep the rows in one table and index the would-be partition key",
                )?;
                self.kw("DETACH PARTITION ");
                self.table(partition)?;
                if *concurrently {
                    self.kw(" CONCURRENTLY");
                }
            }
        }
        Ok(())
    }

    /// The four column shapes SQLite's `ALTER TABLE … ADD COLUMN` refuses.
    ///
    /// SQLite can only append a column to the end of an existing row layout
    /// without rewriting it, which rules out anything that would have to be
    /// checked against, or filled in for, the rows already there. The server's
    /// own messages — `Cannot add a UNIQUE column`,
    /// `Cannot add a NOT NULL column with default value NULL`,
    /// `Cannot add a column with non-constant default`, `cannot add a STORED
    /// column` — arrive at *migration* time, on the customer's database. These
    /// arrive at build time, with the recipe.
    fn check_sqlite_add_column(&self, column: &ColumnSpec) -> Result<(), Error> {
        const REBUILD: &str = "rebuild the table with the column in its `CREATE TABLE` \
                               (`docs/02-data/23-migrations.md`, the 12-step recipe, which \
                               `moso-migrate` emits because it holds the whole target schema)";
        if column.is_unique() {
            return Err(self.no("`ADD COLUMN … UNIQUE`", REBUILD));
        }
        if column.is_primary_key() {
            return Err(self.no("`ADD COLUMN … PRIMARY KEY`", REBUILD));
        }
        if column.generation().is_some_and(Generated::is_stored) {
            return Err(self.no(
                "`ADD COLUMN … GENERATED ALWAYS AS (…) STORED`",
                "use `Generated::virtual_(..)`, which SQLite computes on read and can add in \
                 place, or rebuild the table",
            ));
        }
        match column.default_value() {
            // A constant is the only default SQLite can back-fill without
            // rewriting every row.
            Some(Expr::Value(value)) if !value.is_null() => Ok(()),
            Some(Expr::Value(_)) | None if column.is_nullable() => Ok(()),
            Some(Expr::Value(_)) | None => Err(self.no(
                "`ADD COLUMN … NOT NULL` with no constant default",
                "give the column a constant `.default(..)` so the existing rows have a value, or \
                 add it nullable and tighten it in a table rebuild",
            )),
            Some(_) => Err(self.no(
                "`ADD COLUMN … DEFAULT <expression>`",
                "SQLite can only back-fill a constant: use `.default(Expr::value(..))`, or \
                 rebuild the table",
            )),
        }
    }

    /// `ALTER COLUMN "c" <suffix>`.
    fn alter_column_prefix(&mut self, name: &Ident, suffix: &str) -> Result<(), Error> {
        self.require_column_alter()?;
        self.kw("ALTER COLUMN ");
        self.ident(name)?;
        self.kw(" ");
        self.kw(suffix);
        Ok(())
    }

    /// SQLite's `ALTER TABLE` cannot change a column in place at all.
    fn require_column_alter(&self) -> Result<(), Error> {
        self.require(
            !self.flavor.is_sqlite(),
            "`ALTER COLUMN`",
            "rebuild the table: SQLite's `ALTER TABLE` can only rename, add and drop columns \
             (`docs/02-data/23-migrations.md`, the 12-step recipe, which `moso-migrate` emits \
             because it holds the whole target schema)",
        )
    }

    /// SQLite cannot add or drop a constraint on an existing table either.
    fn require_constraint_alter(&self) -> Result<(), Error> {
        self.require(
            !self.flavor.is_sqlite(),
            "`ADD CONSTRAINT` / `DROP CONSTRAINT`",
            "rebuild the table with the constraint in its `CREATE TABLE` \
             (`docs/02-data/23-migrations.md`, the 12-step recipe)",
        )
    }

    /// `ADD [CONSTRAINT n] PRIMARY KEY USING INDEX i` — the zero-downtime
    /// promotion of an index built with `CONCURRENTLY`.
    fn using_index(
        &mut self,
        keyword: &str,
        name: Option<&Ident>,
        index: &Ident,
    ) -> Result<(), Error> {
        self.require(
            self.caps.concurrent_indexes,
            "`ADD CONSTRAINT … USING INDEX`",
            "declare the constraint in the `CREATE TABLE`; SQLite has no concurrent index build \
             to promote",
        )?;
        self.kw("ADD ");
        if let Some(name) = name {
            self.kw("CONSTRAINT ");
            self.ident(name)?;
            self.kw(" ");
        }
        self.kw(keyword);
        self.kw(" USING INDEX ");
        self.ident(index)
    }

    /// `DROP TABLE`, one statement per table where the dialect takes only one.
    fn drop_table(&mut self, drop: &DropTable) -> Result<(), Error> {
        if drop.tables().is_empty() {
            return Err(Error::incomplete(
                "DROP TABLE",
                "any tables",
                "pass them to `DropTable::new([..])`",
            ));
        }
        if drop.is_cascade() && self.flavor.is_sqlite() {
            return Err(self.no(
                "`DROP TABLE … CASCADE`",
                "drop the dependent tables first, or run the migration with \
                 `PRAGMA foreign_keys = OFF`",
            ));
        }
        if self.flavor.is_sqlite() {
            let mut first = true;
            for table in drop.tables() {
                if first {
                    first = false;
                } else {
                    self.statement_break();
                }
                self.kw("DROP TABLE ");
                if drop.is_if_exists() {
                    self.kw("IF EXISTS ");
                }
                self.table(table)?;
            }
            return Ok(());
        }
        self.kw("DROP TABLE ");
        if drop.is_if_exists() {
            self.kw("IF EXISTS ");
        }
        let mut first = true;
        for table in drop.tables() {
            self.sep(&mut first, ", ");
            self.table(table)?;
        }
        if drop.is_cascade() {
            self.kw(" CASCADE");
        }
        Ok(())
    }

    /// `ALTER TABLE … RENAME TO …`.
    fn rename_table(&mut self, rename: &RenameTable) -> Result<(), Error> {
        self.kw("ALTER TABLE ");
        self.table(rename.from())?;
        self.kw(" RENAME TO ");
        self.ident(rename.to())
    }

    /// `TRUNCATE`, and SQLite's exact equivalent.
    fn truncate(&mut self, truncate: &Truncate) -> Result<(), Error> {
        if truncate.tables().is_empty() {
            return Err(Error::incomplete(
                "TRUNCATE",
                "any tables",
                "pass them to `Truncate::new([..])`",
            ));
        }
        if self.flavor.is_sqlite() {
            if truncate.is_cascade() {
                return Err(self.no(
                    "`TRUNCATE … CASCADE`",
                    "empty the referencing tables first, in dependency order",
                ));
            }
            if truncate.restarts_identity() {
                // `DELETE FROM sqlite_sequence WHERE name = '…'` would be the
                // equivalent, and it is a statement that fails with
                // `no such table: sqlite_sequence` on any database that has
                // never held an `AUTOINCREMENT` table — the catalogue table is
                // created lazily by the first one. Emitting SQL that works on
                // some databases and errors on others is exactly the failure
                // mode this crate refuses, so the caller is told instead.
                return Err(self.no(
                    "`TRUNCATE … RESTART IDENTITY`",
                    "a plain truncate already restarts a rowid counter, because SQLite derives \
                     the next rowid from `max(rowid)`; only an `AUTOINCREMENT` column keeps a \
                     stored one, and resetting that needs \
                     `DELETE FROM sqlite_sequence WHERE name = '…'`, which errors on a database \
                     that has no `AUTOINCREMENT` table at all",
                ));
            }
            // SQLite has no TRUNCATE. `DELETE FROM t` with no WHERE is its
            // documented equivalent and takes the same truncate-optimiser path.
            let mut first = true;
            for table in truncate.tables() {
                if first {
                    first = false;
                } else {
                    self.statement_break();
                }
                self.kw("DELETE FROM ");
                self.table(table)?;
            }
            return Ok(());
        }
        self.kw("TRUNCATE TABLE ");
        let mut first = true;
        for table in truncate.tables() {
            self.sep(&mut first, ", ");
            self.table(table)?;
        }
        if truncate.restarts_identity() {
            self.kw(" RESTART IDENTITY");
        }
        if truncate.is_cascade() {
            self.kw(" CASCADE");
        }
        Ok(())
    }

    /// `CREATE INDEX`.
    fn create_index(&mut self, index: &CreateIndex) -> Result<(), Error> {
        if index.targets().is_empty() {
            return Err(Error::incomplete(
                "CREATE INDEX",
                "any columns to index",
                "pass them to `CreateIndex::new(name, table, [..])`",
            ));
        }
        self.kw("CREATE ");
        if index.is_unique() {
            self.kw("UNIQUE ");
        }
        self.kw("INDEX ");
        if index.is_concurrent() {
            self.require(
                self.caps.concurrent_indexes,
                "`CREATE INDEX CONCURRENTLY`",
                "SQLite builds the index while holding the write lock; drop `CONCURRENTLY` and \
                 run the migration in a maintenance window",
            )?;
            self.kw("CONCURRENTLY ");
        }
        if index.is_if_not_exists() {
            self.kw("IF NOT EXISTS ");
        }
        // SQLite qualifies the *index*, not the table, when the table lives in
        // an attached database.
        if self.flavor.is_sqlite() {
            if let Some(schema) = index.table().schema() {
                self.ident(schema)?;
                self.kw(".");
            }
            self.ident(index.name())?;
            self.kw(" ON ");
            self.ident(index.table().name())?;
        } else {
            self.ident(index.name())?;
            self.kw(" ON ");
            self.table(index.table())?;
        }
        if let Some(method) = index.method() {
            self.require(
                self.caps.index_methods,
                "an index access method other than the default",
                "SQLite has only b-trees; drop the `.using(..)`",
            )?;
            self.kw(" USING ");
            match method {
                IndexMethod::BTree => self.kw("btree"),
                IndexMethod::Hash => self.kw("hash"),
                IndexMethod::Gin => self.kw("gin"),
                IndexMethod::Gist => self.kw("gist"),
                IndexMethod::SpGist => self.kw("spgist"),
                IndexMethod::Brin => self.kw("brin"),
                IndexMethod::Custom(name) => self.ident(name)?,
            }
        }
        self.kw(" (");
        let mut first = true;
        for target in index.targets() {
            self.sep(&mut first, ", ");
            self.index_target(target)?;
        }
        self.kw(")");
        if !index.included().is_empty() {
            self.require(
                !self.flavor.is_sqlite(),
                "`INCLUDE` columns on an index",
                "add them to the key instead: SQLite's index-only scans use the whole key",
            )?;
            self.kw(" INCLUDE (");
            self.ident_list(index.included())?;
            self.kw(")");
        }
        if index.has_nulls_not_distinct() {
            self.require(
                !self.flavor.is_sqlite(),
                "`NULLS NOT DISTINCT` on an index",
                "make the columns `NOT NULL`, or index `coalesce(col, sentinel)`",
            )?;
            self.kw(" NULLS NOT DISTINCT");
        }
        if let Some(predicate) = index.predicate() {
            self.require(
                self.caps.partial_indexes,
                "partial indexes",
                "index the whole table, or index an expression that is NULL for the rows you \
                 wanted to exclude",
            )?;
            self.kw(" WHERE ");
            self.expr(predicate, P_MIN)?;
        }
        Ok(())
    }

    /// One indexed column or expression.
    fn index_target(&mut self, target: &IndexTarget) -> Result<(), Error> {
        // A bare column needs no parentheses; anything else does, because the
        // grammar has no other way to tell an expression from a column list.
        if target.target_expr().as_column().is_some() {
            self.expr(target.target_expr(), P_MIN)?;
        } else {
            self.kw("(");
            self.expr(target.target_expr(), P_MIN)?;
            self.kw(")");
        }
        if let Some(collation) = target.collation() {
            self.kw(" COLLATE ");
            self.ident(collation)?;
        }
        if let Some(class) = target.operator_class_name() {
            self.require(
                !self.flavor.is_sqlite(),
                "index operator classes",
                "SQLite has no operator classes; drop the `.operator_class(..)`",
            )?;
            self.kw(" ");
            self.ident(class)?;
        }
        if let Some(order) = target.sort_order() {
            self.kw(match order {
                Order::Asc => " ASC",
                Order::Desc => " DESC",
            });
        }
        if let Some(nulls) = target.nulls_placement() {
            // `nulls_ordering` is about `ORDER BY`. SQLite parses `NULLS LAST`
            // there and answers `unsupported use of NULLS LAST` in a
            // `CREATE INDEX`, so this is its own check rather than that
            // capability's.
            self.require(
                self.caps.nulls_ordering && !self.flavor.is_sqlite(),
                "`NULLS FIRST` / `NULLS LAST` on an index",
                "drop the placement — the index still serves the lookup, just not the ordering — \
                 or index `coalesce(col, sentinel)` to put the NULLs where you want them",
            )?;
            self.kw(match nulls {
                Nulls::First => " NULLS FIRST",
                Nulls::Last => " NULLS LAST",
            });
        }
        Ok(())
    }

    /// `DROP INDEX`.
    fn drop_index(&mut self, index: &DropIndex) -> Result<(), Error> {
        self.kw("DROP INDEX ");
        if index.is_concurrent() {
            self.require(
                self.caps.concurrent_indexes,
                "`DROP INDEX CONCURRENTLY`",
                "SQLite drops an index instantly; drop `CONCURRENTLY`",
            )?;
            self.kw("CONCURRENTLY ");
        }
        if index.is_if_exists() {
            self.kw("IF EXISTS ");
        }
        if let Some(schema) = index.schema() {
            self.ident(schema)?;
            self.kw(".");
        }
        self.ident(index.name())?;
        if index.is_cascade() {
            self.require(
                !self.flavor.is_sqlite(),
                "`DROP INDEX … CASCADE`",
                "drop the dependent objects first",
            )?;
            self.kw(" CASCADE");
        }
        Ok(())
    }

    /// `ALTER INDEX … RENAME TO …`.
    fn rename_index(&mut self, rename: &RenameIndex) -> Result<(), Error> {
        if self.flavor.is_sqlite() {
            return Err(self.no(
                "renaming an index",
                "drop the index and create it under the new name",
            ));
        }
        self.kw("ALTER INDEX ");
        self.ident(rename.from())?;
        self.kw(" RENAME TO ");
        self.ident(rename.to())
    }

    /// `CREATE TYPE … AS ENUM (…)`.
    fn create_type(&mut self, create: &CreateType) -> Result<(), Error> {
        self.require(
            self.caps.enum_types,
            "user-defined enum types",
            "store the value as `text` with a `CHECK (col IN ('a', 'b'))` constraint",
        )?;
        self.kw("CREATE TYPE ");
        self.type_ref(create.name())?;
        match create.body() {
            TypeBody::Enum(labels) => {
                self.kw(" AS ENUM (");
                let mut first = true;
                for label in labels {
                    self.sep(&mut first, ", ");
                    // An enum label is a string *value*, not an identifier.
                    let label = label.clone();
                    self.quoted_literal(&label);
                }
                self.kw(")");
            }
        }
        Ok(())
    }

    /// `ALTER TYPE`.
    fn alter_type(&mut self, alter: &AlterType) -> Result<(), Error> {
        self.require(
            self.caps.enum_types,
            "user-defined enum types",
            "store the value as `text` with a `CHECK (col IN ('a', 'b'))` constraint",
        )?;
        self.kw("ALTER TYPE ");
        self.type_ref(alter.name())?;
        match alter.action() {
            AlterTypeAction::AddValue {
                value,
                before,
                after,
                if_not_exists,
            } => {
                if before.is_some() && after.is_some() {
                    return Err(Error::InvalidClause {
                        clause: "ALTER TYPE … ADD VALUE",
                        reason: "a new label can be placed before another or after another, not \
                                 both",
                        help: "keep one of `before` and `after`, or drop both to append",
                    });
                }
                self.kw(" ADD VALUE ");
                if *if_not_exists {
                    self.kw("IF NOT EXISTS ");
                }
                let value = value.clone();
                self.quoted_literal(&value);
                if let Some(before) = before {
                    self.kw(" BEFORE ");
                    let before = before.clone();
                    self.quoted_literal(&before);
                }
                if let Some(after) = after {
                    self.kw(" AFTER ");
                    let after = after.clone();
                    self.quoted_literal(&after);
                }
            }
            AlterTypeAction::RenameValue { from, to } => {
                self.kw(" RENAME VALUE ");
                let from = from.clone();
                self.quoted_literal(&from);
                self.kw(" TO ");
                let to = to.clone();
                self.quoted_literal(&to);
            }
            AlterTypeAction::Rename(name) => {
                self.kw(" RENAME TO ");
                self.ident(name)?;
            }
            AlterTypeAction::SetSchema(schema) => {
                self.kw(" SET SCHEMA ");
                self.ident(schema)?;
            }
        }
        Ok(())
    }

    /// `DROP TYPE`.
    fn drop_type(&mut self, drop: &DropType) -> Result<(), Error> {
        self.require(
            self.caps.enum_types,
            "user-defined enum types",
            "there is nothing to drop: the value is stored as `text` with a `CHECK` constraint",
        )?;
        self.kw("DROP TYPE ");
        if drop.is_if_exists() {
            self.kw("IF EXISTS ");
        }
        self.type_ref(drop.name())?;
        if drop.is_cascade() {
            self.kw(" CASCADE");
        }
        Ok(())
    }

    /// `CREATE SCHEMA`.
    fn create_schema(&mut self, create: &CreateSchema) -> Result<(), Error> {
        self.require(
            self.caps.schemas,
            "named schemas",
            "SQLite's namespace is the database file; use one file per schema and `ATTACH` it",
        )?;
        self.kw("CREATE SCHEMA ");
        if create.is_if_not_exists() {
            self.kw("IF NOT EXISTS ");
        }
        self.ident(create.name())?;
        if let Some(owner) = create.owner() {
            self.kw(" AUTHORIZATION ");
            self.ident(owner)?;
        }
        Ok(())
    }

    /// `DROP SCHEMA`.
    fn drop_schema(&mut self, drop: &DropSchema) -> Result<(), Error> {
        self.require(
            self.caps.schemas,
            "named schemas",
            "SQLite's namespace is the database file; `DETACH` it instead",
        )?;
        self.kw("DROP SCHEMA ");
        if drop.is_if_exists() {
            self.kw("IF EXISTS ");
        }
        self.ident(drop.name())?;
        if drop.is_cascade() {
            self.kw(" CASCADE");
        }
        Ok(())
    }

    /// `CREATE EXTENSION`.
    fn create_extension(&mut self, extension: &CreateExtension) -> Result<(), Error> {
        if self.flavor.is_sqlite() {
            return Err(self.no(
                "`CREATE EXTENSION`",
                "load the SQLite extension on the connection instead, with `load_extension`",
            ));
        }
        self.kw("CREATE EXTENSION ");
        if extension.is_if_not_exists() {
            self.kw("IF NOT EXISTS ");
        }
        self.ident(extension.name())?;
        if let Some(schema) = extension.target_schema() {
            self.kw(" SCHEMA ");
            self.ident(schema)?;
        }
        if let Some(version) = extension.required_version() {
            self.kw(" VERSION ");
            let version = version.to_owned();
            self.quoted_literal(&version);
        }
        Ok(())
    }

    /// `COMMENT ON … IS …`.
    fn comment_on(&mut self, comment: &CommentOn) -> Result<(), Error> {
        if self.flavor.is_sqlite() {
            return Err(self.no(
                "`COMMENT ON`",
                "SQLite has no comment catalogue; keep the documentation in the Rust doc comment, \
                 which is where `moso-admin` reads it from anyway",
            ));
        }
        self.kw("COMMENT ON ");
        match comment.target() {
            CommentTarget::Table(table) => {
                self.kw("TABLE ");
                self.table(table)?;
            }
            CommentTarget::Column { table, column } => {
                self.kw("COLUMN ");
                self.table(table)?;
                self.kw(".");
                self.ident(column)?;
            }
            CommentTarget::Index(name) => {
                self.kw("INDEX ");
                self.ident(name)?;
            }
            CommentTarget::Type(name) => {
                self.kw("TYPE ");
                self.type_ref(name)?;
            }
        }
        self.kw(" IS ");
        match comment.text() {
            Some(text) => {
                let text = text.to_owned();
                self.quoted_literal(&text);
            }
            None => self.kw("NULL"),
        }
        Ok(())
    }

    // ── budget ──────────────────────────────────────────────────────────────

    /// Refuses a statement that binds more parameters than the wire protocol
    /// can carry, with a chunk size that would fit.
    fn check_parameter_budget(&self, statement: StatementRef<'_>) -> Result<(), Error> {
        let limit = self.dialect.max_bind_params();
        let found = self.args.len();
        if found <= limit {
            return Ok(());
        }
        let suggested = match statement {
            StatementRef::Insert(insert) if insert.row_count() > 0 => {
                let per_row = insert.bind_count().div_ceil(insert.row_count()).max(1);
                (limit / per_row).max(1)
            }
            _ => limit.max(1),
        };
        Err(Error::TooManyParameters {
            dialect: self.name(),
            limit,
            found,
            suggested,
        })
    }
}

// ── free helpers ────────────────────────────────────────────────────────────

/// Whether an `ALTER TABLE` action is a statement form of its own rather than
/// an entry in the comma-separated action list.
///
/// PostgreSQL's grammar has `ALTER TABLE name action [, …]` *and* a handful of
/// one-action forms — `RENAME`, `SET SCHEMA`, `ATTACH PARTITION`,
/// `DETACH PARTITION`. Mixing them is a syntax error, and the server's message
/// (`syntax error at or near "RENAME"`) points at the keyword rather than at
/// the mistake, so the split happens here.
const fn is_standalone_alter(action: &AlterTableAction) -> bool {
    matches!(
        action,
        AlterTableAction::RenameColumn { .. }
            | AlterTableAction::RenameConstraint { .. }
            | AlterTableAction::SetSchema(_)
            | AlterTableAction::AttachPartition { .. }
            | AlterTableAction::DetachPartition { .. }
    )
}

/// The keyword for a referential action.
const fn referential_action(action: ReferentialAction) -> &'static str {
    match action {
        ReferentialAction::NoAction => "NO ACTION",
        ReferentialAction::Restrict => "RESTRICT",
        ReferentialAction::Cascade => "CASCADE",
        ReferentialAction::SetNull => "SET NULL",
        ReferentialAction::SetDefault => "SET DEFAULT",
    }
}

/// The column type a value of this kind would be stored in, used to give an
/// empty array literal an element type.
fn data_type_of(kind: ValueKind) -> Option<DataType> {
    Some(match kind {
        ValueKind::Unknown | ValueKind::Array => return None,
        ValueKind::Bool => DataType::Boolean,
        ValueKind::I8 | ValueKind::I16 => DataType::SmallInt,
        ValueKind::I32 | ValueKind::U8 | ValueKind::U16 => DataType::Integer,
        ValueKind::I64 | ValueKind::U32 => DataType::BigInt,
        ValueKind::U64 | ValueKind::Decimal => DataType::Numeric {
            precision: None,
            scale: None,
        },
        ValueKind::F32 => DataType::Real,
        ValueKind::F64 => DataType::DoublePrecision,
        ValueKind::Text => DataType::Text,
        ValueKind::Bytes => DataType::Bytea,
        ValueKind::Uuid => DataType::Uuid,
        ValueKind::Json => DataType::JsonB,
        ValueKind::Timestamp => DataType::Timestamp {
            with_time_zone: true,
        },
        ValueKind::DateTime => DataType::Timestamp {
            with_time_zone: false,
        },
        ValueKind::Date => DataType::Date,
        ValueKind::Time => DataType::Time {
            with_time_zone: false,
        },
        ValueKind::Interval => DataType::Interval,
    })
}

/// Renders a [`Timestamp`] as the ISO-8601 UTC text both servers parse.
///
/// [`Timestamp`] is deliberately a second count with no calendar (ADR-0005: no
/// `chrono` in the public API), so the calendar is reconstructed here with the
/// proleptic Gregorian algorithm PostgreSQL itself uses.
fn format_timestamp(timestamp: Timestamp) -> String {
    let seconds = timestamp.unix_seconds();
    let days = seconds.div_euclid(86_400);
    let within_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = within_day / 3_600;
    let minute = (within_day % 3_600) / 60;
    let second = within_day % 60;
    let mut text = format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}");
    if timestamp.nanoseconds() != 0 {
        let _ = write!(text, ".{:09}", timestamp.nanoseconds());
    }
    text.push_str("+00");
    text
}

/// Days since 1970-01-01 to a proleptic Gregorian date.
///
/// Howard Hinnant's `civil_from_days`, which is exact for the whole `i64` range
/// this crate can represent.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01, so that a leap day lands at the end of a
    // "year" and the month arithmetic below has no special case for February.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * month_index + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    })
    .unwrap_or(1);
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod live;
#[cfg(test)]
mod tests;
