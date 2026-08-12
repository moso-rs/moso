#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "Moso: a batteries-included, model-driven web framework for Rust."]
//!
//! You probably want [`prelude`] — `use moso::prelude::*` is the first line of
//! every Moso application, and the ~30 names it brings in cover the whole of
//! the tutorial. The design behind this crate is `docs/00-foundations/02-architecture.md`;
//! the item-by-item reference is `docs/06-reference/61-api-reference.md`.
//!
//! This is the only crate an application depends on. It contains no logic of
//! its own: it re-exports the runtime crates, provides the [`prelude`], and owns
//! the hidden `__private` module that macro output resolves against.
//!
//! ```
//! use moso::prelude::*;
//!
//! /// A user, as the API accepts one.
//! #[derive(Schema)]
//! pub struct CreateUser {
//!     /// Public handle.
//!     #[schema(len = 3..=32)]
//!     pub username: String,
//!     /// Contact address.
//!     pub email: Email,
//! }
//!
//! /// A user, as the API returns one.
//! #[derive(Schema)]
//! pub struct UserOut {
//!     /// Stable identifier.
//!     pub id: u64,
//!     /// Public handle.
//!     pub username: String,
//! }
//!
//! /// Create a user.
//! #[endpoint]
//! async fn create(Json(body): Json<CreateUser>) -> Result<Created<UserOut>> {
//!     let id = 1;
//!     Ok(Created::at(
//!         format!("/users/{id}"),
//!         UserOut { id, username: body.username },
//!     ))
//! }
//!
//! /// Everything this module serves.
//! pub fn router() -> Router {
//!     moso::routes! { POST "/users" => create }.tag("users")
//! }
//! # fn main() { assert_eq!(router().len(), 1); }
//! ```
//!
//! The body is parsed, validated and rejected with a
//! `application/problem+json` document *before* `create` runs, and the OpenAPI
//! operation — path, request schema, `201` response schema, tag — is derived
//! from the same signature. There is no second description of this endpoint to
//! keep in sync.
//!
//! # Where things live
//!
//! | Path | Contents |
//! | --- | --- |
//! | [`prelude`] | the ~35 names an application actually types |
//! | [`extract`] | `Json`, `Path`, `Query`, `Headers`, `Inject`, `Depends`, `Cookies`, … |
//! | [`response`] | `Created`, `NoContent`, `Page`, `Redirect`, `Sse`, `File`, `Either`, … |
//! | [`config`] | `Config`, layered sources, `SecretString`, `Profile` |
//! | [`mod@middleware`] | `MiddlewareStack`, `Slot`, `Next`, `Guard` |
//! | [`schema`] | `Schema`, `Validate`, `Email`, `Slug`, `Id`, the JSON Schema model |
//! | [`openapi`] | the OpenAPI 3.1 document model and its builders |
//! | [`task`] | `blocking`, for the CPU-bound work that must not stall the runtime |
//! | [`shutdown`] | `Signal`, for handlers that outlive a request |
//! | [`deps`] | the third-party crates whose types appear in Moso's API |
//!
//! # Feature flags
//!
//! | Feature | Default | Effect |
//! | --- | --- | --- |
//! | `http` | yes | accepted and inert; `moso-core` is unconditional |
//! | `openapi` | yes | mounts `/docs` and `/openapi.json` |
//! | `tracing` | yes | installs the tracing layer in the default middleware stack |
//! | `compression` | no | response compression |
//! | `cors` | no | the CORS layer |
//! | `multipart` | no | multipart bodies |
//! | `ws` | no | WebSocket upgrades |
//! | `orm` | no | `db`, `sql` and `#[derive(Entity)]` — the data layer |
//! | `auth` | no | `auth` — sessions, passwords, JWT, API keys, OAuth, passkeys |
//! | `kv` | no | `kv` — namespaces, caching, locks, rate limiting (no driver) |
//! | `mail` | no | `mail` — the `Mailer`, templates and the dev inbox (no driver) |
//! | `storage` | no | `storage` — streamed uploads and typed attachments (no driver) |
//! | `full` | no | every battery and optional layer at once, for measurement |
//!
//! The document is generated whatever `openapi` says; the feature decides only
//! whether the routes that *serve* it are mounted, so `moso openapi export`
//! works in every build.
//!
//! `orm` is off by default and must stay off: the topology rule in
//! `docs/00-foundations/02-architecture.md` is that the facade pulls no database
//! driver unless it is asked to, so a stateless service compiles no SQL at all.
//! `auth` implies it, because a user lives in a table.
//!
//! `kv`, `mail` and `storage` are the three batteries that break that link:
//! each depends only on `moso-core`, `moso-schema` and `moso-openapi`, so a
//! `--features kv,mail,storage` build still pulls no database driver. A cache,
//! a mailer and an object store are things a stateless service legitimately
//! needs, and none of them is a reason to compile an ORM. The convenience
//! `full` feature turns on everything at once — including a driver — and exists
//! for measuring the widest the dependency graph gets, not for production.

