//! Everything macro-generated code refers to.
//!
//! **This module is not public API.** Its contents change without notice, in
//! patch releases. Do not import from it. If something here is useful to you,
//! open an issue and it will be given a real home.
//!
//! # Why it exists
//!
//! Generated code must resolve against exactly one path, and that path must not
//! change when a type moves between internal crates. Every macro emits
//! `::moso::__private::X` — never `::moso_core::X`, never `::moso_schema::X` —
//! so `moso-core` can be refactored, split or renamed without touching
//! `moso-macros` and without a user's expanded code breaking.
//!
//! Two rules follow, and both are load-bearing:
//!
//! 1. **Macros never name a runtime crate.** A generated `::moso_core::Error`
//!    would break for any user who renamed the dependency, and would tie the
//!    macro crate's version to the runtime crate's layout.
//! 2. **This module is generous.** A missing re-export breaks every macro at
//!    once, in user code, with an error pointing at generated tokens. Adding a
//!    name here is free; discovering an absent one is not.
//!
//! # What a generated `#[endpoint]` uses
//!
//! An excerpt of the expansion, not a program — the surrounding `async fn` and
//! the `__moso_op_create` unit struct are elided. That every path below resolves
//! is checked by this crate's `generated_endpoint_paths_resolve` test rather
//! than by a doctest, because a bound is a cheaper proof than a call.
//!
//! ```text
//! impl ::moso::__private::Endpoint for __moso_op_create {
//!     const NAME: &'static str = "create";
//!     fn spec(b: &mut ::moso::__private::OperationBuilder) {
//!         b.summary("Create a user.");
//!         b.source(::core::file!(), ::core::line!());
//!         <Json<CreateUser> as ::moso::__private::ExtractBody>::describe(b);
//!         <Result<Created<UserOut>> as ::moso::__private::Describe>::describe(b);
//!     }
//!     fn required_providers() -> &'static [::moso::__private::ProviderReq] {
//!         ::moso::__private::concat_reqs!(
//!             <Json<CreateUser> as ::moso::__private::ExtractBody>::PROVIDER_REQ,
//!         )
//!     }
//! }
//! ```

// ---------------------------------------------------------------------------
// Handlers and routing — `#[endpoint]`, `routes!`, `ep!`
// ---------------------------------------------------------------------------

pub use moso_core::handler::{
    BoxedHandler, Endpoint, ErasedHandler, Handler, HandlerAdapter, HandlerFn, HandlerFuture,
    MAX_HANDLER_PARAMS, UndocumentedEndpoint, boxed,
};
pub use moso_core::router::{
    DynGuard, MethodRouter, Route, RouteEntry, RouteService, Router, StaticSource,
};

/// Concatenate `const` slices of [`ProviderReq`] — how `#[endpoint]` builds
/// `Endpoint::required_providers` from its parameters.
pub use moso_core::concat_reqs;
/// Check a route path literal *at compile time*.
///
/// `routes!` validates the literal itself, where it can put a `note:` and a
/// `help:` on the user's own quotes, and then wraps it in this as a backstop —
/// so a path that reaches the router by another road is still rejected before
/// boot. It expands to a named `const` bound to [`validate_path`], which is why
/// the literal has to reach the macro verbatim.
pub use moso_core::route_path;
/// The `const fn` behind [`route_path!`], named so the expansion resolves.
pub use moso_core::router::validate_path;

// ---------------------------------------------------------------------------
// Extraction and response — every `describe` call site
// ---------------------------------------------------------------------------

/// The `serde` deserialisers `#[schema(delimited = ',')]` names in the
/// `#[serde(deserialize_with = "…")]` it generates.
pub use moso_core::extract::query::{comma_delimited, pipe_delimited, space_delimited};
pub use moso_core::extract::{
    Cookies, Depends, Extract, ExtractBody, Form, Headers, Inject, Json, Opaque, OpaqueBody, Path,
    Query, RequestId, ctx_from_parts, read_limited,
};
pub use moso_core::response::{
    Accepted, Cached, Created, Describe, Either, Empty, HandlerReturn, IntoResponse, NoContent,
    Page, Raw, Redirect, Text, describe_json, empty_response, json_response, set_header,
};

// ---------------------------------------------------------------------------
// Dependency injection
// ---------------------------------------------------------------------------

pub use moso_core::di::{
    Dependency, ProviderMap, ProviderMapBuilder, ProviderReq, missing_provider_error,
};

// ---------------------------------------------------------------------------
// The request and its context
// ---------------------------------------------------------------------------

pub use moso_core::ctx::{DependencyCache, Limits, PathParams, RequestCtx, RequestCtxInner};
pub use moso_core::{BoxFuture, Request, Response};

// ---------------------------------------------------------------------------
// Errors — `#[derive(Error)]` and every generated `?`
// ---------------------------------------------------------------------------

pub use moso_core::error::problem::{Problem, ProblemField, ProblemOptions};
pub use moso_core::error::{BootError, BootErrors, BoxError, Error, ErrorKind, Result};

