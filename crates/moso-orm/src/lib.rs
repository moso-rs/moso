#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "Moso's ORM: entities, shape-stable queries, N+1-safe relations, transactions and pooling."]
//!
//! # What this crate is for
//!
//! The unmet need in Rust's data layer is not another query engine. It is
//! **ergonomics, relations, migrations and error messages** on top of an
//! execution layer that already works. `moso-orm` is the thin, opinionated
//! layer; [`moso_sql`] renders the SQL behind a sealed facade (ADR-0005) and
//! `sqlx` executes it.
//!
//! ```no_run
//! use moso_orm::{Db, Result};
//! # use moso_orm::{Column, ColumnDef, DecodeError, Entity, Row, Select};
//! # use moso_orm::descriptor::EntityDescriptor;
//! # use moso_sql::{TableRef, ValueKind};
//! # use std::sync::OnceLock;
//! # #[derive(Clone, Debug)] pub struct User { pub id: i64, pub is_admin: bool }
//! # impl Entity for User {
//! #     type Pk = i64;
//! #     const TABLE: TableRef = TableRef::from_static("users");
//! #     const COLUMNS: &'static [ColumnDef] = &[
//! #         ColumnDef::new("id", ValueKind::I64).primary_key(),
//! #         ColumnDef::new("is_admin", ValueKind::Bool),
//! #     ];
//! #     const NAME: &'static str = "User";
//! #     fn pk(&self) -> i64 { self.id }
//! #     fn from_row(row: &Row) -> Result<Self, DecodeError> {
//! #         Ok(Self { id: row.get_i64(0)?, is_admin: row.get_bool(1)? })
//! #     }
//! #     fn descriptor() -> &'static EntityDescriptor {
//! #         static D: OnceLock<EntityDescriptor> = OnceLock::new();
//! #         D.get_or_init(|| EntityDescriptor::builder("User", Self::TABLE).build())
//! #     }
//! # }
//! # const IS_ADMIN: Column<User, bool> = Column::new("is_admin");
//! async fn admins(db: &Db) -> Result<Vec<User>> {
//!     Select::<User>::new().filter(IS_ADMIN.eq(true)).fetch_all(db).await
//! }
//! ```
//!
//! # The eight non-negotiables, and where each one lives
//!
//! | | Promise | Where |
//! | --- | --- | --- |
//! | N1 | Shape-stable builders — `Select<E>` stays `Select<E>` | [`mod@select`] |
//! | N2 | No implicit lazy loading; [`Related::get`] never queries | [`mod@relation`] |
//! | N3 | Batched eager loading: `+1` statement per relation | [`Preload::statement_count`] |
//! | N4 | `filter_opt` / `filter_if` / `when`, and `join_if` too | [`Select::filter_opt`] |
//! | N5 | Typed partial selects, tuples and derived projections | [`mod@projection`] |
//! | N6 | Migrations generated from [`EntityDescriptor`](descriptor::EntityDescriptor) | [`mod@descriptor`] |
//! | N7 | Errors that name the problem, with a field pointer | [`mod@error`] |
//! | N8 | `RawQuery`, and `Db::postgres_pool` for everything else | [`mod@raw`] |
//!
//! # The two decisions worth knowing before you read the code
//!
//! **The builder is shape-stable (ADR-0007).** Type safety lives at the
//! expression construction site — [`Column<E, T>`] — rather than in the
//! builder's type. `User::AGE.gt("x")` does not compile; `Select<User>` never
//! becomes forty lines of generics.
//!
//! **The joined-entity set is checked when the statement is built, not in the
//! type.** `docs/adr/README.md` left this open; [`mod@predicate`] records the
//! decision, the four reasons, and the message it produces. `J` — the second
//! parameter of [`Select`] — is kept for the one obligation that has no good
//! runtime equivalent: a tenant scope, whose failure is a silent cross-tenant
//! read rather than a loud SQL error.
//!
//! # The map
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`mod@entity`] | [`Entity`], [`ColumnDef`], [`NewEntity`], [`Ready`], [`NeedsTenant`] |
//! | [`mod@column`] | [`Column`], [`ColumnValue`] |
//! | [`mod@predicate`] | [`Predicate`], and the joined-set decision |
//! | [`mod@sqltype`] | [`SqlType`], [`TextLike`], [`JsonLike`], [`Nullable`], [`Json`], [`DbEnum`] |
//! | [`mod@row`] | [`Row`], [`DecodeError`] |
//! | [`mod@descriptor`] | everything `moso-migrate` diffs and `moso-admin` renders |
//! | [`mod@select`] | [`Select`], [`Projected`], [`LockMode`], [`EntityStream`] |
//! | [`mod@insert`] / [`mod@update`] / [`mod@delete`] | the write builders and their guards |
//! | [`mod@relation`] | [`Related`], [`Preload`], [`BelongsTo`], [`HasMany`], … |
//! | [`mod@projection`] | [`Projection`], [`ColumnTuple`], [`ProjectionScope`] |
//! | [`mod@page`] | [`Paginated`], [`OffsetPaginated`], [`OrderingKey`] |
//! | [`mod@cursor`] | [`PageCursor`] — the signed, opaque keyset token |
//! | [`mod@db`] | [`Db`], [`DatabaseConfig`], [`Backend`], [`PoolStats`] |
//! | [`mod@tx`] | [`Tx`], [`TxOptions`], [`RequestTx`], [`RequestTxLayer`] |
//! | [`mod@executor`] | [`Executor`], [`Handle`], [`StatementCounter`] |
//! | [`mod@raw`] | [`RawQuery`] — non-negotiable N8 |
//! | [`mod@error`] | [`Error`], [`Unjoined`], [`ConstraintViolation`] |
//!
//! # Where the derives are
//!
//! `#[derive(Entity)]`, `#[derive(Projection)]`, `#[derive(Embedded)]` and
//! `#[derive(DbEnum)]` live in `moso-orm-macros` and are re-exported by the
//! **facade**, not by this crate. That is not tidiness: generated code resolves
//! against `::moso::__private::*` and nothing else (decision D6), so a derive
//! reached through `moso_orm` would emit paths that do not exist. Write
//! `use moso::db::prelude::*;`.