pub mod prelude;

#[doc(hidden)]
#[path = "private.rs"]
pub mod __private;

// ---------------------------------------------------------------------------
// Modules, re-exported wholesale from the runtime crates
// ---------------------------------------------------------------------------

#[doc(inline)]
pub use moso_core::{
    app, config, ctx, deps, di, error, extract, handler, health, http_config, middleware, openapi,
    response, router, schema, shutdown, task,
};

/// Installing the process's `tracing` subscriber, and — behind `otel` — its
/// OpenTelemetry OTLP exporter.
///
/// Behind the `subscriber` feature (OFF by default), because its surface needs
/// `tracing-subscriber` compiled in and the default tree is kept lean.
#[cfg(feature = "subscriber")]
#[doc(inline)]
pub use moso_core::observability;

// ---------------------------------------------------------------------------
// The flat surface
// ---------------------------------------------------------------------------

#[doc(inline)]
pub use moso_core::{
    App, AppBuilder, AppState, BoxError, BoxFuture, Config, Dependency, Depends, Describe,
    Endpoint, Error, ErrorKind, Extract, ExtractBody, Guard, Handler, HandlerFn, HealthCheck,
    HealthStatus, Inject, IntoResponse, Lifespan, Limits, MethodRouter, MiddlewareStack, Next,
    Problem, ProviderReq, Request, RequestCtx, Resolver, Response, Result, Route, Router, Signal,
    Slot,
};

/// Every procedural macro Moso ships, named one by one.
///
/// Listed rather than globbed so that adding a macro to `moso-macros` is a
/// deliberate edit here — a reviewed line in the facade's public surface —
/// rather than a silent new export that a `pub use moso_macros::*` would leak
/// the moment it lands. The two crates are still developed independently; only
/// the fact of the re-export is now written down where it can be read.
///
/// The set is the attribute macros [`macro@endpoint`], [`macro@middleware`],
/// [`macro@requires`], [`macro@public`] and [`macro@job`]; the function-like
/// macros [`routes!`](macro@routes), [`ep!`](macro@ep),
/// [`permissions!`](macro@permissions) and [`roles!`](macro@roles); and the
/// derives [`Schema`](macro@Schema), [`Constrained`](macro@Constrained),
/// [`Responder`](macro@Responder), [`Dependency`](macro@Dependency),
/// [`Config`](macro@Config) and [`Error`](macro@Error). The `authz` macros
/// (`permissions!`, `roles!`, `#[requires]`, `#[public]`) and `#[job]` are
/// re-exported unconditionally, exactly as the glob exported them: each expands
/// to a path that only resolves behind its `authz` / `jobs` feature, so a use
/// without the feature is a `__private` resolution error, not a missing macro.
#[doc(inline)]
pub use moso_macros::{
    Config, Constrained, Dependency, Error, Responder, Schema, endpoint, ep, job, middleware,
    permissions, public, requires, roles, routes,
};

// ---------------------------------------------------------------------------
// The data layer — behind the `orm` feature
// ---------------------------------------------------------------------------