// ---------------------------------------------------------------------------
// Middleware — `#[middleware]`
// ---------------------------------------------------------------------------

pub use moso_core::middleware::{
    CustomLayer, Guard, MiddlewareStack, Next, Slot, layer_fn, middleware_ctx,
};

// ---------------------------------------------------------------------------
// Configuration — `#[derive(Config)]`
// ---------------------------------------------------------------------------

pub use moso_core::config::{
    Coerce, CoerceError, Config, ConfigDescriptor, ConfigKey, ConfigLoader, ConfigSource,
    ConfigValue, DefaultsSource, FieldDescriptor, FieldSpec, Origin, Profile, RawValue, Reloadable,
    SecretBytes, SecretProvider, SecretRef, SecretString,
};

// ---------------------------------------------------------------------------
// Schema — `#[derive(Schema)]` and `#[derive(Constrained)]`
// ---------------------------------------------------------------------------

pub use moso_core::schema::json_schema::{
    ArrayBuilder, DEFAULT_REF_PREFIX, Discriminator, NumberBuilder, ObjectBuilder, SchemaGenerator,
    SchemaNode, SchemaRef, StringBuilder,
};
/// The regex engine a `#[schema(pattern = "…")]` body compiles its `OnceLock`
/// against — the same one [`check_pattern`] matches with.
pub use moso_core::schema::regex;
pub use moso_core::schema::types::ConstraintError;
pub use moso_core::schema::{
    ErrorCode, FieldError, MessageProvider, Schema, Validate, ValidationCtx, ValidationErrors,
    codes, generic_schema_name, inline_schema_ref,
};

/// The `check_*` helpers a generated `Validate` body calls.
///
/// Re-exported as a glob because the set grows with the attribute vocabulary,
/// and a derive that needs a helper the facade forgot to list is a bug report
/// from a user rather than a compile error here.
pub use moso_core::schema::checks::*;

// ---------------------------------------------------------------------------
// OpenAPI — every `describe` body
// ---------------------------------------------------------------------------

pub use moso_core::openapi::path::{
    Header, HttpMethod, MediaType, Parameter, ParameterLocation, ParameterStyle, RequestBody,
};
pub use moso_core::openapi::security::{SecurityRequirement, SecurityScheme};
pub use moso_core::openapi::{
    ContentType, DocumentBuilder, OperationBuilder, OperationSpec, PROBLEM_SCHEMA_NAME, Param,
    ResponseSpec, RouteMetadata, SourceLocation, VALIDATION_PROBLEM_SCHEMA_NAME,
};

// ---------------------------------------------------------------------------
// Substrate — so generated code needs no dependency the user did not add
// ---------------------------------------------------------------------------

/// `serde`, for the `Serialize`/`Deserialize` a `#[derive(Schema)]` emits.
///
/// Re-exported so an application can derive `Schema` without adding `serde` to
/// its own `Cargo.toml` — and, more importantly, so it cannot end up with a
/// *different* `serde` from the one Moso compiled against.
pub use moso_core::deps::serde;
pub use moso_core::deps::{axum, http, serde_json, tokio, tower, tracing};

// ---------------------------------------------------------------------------
// Data layer — `#[derive(Entity)]`, `#[derive(Projection)]`,
// `#[derive(Embedded)]`, `#[derive(DbEnum)]`, `#[derive(Factory)]`, `sql!`
// ---------------------------------------------------------------------------
//
// Behind the `orm` feature, because the crates behind these names are. The
// derives live in `moso-orm-macros`, which — like every macro crate here — is
// forbidden to depend on a runtime crate (rule 1), so every path it emits
// resolves here and nowhere else (decision D6). The set below is exactly what
// `moso-orm-macros` names; `crates/moso-orm-tests` compiles real entities
// through every one of them and runs them against a real database, which is
// what keeps the two in step.

