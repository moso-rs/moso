#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "OpenAPI 3.1 for Moso."]
//!
//! An owned OpenAPI 3.1.1 document model plus the builders that `moso-core`
//! drives while routes are registered, and the self-hosted documentation UI.
//!
//! # The shape of the thing
//!
//! The OpenAPI document is a **derived artefact**. Nothing in it is written by
//! hand: `#[endpoint]` turns a handler's doc comment and parameter types into a
//! sequence of calls on [`OperationBuilder`], extractors and response types
//! contribute their own fragments through the same builder, and `App::build()`
//! merges the result into a [`Document`].
//!
//! ```text
//! #[endpoint]  →  Endpoint::spec(&mut OperationBuilder)
//!                      │  extractors call op.parameter(..) / op.request_body(..)
//!                      │  response types call op.response(..)
//!                      ▼
//! Router::get(path, h)  →  OperationSpec merged at (method, path)
//!                      │  router-level .tag() / .security() / .responds() overlaid
//!                      ▼
//! App::build()  →  DocumentBuilder::build() → Document
//! ```
//!
//! # What is in here
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`document`] | [`Document`], [`Info`], [`Server`], [`Tag`], [`Components`] |
//! | [`path`] | [`PathItem`], [`Operation`], [`Parameter`], [`Response`], [`MediaType`] |
//! | [`security`] | [`SecurityScheme`], [`SecurityRequirement`], OAuth flow objects |
//! | [`builder`] | [`DocumentBuilder`], [`OperationBuilder`], [`ResponseSpec`], [`Param`] |
//! | [`ui`] | the embedded, network-free documentation UI |
//! | [`diff`](mod@diff) | document diffing for `moso openapi check` |
//!
//! # Determinism
//!
//! Every map in the model is an [`indexmap::IndexMap`]. The emitted document is
//! byte-stable for a given application, which is what makes a committed
//! `openapi.json` diff cleanly and makes `moso openapi check` meaningful.
//! [`Document::sort_for_output`] applies the canonical ordering: components
//! sorted by name, paths sorted lexicographically, responses sorted by status.
//!
//! # Independence
//!
//! This crate knows about JSON Schema (through `moso-schema`) and about the
//! OpenAPI wire format. It knows nothing about HTTP servers, Axum, or Moso's
//! runtime, and it never performs I/O.

pub mod builder;
pub mod diff;
pub mod document;
pub mod path;
pub mod security;
pub mod ui;

pub use builder::{
    ContentType, DEFAULT_VERSION, DocumentBuilder, OperationBuilder, OperationSpec,
    PROBLEM_SCHEMA_NAME, Param, ResponseSpec, RouteMetadata, SourceLocation,
    VALIDATION_PROBLEM_SCHEMA_NAME, problem_schema, validation_problem_schema,
};
pub use diff::{Change, ChangeKind, DiffOptions, diff, format_changes, has_breaking};
pub use document::{
    Components, Contact, Document, DocumentError, ExternalDocs, Info, License, Server,
    ServerVariable, Tag, etag_for,
};
pub use path::{
    Encoding, Example, Header, HttpMethod, Link, MediaType, Operation, Parameter,
    ParameterLocation, ParameterStyle, PathItem, Response,
};
pub use security::{ApiKeyLocation, OAuthFlow, OAuthFlows, SecurityRequirement, SecurityScheme};
pub use ui::{DocsUi, Theme};

pub use moso_schema::json_schema::{SchemaGenerator, SchemaNode, SchemaRef};

/// The OpenAPI version this crate emits.
///
/// See ADR-0009: 3.1 aligns with JSON Schema 2020-12, so `#[derive(Schema)]`
/// output is embedded verbatim rather than lossily downgraded.
pub const OPENAPI_VERSION: &str = "3.1.1";

/// The JSON Schema dialect every schema in an emitted document conforms to.
pub const JSON_SCHEMA_DIALECT: &str = moso_schema::DIALECT;

/// The `$ref` prefix under which generated schemas are published.
pub const COMPONENTS_SCHEMAS_PREFIX: &str = moso_schema::DEFAULT_REF_PREFIX;

/// `serde` predicate: skip a `bool` field that is `false`.
pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}