/// The ORM: entities, shape-stable queries, relations, transactions, pooling.
///
/// Spelled `db` and not `orm` because that is the name every design document
/// uses — `use moso::db::prelude::*;` opens a model file in
/// `docs/02-data/21-entities-queries.md` — while `orm` is the *cargo feature*
/// that decides whether the module is compiled at all.
///
/// ```
/// use moso::db::{Db, Entity, Select};
///
/// /// Every query over `E` is a `Select<E>`, whatever it was built from.
/// fn shape<E: Entity>(query: Select<E>) -> Select<E> {
///     query
/// }
/// ```
#[cfg(feature = "orm")]
#[doc(inline)]
pub use moso_orm as db;

/// The authentication battery: sessions, passwords, JWT, API keys, OAuth2/OIDC,
/// passkeys, TOTP and the account-lifecycle flows.
///
/// Spelled `auth` in both places, unlike [`db`], because the crate, the cargo
/// feature and the module a handler imports all mean the same thing here and
/// `docs/03-batteries/30-auth.md` writes it that way throughout.
///
/// Reached through the facade so that an application does not name `moso-auth`
/// in its own manifest and risk a version skew against the `moso-core` the
/// facade re-exports — the extractors are `Dependency` impls resolved by the
/// same provider map, so a skew there is a boot error, not a compile error.
///
/// ```
/// use moso::auth::{PasswordPolicy, PrincipalKind};
///
/// // Nobody is anybody until something authenticates them.
/// assert_eq!(PrincipalKind::default(), PrincipalKind::Anonymous);
/// assert!(!PrincipalKind::Anonymous.is_authenticated());
///
/// // Length and breach, not composition — the current NIST position.
/// assert_eq!(PasswordPolicy::default().min_length, 12);
/// assert!(PasswordPolicy::default().breach_check);
/// ```
#[cfg(feature = "auth")]
#[doc(inline)]
pub use moso_auth as auth;

/// The background-job battery: `Job`, `Jobs`, `Worker`, `Scheduler`.
///
/// `moso::jobs::prelude::*` is what a job module imports, exactly as
/// `docs/03-batteries/32-jobs.md` writes it.
///
/// ```
/// use moso::jobs::{Backoff, Priority};
///
/// assert_eq!(Priority::default(), Priority::Normal);
/// assert!(matches!(Backoff::default(), Backoff::Exponential { .. }));
/// ```
#[cfg(feature = "jobs")]
#[doc(inline)]
pub use moso_jobs as jobs;

/// The key-value and cache battery: `Kv`, typed `namespace!`s, single-flight
/// caching, distributed locks and the GCRA rate limiter.
///
/// Spelled `kv` in both places, like [`jobs`], because the crate, the cargo
/// feature and the module a handler imports all mean the same thing and
/// `docs/02-data/25-kv-cache.md` writes it that way — `moso::kv::namespace! { … }`
/// declares a namespace and `moso::kv::connect(&cfg.kv)` opens the store.
///
/// Reached through the facade so an application does not name `moso-kv` in its
/// own manifest and risk a version skew against the `moso-core` the facade
/// re-exports. Unlike [`auth`] this pulls no database driver: the default
/// backend is an in-process map, and Redis or PostgreSQL are `moso-kv`'s own
/// features.
///
/// ```
/// use moso::kv::{minutes, FailureMode};
///
/// // A cache degrades on failure; a session declares `on_failure = fail`.
/// // The default is to degrade — a missing cache must not take the request down.
/// assert_eq!(FailureMode::default(), FailureMode::Degrade);
/// assert!(FailureMode::Degrade.degrades());
/// assert_eq!(minutes(15).as_secs(), 900);
/// ```
#[cfg(feature = "kv")]
#[doc(inline)]
pub use moso_kv as kv;

