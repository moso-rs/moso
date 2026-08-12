#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "Moso's sealed SQL-construction facade: Moso types in, dialect-correct SQL out."]
//!
//! # What this crate is for
//!
//! An ORM lives or dies on the query engine underneath it, and every Rust
//! attempt so far has either written one (and drowned in dialect coverage) or
//! married one (and could not leave). [ADR-0005] takes the third option: Moso
//! borrows an engine and hides it completely.
//!
//! Every type in this crate's public API belongs to Moso — the identifiers, the
//! values, the expression tree, the statements, the errors, even the UUID and
//! the decimal. Nothing from the engine underneath appears in a signature,
//! anywhere, and `xtask check-sealed` fails the build on the commit that
//! changes that. The consequence is the point: the engine can be replaced in a
//! patch release, and no user's code moves.
//!
//! ```
//! use moso_sql::{Expr, Ident, Select, TableRef};
//!
//! let admins = Select::from_table(TableRef::from_static("users"))
//!     .select_all()
//!     .filter(Expr::col(Ident::from_static("is_admin")).eq(Expr::value(true)))
//!     .order_by(moso_sql::OrderTerm::desc(Expr::col(Ident::from_static("created_at"))))
//!     .limit(20);
//! assert_eq!(admins.filters().len(), 1);
//! ```
//!
//! # Two structural guarantees
//!
//! **Injection is impossible, not merely avoided.** A runtime string becomes an
//! identifier only through [`Ident`], which validates it and which every
//! dialect emits quoted. A runtime string becomes a value only through
//! [`Value`], which is bound as a parameter and never formatted into text.
//! There is no third door — not a `raw_column`, not an `unchecked` constructor.
//! The escape hatches ([`RawExpr`], [`RawStatement`]) take a fragment written
//! by the programmer and bind their own parameters, so they do not open one
//! either.
//!
//! **The builders are shape-stable.** [`Select`] stays `Select` through any
//! chain of combinators, because clauses accumulate in runtime vectors rather
//! than in the type ([ADR-0007], non-negotiable N1). That is what makes a
//! dynamic query a `for` loop instead of a type-level puzzle, and it is why no
//! error message from this layer ever prints a forty-line type.
//!
//! # The map
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`ident`] | [`Ident`], [`TableRef`], [`ColumnRef`], [`TypeRef`] |
//! | [`value`] | [`Value`], [`Bindable`], and the scalar types: [`Uuid`], [`Decimal`], [`Timestamp`], [`Date`], [`Time`], [`DateTime`], [`Interval`], [`Json`], [`Array`] |
//! | [`types`] | [`DataType`] — one type vocabulary for `CAST` and for DDL |
//! | [`expr`] | [`Expr`] and everything that appears inside one |
//! | [`order`] | [`OrderTerm`], [`Order`], [`Nulls`] |
//! | [`select`] | [`Select`], joins, CTEs, locks, set operations |
//! | [`insert`] | [`Insert`], [`OnConflict`] |
//! | [`update`] | [`Update`] |
//! | [`delete`] | [`Delete`] |
//! | [`statement`] | [`Statement`], [`StatementRef`], [`Returning`], [`RawStatement`] |
//! | [`ddl`] | schema changes, for the migration generator |
//! | [`dialect`] | [`Dialect`], [`Capabilities`], [`Postgres`], [`Sqlite`] |
//! | [`sql`] | [`Sql`] — the rendered text and its parameters |
//! | [`error`] | [`Error`] |
//!
//! # Who should use it
//!
//! Almost nobody, directly. `moso-orm` is the ergonomic, type-checked layer on
//! top, and `moso::sql!` plus `Db::pool()` are the documented escape hatches
//! for what neither covers. This crate is public because a sealed facade whose
//! seam is invisible cannot be audited, and because a third party adding a
//! backend needs [`Dialect`].
//!
//! [ADR-0005]: https://github.com/lowsbarrel/moso/blob/main/docs/adr/0005-sealed-sql-facade.md
//! [ADR-0007]: https://github.com/lowsbarrel/moso/blob/main/docs/adr/0007-shape-stable-query-builder.md

pub mod ddl;
pub mod delete;
pub mod dialect;
pub mod error;
pub mod expr;
pub mod ident;
pub mod insert;
pub mod order;
mod render;
pub mod select;
pub mod sql;
pub mod statement;
pub mod types;
pub mod update;
pub mod value;

pub use crate::delete::Delete;
pub use crate::dialect::{Capabilities, Dialect, Postgres, Sqlite};
pub use crate::error::Error;
pub use crate::expr::{
    Aggregate, AggregateFunc, BinOp, Case, Expr, Frame, FrameBound, FrameExclusion, FrameUnits,
    Function, JsonOp, Quantifier, RawExpr, TextQuery, TrimMode, UnOp, WindowExpr, WindowFunc,
    WindowRef, WindowSpec,
};
pub use crate::ident::{ColumnRef, Ident, IdentError, TableRef, TypeRef};
pub use crate::insert::{ConflictAction, ConflictTarget, Insert, OnConflict};
pub use crate::order::{Nulls, Order, OrderTerm};
pub use crate::select::{
    Cte, Distinct, FromItem, Join, JoinCondition, JoinKind, Lock, LockBehavior, LockStrength,
    Select, SelectItem, SetOp,
};
pub use crate::sql::Sql;
pub use crate::statement::{
    Assignment, RawStatement, Returning, Statement, StatementKind, StatementRef,
};
pub use crate::types::DataType;
pub use crate::update::Update;
pub use crate::value::{
    Array, Bindable, DateTime, Decimal, Interval, Json, Time, Timestamp, Uuid, Value, ValueError,
    ValueKind,
};

// `Date` is re-exported separately so the name is easy to find in the source
// when someone goes looking for why it is not `chrono::NaiveDate` (ADR-0005:
// no foreign type in a public signature).
pub use crate::value::Date;

#[cfg(test)]
mod engine;
