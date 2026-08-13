#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "The Moso runtime core."]
//!
//! `App`, `Router`, `Handler`, `Extract`, `Error`, dependency injection,
//! configuration and the default middleware stack. Axum is the engine; this
//! crate is the cockpit.
//!
//! # Map of the crate
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`app`] | [`App`], [`AppBuilder`], [`AppState`], [`Resolver`], the boot sequence |
//! | [`router`] | [`Router`], [`MethodRouter`], [`RouteEntry`], the route table |
//! | [`handler`] | [`Handler`], [`Endpoint`], [`HandlerFn`], the arity impls |
//! | [`extract`] | [`Extract`], [`ExtractBody`] and every built-in extractor |
//! | [`response`] | [`Describe`] and every built-in response type |
//! | [`di`] | [`Inject`], [`Depends`], [`Dependency`], [`ProviderReq`] |
//! | [`ctx`] | [`RequestCtx`], the per-request dependency cache, [`Limits`] |
//! | [`error`] | [`Error`], [`ErrorKind`], RFC 9457 rendering, the boot report |
//! | [`middleware`] | [`MiddlewareStack`], [`Slot`], [`Next`], [`Guard`] |
//! | [`config`] | [`Config`], layered sources, [`SecretString`] |
//! | [`health`] | [`HealthCheck`], `/healthz` and `/readyz` |
//! | [`shutdown`] | [`Signal`], [`Drain`], the graceful drain |
//! | [`task`] | [`task::blocking`], the bounded [`BlockingPool`] |
//!
//! # Three invariants worth stating up front
//!
//! **[`Router`] is not generic over state.** There is no `Router<S>` and no
//! `FromRef`. Application state lives in the provider map and is read with
//! [`Inject<T>`](Inject), which is infallible at the use site *because*
//! [`AppBuilder::build`](app::AppBuilder::build) proved the provider exists.
//! This single decision deletes the largest family of trait-resolution errors
//! an Axum user meets, and the largest source of monomorphisation.
//!
//! **Validation happens inside extraction.** There is no way to obtain a `T`
//! out of a request without `T::validate` having run — see
//! [`Json<T>`](extract::Json).
//!
//! **Nothing is registered by link-time magic.** No `inventory`, no `ctor`.
//! Every route, provider, job and permission is registered by a statement you
//! can read.
//!
//! # Relationship with Axum
//!
//! [`IntoResponse`] is a *re-export* of Axum's trait, not a parallel one, so
//! every response type in the ecosystem works here unchanged. [`Extract`] and
//! [`ExtractBody`] are supersets of `FromRequestParts`/`FromRequest`: the same
//! job plus a `describe` method that contributes to the OpenAPI operation.
//! [`Opaque<T>`](extract::Opaque) adapts any Axum extractor into a Moso
//! handler, and [`Router::into_axum`] hands back the plain `axum::Router` at
//! the edge of the application.

pub mod app;
pub mod config;
pub mod ctx;
pub mod di;
pub mod error;
pub mod extract;
pub mod handler;
pub mod health;
pub mod http_config;
pub mod middleware;
#[cfg(feature = "subscriber")]
pub mod observability;
pub mod response;
pub mod router;
pub mod shutdown;
pub mod task;

// ---------------------------------------------------------------------------
// Substrate re-exports
// ---------------------------------------------------------------------------

/// The OpenAPI 3.1 document model and its builders.
///
/// `moso-core` depends on this crate unconditionally: `Extract::describe` takes
/// an `&mut OperationBuilder` whatever the feature set, and a trait signature
/// that changes with a cargo feature is a trap. The `openapi` feature controls
/// only whether the `/docs` and `/openapi.json` routes are mounted.
pub use moso_openapi as openapi;
/// The model layer: `Schema`, `Validate`, the JSON Schema types and the
/// constrained types (`Email`, `Slug`, `Password`, …).
pub use moso_schema as schema;

/// The third-party crates whose types appear in Moso's public API.
///
/// Re-exported so an application can name them without adding a dependency
/// whose version might drift from the one Moso compiled against. Their major
/// versions are part of Moso's semver contract.
pub mod deps {
    pub use {axum, bytes, http, serde, serde_json, tokio, tower, tower_http, tracing};
}

/// A request, exactly as Axum models it: `http::Request<axum::body::Body>`.
pub use axum::extract::Request;
/// Axum's response trait, re-exported rather than reinvented.
///
/// Anything that is an Axum response is a Moso response. What Moso adds on top
/// is [`Describe`], which says what the response *means* for the API contract.
pub use axum::response::IntoResponse;
/// A response, exactly as Axum models it: `http::Response<axum::body::Body>`.
pub use axum::response::Response;

/// A boxed, `Send` future with an explicit lifetime.
///
/// Used wherever a trait must stay dyn-compatible, and as the return type of
/// [`Handler::call`] and [`HandlerFn::invoke`] so that a handler compiles to
/// exactly one concrete future regardless of how many extractors it has.
pub type BoxFuture<'a, T> = core::pin::Pin<Box<dyn core::future::Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// Flat re-exports — the names an application actually types
// ---------------------------------------------------------------------------

pub use crate::app::{App, AppBuilder, AppState, Lifespan, Resolver};
pub use crate::config::{
    Config, ConfigDescriptor, ConfigLoader, ConfigSource, FieldDescriptor, Profile, SecretBytes,
    SecretString,
};
pub use crate::ctx::{DependencyCache, Limits, RequestCtx};
pub use crate::di::{Dependency, Depends, Inject, ProviderMap, ProviderReq};
pub use crate::error::{BootError, BootErrors, BoxError, Error, ErrorKind, Problem, Result};
pub use crate::extract::{Extract, ExtractBody, Opaque};
pub use crate::handler::{Endpoint, Handler, HandlerFn, UndocumentedEndpoint};
pub use crate::health::{HealthCheck, HealthReport, HealthStatus};
pub use crate::http_config::{HttpConfig, ServerConfig, TracingConfig};
pub use crate::middleware::{CustomLayer, Guard, MiddlewareStack, Next, Slot, StackEntry};
#[cfg(feature = "subscriber")]
pub use crate::observability::{TracingGuard, init as init_tracing};
pub use crate::response::Describe;
pub use crate::router::{MethodRouter, Route, RouteEntry, RouteService, Router, StaticSource};
pub use crate::shutdown::{Drain, ShutdownGuard, Signal};
pub use crate::task::BlockingPool;

/// The `$ref` prefix under which generated component schemas are published.
pub const COMPONENTS_SCHEMAS_PREFIX: &str = moso_openapi::COMPONENTS_SCHEMAS_PREFIX;

/// The header carrying the correlation id, on the way in and on the way out.
pub const REQUEST_ID_HEADER: &str = "x-request-id";