/// The mail battery: a framework-owned `Mailer`, compile-checked templates, a
/// suppression list, verified provider webhooks and the `/_mail` dev inbox.
///
/// Spelled `mail` in both places, like [`auth`], because the crate, the cargo
/// feature and the module all mean the same thing — `moso::mail::prelude::*`
/// opens a message module and `moso::mail::from_config(&cfg.mail)?` builds the
/// mailer, exactly as `docs/03-batteries/34-mail-storage-realtime.md` writes it.
///
/// Reached through the facade for the same version-skew reason as [`kv`], and
/// like it pulls no database driver: the default `console` and `memory`
/// backends need no service, and SMTP and the REST providers are `moso-mail`'s
/// own features.
///
/// ```
/// use moso::mail::MailBackendKind;
///
/// // `console` prints and serves the preview inbox — the development default.
/// assert_eq!(MailBackendKind::default(), MailBackendKind::Console);
/// assert_eq!(MailBackendKind::parse("smtp"), Some(MailBackendKind::Smtp));
/// ```
#[cfg(feature = "mail")]
#[doc(inline)]
pub use moso_mail as mail;

/// The object-storage battery: streamed uploads with magic-byte sniffing,
/// presigned direct upload, multipart, and typed entity attachments.
///
/// Spelled `storage` in both places, because the crate, the cargo feature and
/// the module all mean the same thing — `docs/03-batteries/34-mail-storage-realtime.md`
/// writes `moso::storage` throughout.
///
/// Reached through the facade for the same version-skew reason as [`kv`], and
/// like it pulls no database driver: the default `local` and `memory` backends
/// need no service, and S3, GCS and Azure are `moso-storage`'s own features.
/// The feature does turn on `moso-core`'s `multipart`, because an [`Upload<K>`]
/// is a `multipart/form-data` body.
///
/// [`Upload<K>`]: moso_storage::Upload
///
/// ```
/// use moso::storage::StorageKey;
///
/// // The only way to name an object — and it refuses to gain a level silently.
/// let key = StorageKey::from_segments(["avatars", "usr_123", "original.png"]).unwrap();
/// assert_eq!(key.as_str(), "avatars/usr_123/original.png");
/// assert!(StorageKey::from_segments(["a/b"]).is_err());
/// ```
#[cfg(feature = "storage")]
#[doc(inline)]
pub use moso_storage as storage;

/// The sealed SQL facade the ORM builds its statements with (ADR-0005).
///
/// Reached through the facade so that an application that needs a raw
/// [`db::RawQuery`] does not have to name `moso-sql` in its own manifest and
/// risk a version skew.
///
/// ```
/// use moso::sql::Value;
///
/// let bound = Value::text("Ada");
/// assert!(!bound.is_null());
/// ```
#[cfg(feature = "orm")]
#[doc(inline)]
pub use moso_sql as sql;

/// The ORM's macros: `#[derive(Entity)]` and everything that travels with it.
///
/// Listed rather than globbed — unlike [`moso_macros`] above — because this set
/// only exists behind the `orm` feature and a glob would silently export
/// whatever the macro crate grows next into a feature-gated namespace.
///
/// ```
/// use moso::db::prelude::*;
/// use moso::Entity;
///
/// /// One row of `widgets`.
/// #[derive(Entity, Debug, Clone)]
/// pub struct Widget {
///     /// The primary key.
///     #[entity(pk)]
///     pub id: i64,
///     /// What it is called.
///     pub name: String,
/// }
///
/// assert_eq!(<Widget as moso::db::Entity>::TABLE.name().as_str(), "widgets");
/// ```
#[cfg(feature = "orm")]
#[doc(inline)]
pub use moso_orm_macros::{DbEnum, Embedded, Entity, Factory, Projection, migration, sql};

// ---------------------------------------------------------------------------
// Version metadata
// ---------------------------------------------------------------------------

/// The version of Moso this application is built against.
///
/// Reported by `moso --version`, written into the generated OpenAPI document's
/// `x-generator`, and included in the `/readyz` body — so a running instance can
/// always say which framework version produced it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The header carrying the correlation id, on the way in and on the way out.
///
/// Re-exported from `moso-core` because it is part of the wire contract, and
/// anything that has to set or read that header — a client, a test harness, a
/// proxy shim — depends on the facade rather than on `moso-core`.
#[doc(inline)]
pub use moso_core::REQUEST_ID_HEADER;