pub mod column;
pub mod cursor;
pub mod db;
pub mod delete;
pub mod descriptor;
pub mod entity;
pub mod error;
pub mod executor;
pub mod expr;
pub mod insert;
pub mod page;
pub mod predicate;
pub mod projection;
pub mod raw;
pub mod relation;
pub mod row;
pub mod scope;
pub mod select;
pub mod sqltype;
pub mod tx;
pub mod update;

pub mod prelude;

pub use crate::column::{Column, ColumnValue};
pub use crate::cursor::PageCursor;
pub use crate::db::{Backend, DatabaseConfig, Db, PoolStats, ReplicaConfig, TenantId, TlsMode};
pub use crate::delete::Delete;
pub use crate::descriptor::{ColumnRole, EntityDescriptor, RelationKind};
pub use crate::entity::{ColumnDef, Entity, EntityRef, NeedsTenant, NewEntity, Ready};
pub use crate::error::{
    CallSite, ConstraintKind, ConstraintViolation, CursorError, DatabaseError, Error, Result,
    Unjoined,
};
pub use crate::executor::{Executor, Handle, RowStream, StatementCounter, StatementMark};
pub use crate::insert::{Conflict, ConflictAction, Insert};
pub use crate::page::{OffsetPaginated, OrderingKey, PageDirection, Paginated};
pub use crate::predicate::{Predicate, all, any, not};
pub use crate::projection::{ColumnTuple, Projection, ProjectionScope};
pub use crate::raw::{RawQuery, Typed};
pub use crate::relation::{
    BelongsTo, HasMany, HasOne, ManyToMany, NotLoaded, Preload, Related, Relation,
};
pub use crate::row::{DecodeError, DecodeErrorKind, Row};
pub use crate::scope::Scope;
pub use crate::select::{
    Deleted, EntityStream, Filter, JoinKind, Joined, LockBehavior, LockMode, Projected, Select,
};
pub use crate::sqltype::{
    DbEnum, EnumStorage, Json, JsonLike, Nullable, Sortable, SqlType, TextLike,
};
pub use crate::tx::{Isolation, RequestTx, RequestTxLayer, Tx, TxOptions};
pub use crate::update::Update;

/// The version of this crate, for `moso doctor` and the boot log.
///
/// ```
/// assert!(!moso_orm::VERSION.is_empty());
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    /// The public surface is re-exported from the crate root, so that a user
    /// writes `moso_orm::Select` and not `moso_orm::select::Select`. A name
    /// that stops resolving here is a breaking change someone made by accident.
    #[test]
    fn the_frozen_surface_resolves_from_the_root() {
        fn exists<T>() {}

        exists::<crate::Db>();
        exists::<crate::DatabaseConfig>();
        exists::<crate::Backend>();
        exists::<crate::Tx>();
        exists::<crate::TxOptions>();
        exists::<crate::RequestTx>();
        exists::<crate::RequestTxLayer>();
        exists::<crate::Error>();
        exists::<crate::Unjoined>();
        exists::<crate::ConstraintViolation>();
        exists::<crate::DecodeError>();
        exists::<crate::Row>();
        exists::<crate::Predicate>();
        exists::<crate::ColumnDef>();
        exists::<crate::EntityDescriptor>();
        exists::<crate::RawQuery>();
        exists::<crate::StatementCounter>();
        exists::<crate::NeedsTenant>();
        exists::<crate::Preload>();
        exists::<crate::Related<i32>>();
        exists::<crate::NotLoaded>();
        exists::<crate::OrderingKey>();
    }
}