/// `moso_orm::Json`, the JSON *column* wrapper, under a non-colliding name.
///
/// `Json` is already taken in this module by `moso_core::extract::Json`, the
/// HTTP body extractor that `#[endpoint]` names, and a `__private` module can
/// only hold one `Json`. The two are unrelated types with the same good name,
/// so the newer one is the one that moves: `#[derive(Entity)]` emits
/// `#private::SqlJson` for a `#[entity(json)]` column. Renaming the *extractor*
/// instead would change an expansion every shipped `#[endpoint]` already
/// depends on.
#[cfg(feature = "orm")]
pub use moso_orm::Json as SqlJson;
/// The `Result` alias `#[derive(Factory)]` returns.
///
/// Named `OrmResult` and not `Result` because generated code sits in the user's
/// module, where `Result` is `core::result::Result` and shadowing it would be a
/// surprise in every unrelated expansion.
#[cfg(feature = "orm")]
pub use moso_orm::Result as OrmResult;
#[cfg(feature = "orm")]
pub use moso_orm::descriptor::{
    CheckDescriptor, ColumnDefault, ColumnDescriptor, EnumTypeDescriptor, ForeignKeyDescriptor,
    IndexDescriptor, JoinTableDescriptor, PolymorphicDescriptor, RelationDescriptor,
};
#[cfg(feature = "orm")]
pub use moso_orm::entity::{concat_columns, concat_names, total_columns, total_names};
#[cfg(feature = "orm")]
pub use moso_orm::projection::{checked_aggregate, checked_column_as, raw_expr_as};
#[cfg(feature = "orm")]
pub use moso_orm::relation::{
    BelongsTo, BelongsToAny, HasMany, HasOne, LoadedRows, ManyToMany, PolymorphicKeyFn,
    PolymorphicVariant, Preload, Relation,
};
#[cfg(feature = "orm")]
pub use moso_orm::{
    Column, ColumnDef, ColumnRole, ColumnValue, DbEnum, DecodeError, Delete, Entity,
    EntityDescriptor, EnumStorage, Executor, Insert, NeedsTenant, NewEntity, NotLoaded, Projection,
    ProjectionScope, RawQuery, Related, RelationKind, Row, Select, SqlType, Update,
};
#[cfg(feature = "orm")]
pub use moso_sql::ddl::{IndexMethod, ReferentialAction};
#[cfg(feature = "orm")]
pub use moso_sql::{
    AggregateFunc, DataType, Expr, Ident, RawExpr, SelectItem, TableRef, TypeRef, Value, ValueKind,
};

// ---------------------------------------------------------------------------
// Authorization — `permissions!`, `roles!`, `#[requires]`, `#[public]`
// ---------------------------------------------------------------------------
//
// Behind the `authz` feature, because `moso-authz` is. `permissions!` and
// `roles!` generate a `Perm` and a `Role` in the *user's* crate that implement
// the two traits below; `#[requires]` generates a `Requirement` marker and
// injects `Required<That>`; `#[public]` injects `Public`. Every path resolves
// here and nowhere else (decision D6). `crates/moso-authz-tests` compiles a real
// registry through all four and runs it, which is what keeps the two in step.

/// `moso_authz::Error`, under a non-colliding name.
///
/// `Error` in this module is `moso_core::Error`, which every generated `?`
/// names. The authorization error converts into it with `From`, so generated
/// code never needs both — but a hand-written impl might, and a `__private`
/// module can only hold one `Error`.
#[cfg(feature = "authz")]
pub use moso_authz::Error as AuthzError;
/// The registry fingerprint `permissions!` emits as a `const`.
#[cfg(feature = "authz")]
pub use moso_authz::perm::fingerprint_of;
#[cfg(feature = "authz")]
pub use moso_authz::{
    AUTHZ_EXTENSION, Action, Actor, ActorId, ActorKind, ActorPermissions, ActorSource, AuditConfig,
    AuditOutcome, AuditRecord, AuditSink, Authorized, AuthorizedQuery, AuthzDeclaration, Decision,
    Explanation, FromPath, FromPathId, HasRole, MAX_PERMISSIONS, MAX_ROLES, Masked, Obligation,
    PathName, PermBits, PermRef, PermSet, Permission, PermissionRegistry, PermissionSource,
    PolicyCtx, PolicyRef, PolicyRegistry, Public, Redacted, RequireMode, Required, Requirement,
    Requires, ResourceSource, Role, RoleAssignment, RoleSet, RoleSource, Scope, ScopeId, TraceStep,
    boot_problems, mark_public, read_declarations,
};
/// `moso_authz::Policy`, under a non-colliding name for the same reason.
#[cfg(feature = "authz")]
pub use moso_authz::{Policy as AuthzPolicy, ScopedPolicy as AuthzScopedPolicy};

// ---------------------------------------------------------------------------
// Background jobs — `#[job]`
// ---------------------------------------------------------------------------
//
// Behind the `jobs` feature, because `moso-jobs` is. `#[job]` turns an
// `async fn` into a unit struct implementing `Job`, resolves each `Inject(..)`
// parameter through `JobCtx`, and folds the attribute's `queue`, `retries`,
// `backoff`, `timeout`, `unique_for`, `priority` and `serial` into associated
// constants. Every path it emits resolves here and nowhere else (decision D6).

/// `moso_jobs::Error`, under a non-colliding name.
///
/// `Error` in this module is `moso_core::Error`. A job body's `?` converts into
/// the job error with `From`, and a `__private` module can only hold one
/// `Error`.
#[cfg(feature = "jobs")]
pub use moso_jobs::Error as JobError;
/// `moso_jobs::Result`, under a non-colliding name for the same reason.
///
/// This is what a generated `Job::run` returns, so a job body written as
/// `-> Result<()>` against the prelude's `Result` still lines up: both are
/// `core::result::Result` with a job error.
#[cfg(feature = "jobs")]
pub use moso_jobs::Result as JobResult;
#[cfg(feature = "jobs")]
pub use moso_jobs::{
    Backoff, DEFAULT_QUEUE, DEFAULT_RETRIES, DEFAULT_TIMEOUT, Enqueue, EnqueueBuilder, Job, JobCtx,
    JobId, JobRegistry, Jobs, Priority, RetryPolicy,
};