/// The `$ref` prefix under which generated component schemas are published.
///
/// Re-exported for the same reason: a tool that resolves a `$ref` in a generated
/// document should not have to add `moso-openapi` to its manifest to name the
/// prefix it must strip.
#[doc(inline)]
pub use moso_core::COMPONENTS_SCHEMAS_PREFIX;

#[cfg(test)]
#[allow(
    dead_code,
    reason = "the bound-check functions exist to be type-checked, not called"
)]
mod tests {
    use crate::__private as p;

    // Naming a trait in a bound is enough to prove its `__private` path
    // resolves; several of these have no implementer inside this crate, which
    // is exactly why the check is a bound and not a call.
    fn endpoint<T: p::Endpoint>() {}
    fn handler_fn<T: p::HandlerFn>() {}
    fn handler<T: p::Handler<M>, M>() {}
    fn extract<T: p::Extract>() {}
    fn extract_body<T: p::ExtractBody>() {}
    fn describe<T: p::Describe>() {}
    fn dependency<T: p::Dependency>() {}
    fn guard<T: p::Guard>() {}
    fn schema<T: p::Schema>() {}
    fn validate<T: p::Validate>() {}
    fn config<T: p::Config>() {}
    fn config_source<T: p::ConfigSource>() {}
    fn coerce<T: p::Coerce>() {}
    fn into_response<T: p::IntoResponse>() {}
    fn custom_layer<T: p::CustomLayer>() {}

    /// Resolve every path `#[endpoint]` emits, through the same `::moso::…`
    /// spelling generated code uses.
    ///
    /// A missing `__private` re-export breaks every macro at once, in user
    /// code, with a span pointing at generated tokens. This test moves that
    /// failure here, where it costs a second to diagnose.
    #[test]
    fn generated_endpoint_paths_resolve() {
        endpoint::<p::UndocumentedEndpoint>();
        extract::<p::RequestId>();
        extract_body::<p::Text>();
        describe::<p::NoContent>();
        into_response::<p::NoContent>();
        schema::<String>();
        validate::<String>();

        let _: fn(&mut p::OperationBuilder) = |_| {};
        let _: Option<p::ProviderReq> = None;
        let _: Option<p::SchemaNode> = None;
        let _: Option<p::SchemaGenerator> = None;
        let _: Option<p::ValidationCtx> = None;
        let _: Option<p::ValidationErrors> = None;
        let _: Option<p::Param> = None;
        let _: Option<p::ResponseSpec> = None;
        let _: Option<p::ContentType> = None;
        let _: Option<p::SourceLocation> = None;
        let _: Option<p::Error> = None;
        let _: Option<p::Response> = None;
        let _: Option<p::Request> = None;
        let _: Option<p::RequestCtx> = None;
        let _: Option<p::BoxFuture<'static, ()>> = None;
        let _: Option<p::RouteMetadata> = None;
        let _: Option<p::HttpMethod> = None;
        let _: Option<p::SecurityRequirement> = None;
        let _: Option<p::ConfigDescriptor> = None;
        let _: Option<p::FieldDescriptor> = None;
        let _: Option<p::SecretString> = None;
    }

    /// `concat_reqs!` resolves and evaluates through the facade path.
    #[test]
    fn concat_reqs_resolves_through_the_facade() {
        const A: &[p::ProviderReq] = &[p::ProviderReq::of::<u8>()];
        const BOTH: &[p::ProviderReq] = p::concat_reqs!(A, A);
        assert_eq!(BOTH.len(), 2);
    }

    /// One of the `check_*` helpers a generated `Validate` body calls, reached
    /// through the glob re-export.
    #[test]
    fn validation_helpers_resolve_through_the_facade() {
        let mut errors = p::ValidationErrors::new();
        p::check_required(None::<&u8>, "/field", &mut errors);
        assert_eq!(errors.len(), 1);
    }
}
