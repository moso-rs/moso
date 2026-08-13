//! The names an application types, and nothing else.
//!
//! You probably want the glob. Import it once at the top of every module that
//! defines endpoints or models; everything else Moso ships is one explicit path
//! away. The rationale for keeping this list short is in
//! `docs/00-foundations/02-architecture.md`.
//!
//! ```
//! use moso::prelude::*;
//!
//! /// A blog post.
//! #[derive(Schema)]
//! pub struct Post {
//!     /// URL-safe identifier.
//!     pub slug: Slug,
//!     /// Headline.
//!     #[schema(len = 1..=200)]
//!     pub title: String,
//! }
//!
//! /// Show one post.
//! #[endpoint]
//! async fn show(Path(slug): Path<Slug>) -> Result<Json<Post>> {
//!     Ok(Json(Post { slug, title: "Hello".to_owned() }))
//! }
//!
//! # fn main() {
//! let router: Router = moso::routes! { GET "/posts/{slug}" => show };
//! assert_eq!(router.len(), 1);
//! # }
//! ```
//!
//! # The 40-item rule
//!
//! The prelude MUST NOT exceed 40 items and every item in it MUST be needed by
//! the tutorial application. This is a hard rule, not a guideline: a prelude
//! that grows collides with application names, makes `use moso::prelude::*`
//! something a reader cannot reason about, and turns every addition to the
//! framework into a potential breaking change for someone.
//!
//! Anything not here is one path away — `moso::extract::Cookies`,
//! `moso::response::Sse`, `moso::config::Profile`, `moso::openapi::Document` —
//! and the path says where it comes from, which is usually an improvement.
//!
//! Current count: **28 named items** plus the 15 macros the macro crate
//! exports. The macro list is now explicit rather than a glob, so the set is
//! the one written in the `macros` section below and adding to it is a
//! deliberate edit — the `authz` and `jobs` macros (`permissions!`, `roles!`,
//! `#[requires]`, `#[public]`, `#[job]`) ride along, resolving only behind
//! their feature.
//!
//! # What is deliberately absent
//!
//! - `Headers`, `Cookies`, `Form`, `Redirect`, `Sse`, `File`, `Cached` — real
//!   but not universal; import them where you use them.
//! - `ExtractBody`, `Describe`, `Handler` — implemented by the framework and by
//!   macros, rarely by hand.
//! - `Validate`, `SchemaNode`, `SchemaGenerator` — `#[derive(Schema)]` writes
//!   the code that needs them.
//! - `Profile`, `MiddlewareStack`, `Slot` — used once, in the composition root,
//!   where an explicit path reads better.

// ── application ───────────────────────────────────────────────────────────
pub use moso_core::{App, Router};

// ── errors ────────────────────────────────────────────────────────────────
pub use moso_core::{Error, Result};

// ── extraction ────────────────────────────────────────────────────────────
pub use moso_core::extract::{Depends, Form, Inject, Json, Path, Query};
pub use moso_core::{Dependency, Extract, RequestCtx};

// ── responses ─────────────────────────────────────────────────────────────
pub use moso_core::IntoResponse;
pub use moso_core::response::{Created, Empty, NoContent, Page};

// ── documentation ─────────────────────────────────────────────────────────
pub use moso_core::openapi::{OperationBuilder, ResponseSpec, SecurityScheme};

// ── the model layer ───────────────────────────────────────────────────────
pub use moso_core::schema::{Cursor, Email, Id, Schema, Slug};

// ── configuration ─────────────────────────────────────────────────────────
pub use moso_core::config::{Config, SecretString};

// ── macros ────────────────────────────────────────────────────────────────
//
// Every proc macro `moso-macros` exports, named one by one rather than globbed,
// so a new macro is a deliberate edit rather than a silent addition to the
// prelude — the one namespace where a new name is most likely to collide with a
// user's own. The attribute macros `#[endpoint]`, `#[middleware]`,
// `#[requires]`, `#[public]`, `#[job]`; the function-like `routes!`, `ep!`,
// `permissions!`, `roles!`; the derives `Schema`, `Constrained`, `Responder`,
// `Dependency`, `Config`, `Error`.
//
// The derives share names with the traits above; that is legal and intended —
// a trait lives in the type namespace and a derive in the macro namespace, so
// `#[derive(Schema)] struct T` and `T: Schema` both resolve. The `authz` and
// `jobs` macros are re-exported unconditionally: each expands to a path that
// only resolves behind its feature, so a use without the feature is a
// resolution error, not a missing name.
pub use moso_macros::{
    Config, Constrained, Dependency, Error, Responder, Schema, endpoint, ep, job, middleware,
    permissions, public, requires, roles, routes,
};
