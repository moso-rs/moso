//! What a file full of entities and queries needs, in one `use`.
//!
//! Re-exported by the facade as `moso::db::prelude`, which is the spelling the
//! tutorial uses.
//!
//! ```
//! use moso_orm::prelude::*;
//!
//! // Everything a model file needs is now in scope.
//! fn is_in_scope<T: SqlType>() {}
//! is_in_scope::<String>();
//! ```
//!
//! # The derives are not here
//!
//! `#[derive(Entity)]` and friends come from the facade — `moso::db::prelude`
//! re-exports both halves — because generated code names `::moso::__private::*`
//! and nothing else (decision D6).
//!
//! # What is deliberately not here
//!
//! [`Handle`](crate::Handle), [`RowStream`](crate::RowStream),
//! [`StatementCounter`](crate::StatementCounter) and the descriptor types: they
//! are read by the framework and by `moso-migrate`, not written by
//! applications. A prelude that exports everything is a prelude nobody can
//! read.
//!
//! [`Scope`] *is* here, and the line it sits on is the one an
//! application writes: [`Select::with_scope`](crate::Select::with_scope) takes
//! one, so exporting the method without its only argument type would send a
//! reader hunting for a name the prelude already knows. Note that
//! `moso_authz::prelude` exports a different `Scope` — a role assignment's
//! reach — so a file that globs both preludes has to import one of them by
//! name. Those two preludes already collide on `Error` and `Result`, which is
//! why globbing both is not the shape either of them is written for.

pub use crate::column::{Column, ColumnValue};
pub use crate::db::{Backend, DatabaseConfig, Db, TenantId};
pub use crate::delete::Delete;
pub use crate::entity::{ColumnDef, Entity, NewEntity};
pub use crate::error::{Error, Result};
pub use crate::executor::Executor;
pub use crate::insert::Insert;
pub use crate::page::{OffsetPaginated, Paginated};
pub use crate::predicate::{Predicate, all, any, not};
pub use crate::projection::Projection;
pub use crate::raw::RawQuery;
pub use crate::relation::{BelongsTo, HasMany, HasOne, ManyToMany, Related, Relation};
pub use crate::row::{DecodeError, Row};
pub use crate::scope::Scope;
pub use crate::select::{LockMode, Select};
pub use crate::sqltype::{Json, SqlType};
pub use crate::tx::{Isolation, RequestTx, Tx, TxOptions};
pub use crate::update::Update;

// The identifier and value vocabulary an entity declaration names directly.
pub use moso_sql::{Expr, Ident, OrderTerm, TableRef};
