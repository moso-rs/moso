//! The builders `moso-core` drives while routes are registered.
//!
//! [`OperationBuilder`] is the hinge of the zero-annotation design. Every
//! extractor, every response type, every guard and every request-scoped
//! dependency writes into one, and the operation that comes out is the sum of
//! what the handler's *types* said about it. Nobody writes an OpenAPI fragment
//! by hand.
//!
//! This is how `moso-core` writes one (an excerpt, not a program):
//!
//! ```text
//! impl<T: Schema> ExtractBody for Json<T> {
//!     fn describe(op: &mut OperationBuilder) {
//!         let schema = op.generator().define::<T>();
//!         op.request_body(ContentType::Json, schema, true);
//!         op.response(422, ResponseSpec::validation_problem_of::<T>());
//!         op.response(400, ResponseSpec::problem("Malformed JSON"));
//!         if T::HAS_CONSTRAINTS {
//!             op.mark_validated();
//!         }
//!     }
//!     // …
//! }
//! ```
//!
//! # Merge semantics
//!
//! Several describers contribute to one operation and they must not fight.
//! The rules, applied by [`OperationSpec`], are:
//!
//! | Member | Rule |
//! | --- | --- |
//! | `summary`, `description`, `operationId`, `externalDocs` | **first writer wins**; later writers are ignored |
//! | `tags` | appended, deduplicated, insertion order preserved |
//! | `parameters` | keyed by `(in, name)`; first wins, later fills only absent members |
//! | `requestBody` | first wins; later calls add *content types* it did not describe |
//! | `responses` | keyed by status; first wins, later fills only absent members |
//! | `security` | appended unless an identical requirement is already present |
//! | `deprecated`, `hidden`, `validated` | sticky: once `true`, always `true` |
//! | `x-*` extensions | first writer wins per key |
//!
//! "First writer wins" is deterministic because `#[endpoint]` emits its
//! `describe` calls in declaration order, and router-level metadata
//! ([`RouteMetadata`]) is overlaid *after* the handler's own description — so
//! `Router::tag("users")` cannot clobber an operation that named its own tag.

use indexmap::IndexMap;
use indexmap::map::Entry;
use serde_json::Value;

use crate::document::{
    Components, Contact, Document, DocumentError, ExternalDocs, Info, License, Server, Tag,
};
use crate::path::{
    Header, HttpMethod, MediaType, Operation, Parameter, ParameterLocation, ParameterStyle,
    PathItem, RequestBody, Response,
};
use crate::security::{SecurityRequirement, SecurityScheme};
use moso_schema::Schema;
use moso_schema::json_schema::{
    AdditionalProperties, ArrayBuilder, NumberBuilder, ObjectBuilder, SchemaGenerator, SchemaNode,
    SchemaRef, StringBuilder,
};

/// A schema that is produced later, once a [`SchemaGenerator`] is available.
///
/// This is what lets `Param::query("limit").schema_of::<u32>()` be written in
/// argument position — `op.parameter(…)` already holds `&mut self`, so the
/// argument expression cannot borrow the builder's generator as well. The
/// thunk is a plain `fn` pointer, so [`Param`] and [`ResponseSpec`] stay
/// `Clone` and can be reused across every route of a router.
pub type SchemaThunk = fn(&mut SchemaGenerator) -> SchemaNode;

/// Where in the user's source an operation was defined.
///
/// Recorded by `#[endpoint]` through
/// [`OperationBuilder::source`] and surfaced by `moso openapi check`, which
/// prints `+ POST /users/{id}/deactivate (added in src/routes/users.rs:102)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLocation {
    /// The file, as `file!()` reports it.
    pub file: &'static str,
    /// The line, as `line!()` reports it.
    pub line: u32,
}

impl core::fmt::Display for SourceLocation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// The media type of a request or response representation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ContentType {
    /// `application/json`.
    Json,
    /// `application/problem+json` — the RFC 9457 error format.
    ProblemJson,
    /// `application/x-www-form-urlencoded`.
    Form,
    /// `multipart/form-data`.
    Multipart,
    /// `application/octet-stream` — an opaque byte stream or file download.
    OctetStream,
    /// `text/plain; charset=utf-8`.
    Text,
    /// `text/html; charset=utf-8`.
    Html,
    /// `text/event-stream` — server-sent events.
    EventStream,
    /// `application/yaml`.
    Yaml,
    /// `application/xml`.
    Xml,
    /// `application/x-ndjson` — newline-delimited JSON.
    NdJson,
    /// Anything else.
    Other(std::borrow::Cow<'static, str>),
}

impl ContentType {
    /// A media type not covered by the named variants.
    pub fn custom(value: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        ContentType::Other(value.into())
    }

    /// The media type string used as a `content` map key.
    pub fn as_str(&self) -> &str {
        match self {
            ContentType::Json => "application/json",
            ContentType::ProblemJson => "application/problem+json",
            ContentType::Form => "application/x-www-form-urlencoded",
            ContentType::Multipart => "multipart/form-data",
            ContentType::OctetStream => "application/octet-stream",
            ContentType::Text => "text/plain; charset=utf-8",
            ContentType::Html => "text/html; charset=utf-8",
            ContentType::EventStream => "text/event-stream",
            ContentType::Yaml => "application/yaml",
            ContentType::Xml => "application/xml",
            ContentType::NdJson => "application/x-ndjson",
            ContentType::Other(value) => value.as_ref(),
        }
    }
}

impl core::fmt::Display for ContentType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&'static str> for ContentType {
    fn from(value: &'static str) -> Self {
        ContentType::Other(std::borrow::Cow::Borrowed(value))
    }
}

// ---------------------------------------------------------------------------
// Problem schemas
// ---------------------------------------------------------------------------

/// The component name under which the RFC 9457 problem schema is published.
pub const PROBLEM_SCHEMA_NAME: &str = "Problem";

/// The component name under which the validation-failure schema is published.
pub const VALIDATION_PROBLEM_SCHEMA_NAME: &str = "ValidationProblem";

/// The response extension under which server-sent event payloads are listed.
///
/// OpenAPI has no vocabulary for the shape of an individual SSE event, so
/// [`ResponseSpec::sse_event`] records them here instead of pretending the
/// stream has one schema.
pub const SSE_EVENTS_EXTENSION: &str = "x-sse-events";

/// The `info.title` used when the application declared none.
///
/// An empty `title` is not a valid OpenAPI document, and refusing to boot over
/// a missing title would be a poor trade for something with an obvious default.
const DEFAULT_TITLE: &str = "API";

/// The `info.version` used when the application declared none.
///
/// Public because "did the application declare a version?" is a question other
/// crates ask: `/readyz` reports the application's version when there is one and
/// falls back to the environment otherwise, and it can only tell the two apart
/// by comparing against this placeholder.
pub const DEFAULT_VERSION: &str = "0.0.0";

/// The RFC 9457 `application/problem+json` object Moso emits for every error.
///
/// `type`, `title`, `status`, `detail`, `instance` per the RFC, plus Moso's
/// `request_id` and `trace_id` extension members. `additionalProperties` is
/// `true` because RFC 9457 permits arbitrary extension members and
/// `moso_core::Error::with_extension` uses them.
///
/// The members are exactly those of `moso_core::error::Problem`, in the same
/// order, so the schema and the serialiser cannot drift apart unnoticed.
pub fn problem_schema() -> SchemaNode {
    problem_object(PROBLEM_SCHEMA_NAME, None)
}

/// The problem object extended with `errors`, the per-field validation array.
///
/// Each entry carries an RFC 6901 JSON Pointer into the submitted document, a
/// code from `moso_schema::codes`, a human-readable message, and the code's
/// parameters.
///
/// The members are repeated rather than composed with
/// `allOf: [{$ref: Problem}]`: a `$ref` would only resolve if some *other*
/// describer had also registered `Problem`, and an operation that documents a
/// 422 without documenting a 400 is perfectly normal.
pub fn validation_problem_schema() -> SchemaNode {
    problem_object(
        VALIDATION_PROBLEM_SCHEMA_NAME,
        Some(
            ArrayBuilder::new()
                .description(
                    "Every field that failed, not just the first. Capped by \
                     `validation.max_errors`, default 50.",
                )
                .items(field_error_schema())
                .min_items(1)
                .build(),
        ),
    )
}

/// The shared body of [`problem_schema`] and [`validation_problem_schema`].
///
/// `errors` is threaded in rather than appended by the caller so that both
/// documents list their members in the wire order of
/// `docs/01-http/16-errors.md`.
fn problem_object(title: &'static str, errors: Option<SchemaNode>) -> SchemaNode {
    let mut object = ObjectBuilder::new()
        .title(title)
        .description(
            "An RFC 9457 problem document. Every Moso error is serialised in this \
             shape, as `application/problem+json`.",
        )
        .property(
            "type",
            StringBuilder::new()
                .description(
                    "A URI identifying the problem *class*. Dereferenceable \
                     documentation; defaults to `https://moso.rs/errors/{kind}`.",
                )
                .format("uri-reference")
                .default_value("about:blank")
                .example("https://moso.rs/errors/validation")
                .build(),
            true,
        )
        .property(
            "title",
            StringBuilder::new()
                .description("A short, human-readable summary of the problem class.")
                .example("Validation failed")
                .build(),
            true,
        )
        .property(
            "status",
            NumberBuilder::integer()
                .description("The HTTP status code, repeated in the body as RFC 9457 requires.")
                .minimum(100)
                .maximum(599)
                .example(422)
                .build(),
            true,
        )
        .property(
            "detail",
            StringBuilder::new()
                .description(
                    "What went wrong with *this* request. Absent when disclosure is \
                     refused, which is the default for any 5xx.",
                )
                .build(),
            false,
        )
        .property(
            "instance",
            StringBuilder::new()
                .description("The request path, identifying this specific occurrence.")
                .format("uri-reference")
                .example("/api/v1/users")
                .build(),
            false,
        );

    if let Some(errors) = errors {
        object = object.property("errors", errors, true);
    }

    object
        .property(
            "request_id",
            StringBuilder::new()
                .description("The correlation id, echoed in the `x-request-id` response header.")
                .example("01J8XG7K3RQZ4B0N2Y6M9C5V1T")
                .build(),
            false,
        )
        .property(
            "trace_id",
            StringBuilder::new()
                .description("The W3C trace id, present when a tracing context was propagated.")
                .build(),
            false,
        )
        .additional_properties(true)
        .build()
}

/// One entry of the `errors` array: a pointer, a code, a message, parameters.
fn field_error_schema() -> SchemaNode {
    ObjectBuilder::new()
        .title("FieldError")
        .description("One field-level failure, addressed by a JSON Pointer.")
        .property(
            "pointer",
            StringBuilder::new()
                .description(
                    "An RFC 6901 JSON Pointer into the submitted document \
                     (`/username`, `/tags/2`), or into a non-body source \
                     (`/query/limit`, `/path/id`, `/header/x-tenant`).",
                )
                .format("json-pointer")
                .example("/tags/2")
                .build(),
            true,
        )
        .property(
            "code",
            StringBuilder::new()
                .description(
                    "A stable code from the closed `moso_schema::codes` set — \
                     `required`, `type`, `len`, `range`, `pattern`, `format`, `enum`, \
                     `unique`, `multiple_of` — or an application code prefixed \
                     `custom:`. Clients branch on this, never on `message`.",
                )
                .example("len")
                .build(),
            true,
        )
        .property(
            "message",
            StringBuilder::new()
                .description(
                    "A human-readable message, localised through the application's \
                     `MessageProvider`. Never part of the contract.",
                )
                .example("must be between 1 and 24 characters")
                .build(),
            true,
        )
        .property(
            "params",
            ObjectBuilder::new()
                .description("The constraint's parameters, so a client can render its own message.")
                .additional_properties(true)
                .build(),
            false,
        )
        .build()
}

// ---------------------------------------------------------------------------
// Param
// ---------------------------------------------------------------------------

/// A parameter under construction.
///
/// Cheap to clone, so a router can hold one and apply it to every route it
/// carries.
///
/// Deliberately not `PartialEq`: a deferred schema is a `fn` pointer, and
/// comparing `fn` pointers does not produce a meaningful answer. Compare
/// [`Param::build`] output instead.
///
/// ```
/// use moso_openapi::{OperationBuilder, Param, ParameterLocation};
/// use moso_schema::json_schema::SchemaGenerator;
///
/// // `schema_of` defers schema generation, so it can be used in argument position
/// // while the builder is already mutably borrowed.
/// let mut op = OperationBuilder::new(SchemaGenerator::default());
/// op.parameter(Param::query("limit").schema_of::<u32>().required(false));
/// op.parameter(Param::path("id").schema_of::<u64>().description("Which user"));
///
/// let (spec, _) = op.finish();
/// assert_eq!(spec.parameters[0].location, ParameterLocation::Query);
/// assert!(!spec.parameters[0].required);
///
/// // A path parameter is required by definition; the flag cannot be unset.
/// assert!(spec.parameters[1].required);
/// ```
#[derive(Debug, Clone)]
pub struct Param {
    parameter: Parameter,
    deferred: Option<SchemaThunk>,
}

impl Param {
    /// A path parameter. Always required — a path parameter that is absent
    /// means a different route matched.
    pub fn path(name: impl Into<String>) -> Self {
        Self::new(ParameterLocation::Path, name)
    }

    /// A query-string parameter. Optional unless [`Param::required`] says otherwise.
    pub fn query(name: impl Into<String>) -> Self {
        Self::new(ParameterLocation::Query, name)
    }

    /// A request-header parameter.
    pub fn header(name: impl Into<String>) -> Self {
        Self::new(ParameterLocation::Header, name)
    }

    /// A single cookie.
    pub fn cookie(name: impl Into<String>) -> Self {
        Self::new(ParameterLocation::Cookie, name)
    }

    /// A parameter at an explicit location.
    pub fn new(location: ParameterLocation, name: impl Into<String>) -> Self {
        Self {
            parameter: Parameter::new(name, location),
            deferred: None,
        }
    }

    /// Whether the request is invalid without this parameter.
    ///
    /// Ignored for path parameters, which are required by definition.
    pub fn required(mut self, required: bool) -> Self {
        if self.parameter.location != ParameterLocation::Path {
            self.parameter.required = required;
        }
        self
    }

    /// What the parameter means. Normally the field's doc comment.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.parameter.description = Some(description.into());
        self
    }

    /// Describe the parameter with `T`'s schema, registering any named
    /// sub-schemas in `generator`.
    pub fn schema<T: Schema>(mut self, generator: &mut SchemaGenerator) -> Self {
        self.parameter.schema = Some(generator.subschema_for::<T>());
        self.deferred = None;
        self
    }

    /// Describe the parameter with `T`'s schema, resolved when the parameter is
    /// handed to [`OperationBuilder::parameter`].
    ///
    /// Use this when the builder is already mutably borrowed:
    ///
    /// ```
    /// use moso_openapi::{OperationBuilder, Param};
    /// use moso_schema::json_schema::SchemaGenerator;
    ///
    /// let mut op = OperationBuilder::new(SchemaGenerator::default());
    /// op.parameter(Param::query("limit").schema_of::<u32>().required(false));
    ///
    /// let (spec, _) = op.finish();
    /// assert_eq!(spec.parameters[0].name, "limit");
    /// assert!(!spec.parameters[0].required);
    /// ```
    ///
    /// The eager [`Param::schema`] does not compile here: `op.parameter(…)`
    /// already holds `&mut op`, so `op.generator()` cannot be borrowed inside
    /// the argument.
    pub fn schema_of<T: Schema>(mut self) -> Self {
        self.parameter.schema = None;
        self.deferred = Some(|generator| generator.subschema_for::<T>());
        self
    }

    /// Describe the parameter with an already-built schema node.
    pub fn schema_node(mut self, schema: SchemaNode) -> Self {
        self.parameter.schema = Some(schema);
        self.deferred = None;
        self
    }

    /// Describe the parameter with an already-resolved schema reference.
    pub fn schema_ref(self, schema: SchemaRef) -> Self {
        self.schema_node(schema.into())
    }

    /// Whether array and object members are sent as separate occurrences.
    pub fn explode(mut self, explode: bool) -> Self {
        self.parameter.explode = Some(explode);
        self
    }

    /// How the value is serialised into the request.
    pub fn style(mut self, style: ParameterStyle) -> Self {
        self.parameter.style = Some(style);
        self
    }

    /// Mark the parameter as one clients should stop sending.
    pub fn deprecated(mut self, deprecated: bool) -> Self {
        self.parameter.deprecated = deprecated;
        self
    }

    /// Whether an empty value is meaningful. Query parameters only.
    pub fn allow_empty_value(mut self, allow: bool) -> Self {
        self.parameter.allow_empty_value = allow;
        self
    }

    /// Attach a single example value.
    pub fn example(mut self, value: impl Into<Value>) -> Self {
        self.parameter.example = Some(value.into());
        self
    }

    /// Attach an `x-*` specification extension.
    pub fn extension(mut self, key: &'static str, value: impl Into<Value>) -> Self {
        self.parameter.extensions.insert(key.into(), value.into());
        self
    }

    /// Shorthand for `?filter[status]=open` style nesting: sets
    /// [`ParameterStyle::DeepObject`] and `explode: true`.
    pub fn deep_object(self) -> Self {
        self.style(ParameterStyle::DeepObject).explode(true)
    }

    /// The parameter's name.
    pub fn name(&self) -> &str {
        &self.parameter.name
    }

    /// Where the parameter is carried.
    pub fn location(&self) -> ParameterLocation {
        self.parameter.location
    }

    /// The parameter as built so far, before any deferred schema is resolved.
    pub fn as_parameter(&self) -> &Parameter {
        &self.parameter
    }

    /// Resolve any deferred schema and produce the wire object.
    pub fn build(self, generator: &mut SchemaGenerator) -> Parameter {
        let Self {
            mut parameter,
            deferred,
        } = self;
        if let Some(thunk) = deferred {
            parameter.schema = Some(thunk(generator));
        }
        parameter
    }
}

// ---------------------------------------------------------------------------
// ResponseSpec
// ---------------------------------------------------------------------------

/// A response under construction.
///
/// Cheap to clone, so `Router::responds(429, ResponseSpec::problem("Rate limited"))`
/// can be applied to every route in a subtree.
///
/// Deliberately not `PartialEq`, for the same reason as [`Param`]: a deferred
/// schema is a `fn` pointer. Compare [`ResponseSpec::build`] output instead.
///
/// ```
/// use moso_openapi::{ContentType, OperationBuilder, ResponseSpec};
/// use moso_schema::json_schema::SchemaGenerator;
///
/// let mut op = OperationBuilder::new(SchemaGenerator::default());
///
/// op.response(200, ResponseSpec::json_of::<String>());
/// op.response(404, ResponseSpec::problem("No such user"));
/// op.response(204, ResponseSpec::empty("Deleted"));
///
/// let (spec, _) = op.finish();
/// assert_eq!(spec.responses.len(), 3);
/// assert_eq!(
///     spec.responses["404"].content.keys().next().map(String::as_str),
///     Some(ContentType::ProblemJson.as_str()),
/// );
/// ```
#[derive(Debug, Clone)]
pub struct ResponseSpec {
    response: Response,
    deferred: Vec<(String, SchemaThunk)>,
}

impl ResponseSpec {
    /// A JSON response carrying `T`, registering `T`'s schema in `generator`.
    pub fn json<T: Schema>(generator: &mut SchemaGenerator) -> Self {
        let node = generator.subschema_for::<T>();
        Self::with_content(ContentType::Json, node).description(describe_type::<T>())
    }

    /// A JSON response carrying `T`, resolved when handed to
    /// [`OperationBuilder::response`].
    pub fn json_of<T: Schema>() -> Self {
        Self::deferred_content(ContentType::Json, |generator| {
            generator.subschema_for::<T>()
        })
        .description(describe_type::<T>())
    }

    /// An `application/problem+json` error response.
    ///
    /// The `Problem` schema is registered in `components.schemas` the first
    /// time any problem response is built, and referenced thereafter.
    pub fn problem(description: impl Into<String>) -> Self {
        Self::deferred_content(ContentType::ProblemJson, |generator| {
            generator
                .insert(PROBLEM_SCHEMA_NAME, problem_schema())
                .into()
        })
        .description(description)
    }

    /// The 422 emitted when `T` fails validation.
    ///
    /// The description names `T` so the documentation says *which* payload was
    /// rejected, and the schema is the shared `ValidationProblem`.
    pub fn validation_problem<T: Schema>(generator: &mut SchemaGenerator) -> Self {
        let node = generator
            .insert(VALIDATION_PROBLEM_SCHEMA_NAME, validation_problem_schema())
            .into();
        Self::with_content(ContentType::ProblemJson, node)
            .description(format!("`{}` failed validation", T::schema_name()))
    }

    /// The 422 emitted when `T` fails validation, resolved when handed to
    /// [`OperationBuilder::response`].
    pub fn validation_problem_of<T: Schema>() -> Self {
        Self::deferred_content(ContentType::ProblemJson, |generator| {
            generator
                .insert(VALIDATION_PROBLEM_SCHEMA_NAME, validation_problem_schema())
                .into()
        })
        .description(format!("`{}` failed validation", T::schema_name()))
    }

    /// A response with no body, such as `204 No Content`.
    pub fn empty(description: impl Into<String>) -> Self {
        Self {
            response: Response::new(description),
            deferred: Vec::new(),
        }
    }

    /// A binary download: `application/octet-stream` with `format: binary`.
    pub fn binary(description: impl Into<String>) -> Self {
        Self::with_content(
            ContentType::OctetStream,
            StringBuilder::new().format("binary").build(),
        )
        .description(description)
    }

    /// A `text/event-stream` response.
    ///
    /// OpenAPI cannot express the shape of individual events, so the event
    /// schemas are additionally listed under the `x-sse-events` extension —
    /// see [`ResponseSpec::sse_event`].
    pub fn sse(description: impl Into<String>) -> Self {
        Self::with_content(
            ContentType::EventStream,
            StringBuilder::new()
                .description(
                    "A `text/event-stream` body. The individual event payloads are \
                     listed under `x-sse-events`.",
                )
                .build(),
        )
        .description(description)
        .extension(SSE_EVENTS_EXTENSION, Value::Object(serde_json::Map::new()))
    }

    /// A `text/plain` response.
    pub fn text(description: impl Into<String>) -> Self {
        Self::with_content(
            ContentType::Text,
            SchemaNode::of_type(moso_schema::json_schema::JsonType::String),
        )
        .description(description)
    }

    /// A redirect: no body, a `Location` header.
    pub fn redirect(description: impl Into<String>) -> Self {
        Self::empty(description).header_spec(
            "location",
            Header::new(
                StringBuilder::new()
                    .format("uri-reference")
                    .example("/users/42")
                    .build(),
            )
            .with_description("Where the client should go next.")
            .required(),
        )
    }

    /// A response with an explicit media type and schema.
    pub fn with_content(content_type: ContentType, schema: SchemaNode) -> Self {
        let mut response = Response::default();
        response
            .content
            .insert(content_type.as_str().to_owned(), MediaType::new(schema));
        Self {
            response,
            deferred: Vec::new(),
        }
    }

    /// A response whose schema is produced once a generator is available.
    pub fn deferred_content(content_type: ContentType, schema: SchemaThunk) -> Self {
        Self {
            response: Response::default(),
            deferred: vec![(content_type.as_str().to_owned(), schema)],
        }
    }

    /// A `$ref` to a response declared with
    /// [`DocumentBuilder::shared_response`].
    pub fn shared(name: &str) -> Self {
        Self {
            response: Response::reference(name),
            deferred: Vec::new(),
        }
    }

    /// Set or replace the description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.response.description = Some(description.into());
        self
    }

    /// Document a response header.
    pub fn header(mut self, name: impl Into<String>, schema: SchemaNode) -> Self {
        self.response
            .headers
            .insert(name.into(), Header::new(schema));
        self
    }

    /// Document a response header with full control over the header object.
    pub fn header_spec(mut self, name: impl Into<String>, header: Header) -> Self {
        self.response.headers.insert(name.into(), header);
        self
    }

    /// Add a second representation of the same response.
    pub fn also(mut self, content_type: ContentType, schema: SchemaNode) -> Self {
        self.response
            .content
            .insert(content_type.as_str().to_owned(), MediaType::new(schema));
        self
    }

    /// Attach an example payload to every representation that has none.
    pub fn example(mut self, value: impl Into<Value>) -> Self {
        let value = value.into();
        for media in self.response.content.values_mut() {
            if media.example.is_none() {
                media.example = Some(value.clone());
            }
        }
        self
    }

    /// Declare one event type carried by a [`ResponseSpec::sse`] stream.
    ///
    /// Appends to the `x-sse-events` extension: `{"name": <schema>}`.
    pub fn sse_event(mut self, name: &str, schema: SchemaNode) -> Self {
        let entry = self
            .response
            .extensions
            .entry(SSE_EVENTS_EXTENSION.to_owned())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(serde_json::Map::new());
        }
        if let Value::Object(events) = entry {
            // `SchemaNode` serialises to a JSON object with string keys and
            // finite numbers, so this conversion cannot fail; `Null` is the
            // inert fallback rather than a panic on the documentation path.
            let value = serde_json::to_value(&schema).unwrap_or(Value::Null);
            events.insert(name.to_owned(), value);
        }
        self
    }

    /// Link to another operation reachable from this response.
    pub fn link(mut self, name: impl Into<String>, link: crate::path::Link) -> Self {
        self.response.links.insert(name.into(), link);
        self
    }

    /// Attach an `x-*` specification extension.
    pub fn extension(mut self, key: &'static str, value: impl Into<Value>) -> Self {
        self.response.extensions.insert(key.into(), value.into());
        self
    }

    /// Resolve any deferred schemas and produce the wire object.
    pub fn build(self, generator: &mut SchemaGenerator) -> Response {
        let Self {
            mut response,
            deferred,
        } = self;
        for (content_type, thunk) in deferred {
            let node = thunk(generator);
            response
                .content
                .entry(content_type)
                .or_insert_with(|| MediaType::new(node));
        }
        response
    }
}

/// The default description for a JSON response carrying `T`.
fn describe_type<T: Schema>() -> String {
    format!("`{}`", T::schema_name())
}

// ---------------------------------------------------------------------------
// OperationSpec
// ---------------------------------------------------------------------------

/// An operation's accumulated description, before it becomes wire format.
///
/// Distinct from [`Operation`] because it carries members that never reach the
/// document — [`OperationSpec::hidden`], [`OperationSpec::validated`],
/// [`OperationSpec::source`] — and because merging is defined on it.
///
/// ```
/// use moso_openapi::{OperationBuilder, OperationSpec};
/// use moso_schema::json_schema::SchemaGenerator;
///
/// let mut op = OperationBuilder::new(SchemaGenerator::default());
/// op.summary("List users").tag("users").tag("users");   // the duplicate is dropped
/// op.summary("Something else");                         // first writer wins
///
/// let spec: OperationSpec = op.into_spec();
/// assert_eq!(spec.summary.as_deref(), Some("List users"));
/// assert_eq!(spec.tags, ["users"]);
/// ```
///
/// The accumulator every contributor writes into. Its merge rules are what let an
/// extractor, a response type, a guard and the router all describe one operation
/// without fighting.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OperationSpec {
    /// One-line summary, from the handler's doc comment.
    pub summary: Option<String>,
    /// Long description, from the rest of the handler's doc comment.
    pub description: Option<String>,
    /// Unique identifier; client generators use it as a method name.
    pub operation_id: Option<String>,
    /// Tags grouping this operation.
    pub tags: Vec<String>,
    /// Path, query, header and cookie parameters, in contribution order.
    pub parameters: Vec<Parameter>,
    /// The request body, if any extractor consumes one.
    pub request_body: Option<RequestBody>,
    /// Responses keyed by status code, `NXX` range, or `default`.
    pub responses: IndexMap<String, Response>,
    /// Security requirements. `None` inherits the document-level ones;
    /// `Some(vec![])` explicitly permits unauthenticated access.
    pub security: Option<Vec<SecurityRequirement>>,
    /// A pointer to prose documentation.
    pub external_docs: Option<ExternalDocs>,
    /// `x-*` specification extensions.
    pub extensions: IndexMap<String, Value>,
    /// Whether clients should migrate away.
    pub deprecated: bool,
    /// Whether to omit this operation from the document entirely.
    ///
    /// Hidden operations still exist and are still routed. This is for internal
    /// endpoints, not for security.
    pub hidden: bool,
    /// Whether the request is validated against its declared constraints
    /// before the handler runs.
    ///
    /// Set by [`OperationBuilder::mark_validated`]. Not serialised; used by the
    /// assembler to check that an operation which validates also documents a
    /// `422`.
    pub validated: bool,
    /// Where in the user's source the operation was defined.
    pub source: Option<SourceLocation>,
}

impl OperationSpec {
    /// Add a tag unless it is already present.
    pub fn merge_tag(&mut self, tag: impl Into<String>) {
        let tag = tag.into();
        if !self.tags.contains(&tag) {
            self.tags.push(tag);
        }
    }

    /// Add a parameter, or fill in absent members of the one already at the
    /// same `(in, name)`.
    ///
    /// An **empty name never matches**, even against another empty name. A
    /// nameless parameter is a placeholder a positional `Path<T>` contributes —
    /// `Path<(u64, String)>` contributes two — and the route names them
    /// afterwards, because only it has seen the template. Treating the empty
    /// string as an identity would collapse those two into one and document a
    /// two-segment route as taking a single parameter.
    pub fn merge_parameter(&mut self, parameter: Parameter) {
        let existing = if parameter.name.is_empty() {
            None
        } else {
            self.parameters.iter_mut().find(|existing| {
                existing.location == parameter.location && existing.name == parameter.name
            })
        };
        match existing {
            Some(existing) => existing.merge_missing(parameter),
            None => self.parameters.push(parameter),
        }
    }

    /// Install the request body, or add content types the existing one lacks.
    pub fn merge_request_body(&mut self, body: RequestBody) {
        let Some(existing) = self.request_body.as_mut() else {
            self.request_body = Some(body);
            return;
        };
        // `required` is deliberately untouched: the first writer installed the
        // whole object, so its `false` means "this body is optional", not
        // "unset". Or-ing here would let a router-level overlay silently
        // promote an `Option<Json<T>>` body to required.
        if existing.reference.is_none() {
            existing.reference = body.reference;
        }
        if existing.description.is_none() {
            existing.description = body.description;
        }
        for (content_type, media) in body.content {
            existing.content.entry(content_type).or_insert(media);
        }
        for (key, value) in body.extensions {
            existing.extensions.entry(key).or_insert(value);
        }
    }

    /// Add a response at `key`, or fill in absent members of the one already there.
    pub fn merge_response(&mut self, key: impl Into<String>, response: Response) {
        match self.responses.entry(key.into()) {
            Entry::Occupied(mut occupied) => occupied.get_mut().merge_missing(response),
            Entry::Vacant(vacant) => {
                vacant.insert(response);
            }
        }
    }

    /// Add a security requirement unless an identical one is present.
    pub fn merge_security(&mut self, requirement: SecurityRequirement) {
        let requirements = self.security.get_or_insert_with(Vec::new);
        if !requirements.contains(&requirement) {
            requirements.push(requirement);
        }
    }

    /// Add an extension unless the key is already set.
    pub fn merge_extension(&mut self, key: impl Into<String>, value: Value) {
        self.extensions.entry(key.into()).or_insert(value);
    }

    /// Overlay router-level metadata: `other` supplies only what `self` lacks.
    ///
    /// This is the rule that keeps `Router::tag("users")` from clobbering an
    /// endpoint that named its own tag, and it is why the overlay is applied
    /// *after* the handler has described itself.
    pub fn overlay(&mut self, other: &OperationSpec) {
        if self.summary.is_none() {
            self.summary.clone_from(&other.summary);
        }
        if self.description.is_none() {
            self.description.clone_from(&other.description);
        }
        if self.operation_id.is_none() {
            self.operation_id.clone_from(&other.operation_id);
        }
        if self.external_docs.is_none() {
            self.external_docs.clone_from(&other.external_docs);
        }
        if self.source.is_none() {
            self.source = other.source;
        }

        for tag in &other.tags {
            self.merge_tag(tag.clone());
        }
        for parameter in &other.parameters {
            self.merge_parameter(parameter.clone());
        }
        if let Some(body) = &other.request_body {
            self.merge_request_body(body.clone());
        }
        for (key, response) in &other.responses {
            self.merge_response(key.clone(), response.clone());
        }
        match &other.security {
            // `Some(vec![])` is "explicitly unauthenticated". It can install
            // that stance where none was taken, but it never removes one.
            Some(requirements) if requirements.is_empty() => {
                self.security.get_or_insert_with(Vec::new);
            }
            Some(requirements) => {
                for requirement in requirements {
                    self.merge_security(requirement.clone());
                }
            }
            None => {}
        }
        for (key, value) in &other.extensions {
            self.merge_extension(key.clone(), value.clone());
        }

        self.deprecated |= other.deprecated;
        self.hidden |= other.hidden;
        self.validated |= other.validated;
    }

    /// Sort responses by status, with `default` last.
    ///
    /// Numeric statuses ascend first, then `NXX` ranges, then anything
    /// unrecognised, then `default`.
    pub fn sort_responses(&mut self) {
        self.responses.sort_by(|left, _, right, _| {
            response_key_rank(left)
                .cmp(&response_key_rank(right))
                .then_with(|| left.cmp(right))
        });
    }

    /// Whether a response is documented at `key`.
    pub fn has_response(&self, key: &str) -> bool {
        self.responses.contains_key(key)
    }

    /// Produce the wire object.
    ///
    /// Sorts responses first, so the output is stable regardless of the order
    /// in which extractors contributed.
    ///
    /// A response that is neither a `$ref` nor carries a description is given
    /// the status code's reason phrase: `description` is *required* by the
    /// specification, and emitting an invalid document because a contributor
    /// forgot a string helps nobody.
    pub fn into_operation(mut self) -> Operation {
        self.sort_responses();

        let mut responses = self.responses;
        for (key, response) in &mut responses {
            if response.reference.is_none() && response.description.is_none() {
                response.description = Some(default_response_description(key));
            }
        }

        Operation {
            tags: self.tags,
            summary: self.summary,
            description: self.description,
            external_docs: self.external_docs,
            operation_id: self.operation_id,
            parameters: self.parameters,
            request_body: self.request_body,
            responses,
            callbacks: IndexMap::new(),
            deprecated: self.deprecated,
            security: self.security,
            servers: Vec::new(),
            extensions: self.extensions,
        }
    }
}

/// Derive an `operationId` from a method and a templated path.
///
/// `GET /users/{id}/posts` becomes `get_users_by_id_posts`. Used only when
/// `#[endpoint]` could not supply one, which happens for handlers registered
/// through the plain-`async fn` [`Handler`] impls that carry no metadata.
///
/// [`Handler`]: https://docs.rs/moso
pub fn derive_operation_id(method: HttpMethod, path: &str) -> String {
    let mut id = method.as_str().to_owned();
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        let slug = match placeholder_name(segment) {
            Some(name) => {
                let name = slugify(name);
                if name.is_empty() {
                    String::new()
                } else {
                    format!("by_{name}")
                }
            }
            None => slugify(segment),
        };
        if slug.is_empty() {
            continue;
        }
        id.push('_');
        id.push_str(&slug);
    }
    id
}

/// The parameter name of a `{placeholder}` segment, if `segment` is one.
///
/// A leading `*` (Axum 0.8's `{*rest}` catch-all) is stripped: the OpenAPI
/// parameter is named `rest`, not `*rest`.
fn placeholder_name(segment: &str) -> Option<&str> {
    segment
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .map(|name| name.trim_start_matches('*'))
}

/// Lowercase ASCII alphanumerics, every other run collapsing to one `_`.
fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut separator_pending = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            if separator_pending && !out.is_empty() {
                out.push('_');
            }
            separator_pending = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            separator_pending = true;
        }
    }
    out
}

/// The `{placeholders}` of a templated path, in order, deduplicated.
pub(crate) fn path_placeholders(path: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        let name = after[..end].trim_start_matches('*');
        if !name.is_empty() && !names.iter().any(|known: &String| known == name) {
            names.push(name.to_owned());
        }
        rest = &after[end + 1..];
    }
    names
}

/// The leading digit of an `NXX` range key, if `key` is one.
pub(crate) fn range_key_digit(key: &str) -> Option<u8> {
    let bytes = key.as_bytes();
    if bytes.len() != 3 {
        return None;
    }
    if !(b'1'..=b'5').contains(&bytes[0]) {
        return None;
    }
    if !bytes[1].eq_ignore_ascii_case(&b'X') || !bytes[2].eq_ignore_ascii_case(&b'X') {
        return None;
    }
    Some(bytes[0] - b'0')
}

/// Whether `key` is a status code, an `NXX` range, or `default`.
pub(crate) fn is_valid_response_key(key: &str) -> bool {
    if key == "default" {
        return true;
    }
    if let Ok(status) = key.parse::<u16>() {
        return key.len() == 3 && (100..=599).contains(&status);
    }
    range_key_digit(key).is_some()
}

/// The sort key of a response: numeric statuses, then ranges, then anything
/// unrecognised, then `default`.
pub(crate) fn response_key_rank(key: &str) -> (u8, u32) {
    if key == "default" {
        return (3, 0);
    }
    if let Ok(status) = key.parse::<u32>() {
        return (0, status);
    }
    if let Some(digit) = range_key_digit(key) {
        return (1, u32::from(digit));
    }
    (2, 0)
}

/// The description given to a response whose contributor supplied none.
pub(crate) fn default_response_description(key: &str) -> String {
    if key == "default" {
        return "Unexpected error".to_owned();
    }
    if let Some(digit) = range_key_digit(key) {
        let phrase = match digit {
            1 => "Informational",
            2 => "Success",
            3 => "Redirection",
            4 => "Client error",
            _ => "Server error",
        };
        return phrase.to_owned();
    }
    key.parse::<u16>()
        .ok()
        .and_then(reason_phrase)
        .unwrap_or("Response")
        .to_owned()
}

/// The IANA reason phrase for a status code.
///
/// A local table rather than a dependency on `http`: this crate performs no
/// I/O and knows nothing about HTTP servers, and the only use is a fallback
/// description.
pub(crate) fn reason_phrase(status: u16) -> Option<&'static str> {
    let phrase = match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        418 => "I'm a teapot",
        422 => "Unprocessable Content",
        423 => "Locked",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        507 => "Insufficient Storage",
        _ => return None,
    };
    Some(phrase)
}

// ---------------------------------------------------------------------------
// OperationBuilder
// ---------------------------------------------------------------------------

/// The builder every extractor and response type writes into.
///
/// It owns the [`SchemaGenerator`] for the duration of one operation's
/// description so that `describe` can register named schemas without the
/// caller juggling two mutable borrows. [`DocumentBuilder::operation`] lends it
/// out and takes it back; use [`OperationBuilder::new`] directly only when
/// describing an operation in isolation, such as in a unit test.
///
/// Every mutator returns `&mut Self`, so both statement style
/// (`op.summary("…");`) and chaining read naturally.
///
/// ```
/// use moso_openapi::{ContentType, OperationBuilder, Param, ResponseSpec};
/// use moso_schema::json_schema::SchemaGenerator;
///
/// let mut op = OperationBuilder::new(SchemaGenerator::default());
///
/// // One contributor names the operation …
/// op.summary("List users").tag("users");
///
/// // … another adds a parameter …
/// op.parameter(Param::query("limit").schema_of::<u32>().required(false));
///
/// // … and a third the responses. Nobody writes an OpenAPI fragment by hand.
/// op.request_body_of::<String>(ContentType::Json, true);
/// op.response(200, ResponseSpec::json_of::<Vec<String>>());
///
/// let (spec, generator) = op.finish();
/// assert_eq!(spec.summary.as_deref(), Some("List users"));
/// assert_eq!(spec.parameters.len(), 1);
/// assert!(spec.responses.contains_key("200"));
/// # let _ = generator;
/// ```
///
/// **First writer wins** for scalars, so an extractor cannot overwrite the summary
/// `#[endpoint]` took from the doc comment. Tags and security requirements append
/// and deduplicate; parameters and responses are keyed and fill only absent members.
#[derive(Debug)]
pub struct OperationBuilder {
    generator: SchemaGenerator,
    spec: OperationSpec,
}

impl OperationBuilder {
    /// A builder for a fresh operation, borrowing ownership of `generator`.
    pub fn new(generator: SchemaGenerator) -> Self {
        Self {
            generator,
            spec: OperationSpec::default(),
        }
    }

    /// A builder continuing an operation that is already partly described.
    pub fn from_parts(generator: SchemaGenerator, spec: OperationSpec) -> Self {
        Self { generator, spec }
    }

    /// One-line summary. First writer wins.
    pub fn summary(&mut self, summary: impl Into<String>) -> &mut Self {
        if self.spec.summary.is_none() {
            self.spec.summary = Some(summary.into());
        }
        self
    }

    /// Long description. First writer wins.
    pub fn description(&mut self, description: impl Into<String>) -> &mut Self {
        if self.spec.description.is_none() {
            self.spec.description = Some(description.into());
        }
        self
    }

    /// Unique operation identifier. First writer wins.
    pub fn operation_id(&mut self, operation_id: impl Into<String>) -> &mut Self {
        if self.spec.operation_id.is_none() {
            self.spec.operation_id = Some(operation_id.into());
        }
        self
    }

    /// Add a tag. Deduplicated, order preserved.
    pub fn tag(&mut self, tag: impl Into<String>) -> &mut Self {
        self.spec.merge_tag(tag);
        self
    }

    /// Mark the operation deprecated. Sticky.
    pub fn deprecated(&mut self) -> &mut Self {
        self.spec.deprecated = true;
        self
    }

    /// Mark the operation deprecated and record a sunset date as `x-sunset`.
    pub fn sunset(&mut self, date: impl Into<String>) -> &mut Self {
        self.spec.deprecated = true;
        self.spec
            .merge_extension("x-sunset", Value::String(date.into()));
        self
    }

    /// Omit the operation from the document. Sticky.
    ///
    /// The route is still mounted and still serves traffic. This is for
    /// internal endpoints; it is not an access control.
    pub fn hidden(&mut self) -> &mut Self {
        self.spec.hidden = true;
        self
    }

    /// Record where in the user's source this operation is defined.
    pub fn source(&mut self, file: &'static str, line: u32) -> &mut Self {
        if self.spec.source.is_none() {
            self.spec.source = Some(SourceLocation { file, line });
        }
        self
    }

    /// Contribute a parameter.
    pub fn parameter(&mut self, param: Param) -> &mut Self {
        let parameter = param.build(&mut self.generator);
        self.spec.merge_parameter(parameter);
        self
    }

    /// Contribute several parameters.
    pub fn parameters(&mut self, params: impl IntoIterator<Item = Param>) -> &mut Self {
        for param in params {
            self.parameter(param);
        }
        self
    }

    /// Contribute the request body.
    pub fn request_body(
        &mut self,
        content_type: ContentType,
        schema: SchemaRef,
        required: bool,
    ) -> &mut Self {
        let mut body = RequestBody {
            required,
            ..RequestBody::default()
        };
        body.content.insert(
            content_type.as_str().to_owned(),
            MediaType::new(schema.into()),
        );
        self.spec.merge_request_body(body);
        self
    }

    /// Contribute the request body from `T`'s schema, registering it.
    pub fn request_body_of<T: Schema>(
        &mut self,
        content_type: ContentType,
        required: bool,
    ) -> &mut Self {
        let schema = self.generator.define::<T>();
        self.request_body(content_type, schema, required)
    }

    /// Contribute a fully-built request body object.
    pub fn request_body_spec(&mut self, body: RequestBody) -> &mut Self {
        self.spec.merge_request_body(body);
        self
    }

    /// Contribute a response at `status`.
    ///
    /// Registering the same status twice is idempotent: the first
    /// registration's description survives and the second adds only what the
    /// first did not describe.
    pub fn response(&mut self, status: u16, spec: ResponseSpec) -> &mut Self {
        let response = spec.build(&mut self.generator);
        self.spec.merge_response(status.to_string(), response);
        self
    }

    /// Contribute a response under an arbitrary key: a status code, an `NXX`
    /// range such as `"5XX"`, or `"default"`.
    pub fn response_key(&mut self, key: impl Into<String>, spec: ResponseSpec) -> &mut Self {
        let response = spec.build(&mut self.generator);
        self.spec.merge_response(key, response);
        self
    }

    /// Contribute the `default` response, covering every status not listed.
    pub fn default_response(&mut self, spec: ResponseSpec) -> &mut Self {
        self.response_key("default", spec)
    }

    /// Contribute a security requirement.
    ///
    /// Called by authenticating extractors, never written by hand: an
    /// operation's documented authentication is the authentication it performs.
    pub fn security(&mut self, requirement: SecurityRequirement) -> &mut Self {
        self.spec.merge_security(requirement);
        self
    }

    /// Declare the operation as explicitly unauthenticated, overriding any
    /// document-level requirement.
    pub fn public(&mut self) -> &mut Self {
        self.spec.security = Some(Vec::new());
        self
    }

    /// Attach an `x-*` specification extension. First writer wins per key.
    pub fn extension(&mut self, key: &'static str, value: Value) -> &mut Self {
        self.spec.merge_extension(key, value);
        self
    }

    /// Point at prose documentation for this operation.
    pub fn external_docs(
        &mut self,
        url: impl Into<String>,
        description: impl Into<String>,
    ) -> &mut Self {
        if self.spec.external_docs.is_none() {
            self.spec.external_docs = Some(ExternalDocs::new(url, description));
        }
        self
    }

    /// Record that this operation's input is validated against its declared
    /// constraints before the handler runs. Sticky.
    ///
    /// The assembler uses this to check that a validating operation also
    /// documents a `422`, so a schema with constraints can never be documented
    /// as if it accepted anything.
    pub fn mark_validated(&mut self) -> &mut Self {
        self.spec.validated = true;
        self
    }

    /// Whether [`OperationBuilder::mark_validated`] has been called.
    pub fn is_validated(&self) -> bool {
        self.spec.validated
    }

    /// The schema generator, so `describe` can register named schemas.
    ///
    /// Schemas registered here land in `components.schemas` and are referenced
    /// by `$ref`, which is what makes one struct used by twenty endpoints
    /// appear once in the document.
    pub fn generator(&mut self) -> &mut SchemaGenerator {
        &mut self.generator
    }

    /// The description accumulated so far.
    pub fn spec(&self) -> &OperationSpec {
        &self.spec
    }

    /// Mutable access to the description accumulated so far, for the rare
    /// contributor that needs to express something the mutators do not cover.
    pub fn spec_mut(&mut self) -> &mut OperationSpec {
        &mut self.spec
    }

    /// Finish, returning the description and the generator that was lent in.
    pub fn finish(self) -> (OperationSpec, SchemaGenerator) {
        (self.spec, self.generator)
    }

    /// Finish, discarding the generator.
    ///
    /// Only correct when the generator was created for this operation alone —
    /// otherwise the named schemas registered during `describe` are lost.
    pub fn into_spec(self) -> OperationSpec {
        self.spec
    }
}

// ---------------------------------------------------------------------------
// RouteMetadata
// ---------------------------------------------------------------------------

/// OpenAPI metadata attached to a router rather than to a handler.
///
/// `Router::tag`, `Router::security`, `Router::responds` and
/// `Router::deprecated` accumulate into one of these, and `nest`/`merge`
/// compose them downward with [`RouteMetadata::extend_from`]. It is applied to
/// each operation *after* the handler has described itself, so it can only
/// add — never overwrite.
#[derive(Debug, Clone, Default)]
pub struct RouteMetadata {
    /// Tags applied to every operation in the subtree.
    pub tags: Vec<String>,
    /// Security requirements applied to every operation in the subtree.
    pub security: Vec<SecurityRequirement>,
    /// Extra responses documented on every operation in the subtree.
    pub responses: Vec<(u16, ResponseSpec)>,
    /// Whether every operation in the subtree is deprecated.
    pub deprecated: bool,
    /// Whether every operation in the subtree is omitted from the document.
    pub hidden: bool,
    /// `x-*` extensions applied to every operation in the subtree.
    pub extensions: IndexMap<String, Value>,
}

impl RouteMetadata {
    /// Empty metadata.
    pub fn new() -> Self {
        Self::default()
    }

    /// Tag every operation in the subtree.
    pub fn tag(&mut self, tag: impl Into<String>) -> &mut Self {
        let tag = tag.into();
        if !self.tags.contains(&tag) {
            self.tags.push(tag);
        }
        self
    }

    /// Require authentication for every operation in the subtree.
    pub fn security(&mut self, requirement: SecurityRequirement) -> &mut Self {
        if !self.security.contains(&requirement) {
            self.security.push(requirement);
        }
        self
    }

    /// Document an extra response on every operation in the subtree.
    pub fn responds(&mut self, status: u16, spec: ResponseSpec) -> &mut Self {
        self.responses.push((status, spec));
        self
    }

    /// Deprecate every operation in the subtree.
    pub fn deprecate(&mut self) -> &mut Self {
        self.deprecated = true;
        self
    }

    /// Hide every operation in the subtree from the document.
    pub fn hide(&mut self) -> &mut Self {
        self.hidden = true;
        self
    }

    /// Attach an `x-*` extension to every operation in the subtree.
    pub fn extension(&mut self, key: &'static str, value: impl Into<Value>) -> &mut Self {
        self.extensions.entry(key.into()).or_insert(value.into());
        self
    }

    /// Absorb an outer router's metadata, as `nest` and `merge` do.
    ///
    /// Outer tags and responses are appended after this router's own, so the
    /// inner, more specific declaration is listed first.
    pub fn extend_from(&mut self, outer: &RouteMetadata) {
        for tag in &outer.tags {
            if !self.tags.contains(tag) {
                self.tags.push(tag.clone());
            }
        }
        for requirement in &outer.security {
            if !self.security.contains(requirement) {
                self.security.push(requirement.clone());
            }
        }
        self.responses.extend(outer.responses.iter().cloned());
        for (key, value) in &outer.extensions {
            self.extensions.entry(key.clone()).or_insert(value.clone());
        }
        self.deprecated |= outer.deprecated;
        self.hidden |= outer.hidden;
    }

    /// `true` when nothing is set, so applying it can be skipped entirely.
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
            && self.security.is_empty()
            && self.responses.is_empty()
            && self.extensions.is_empty()
            && !self.deprecated
            && !self.hidden
    }

    /// Apply this metadata to an operation that has already described itself.
    pub fn apply(&self, op: &mut OperationBuilder) {
        for tag in &self.tags {
            op.tag(tag.clone());
        }
        for requirement in &self.security {
            op.security(requirement.clone());
        }
        for (status, spec) in &self.responses {
            op.response(*status, spec.clone());
        }
        for (key, value) in &self.extensions {
            op.spec_mut().merge_extension(key.clone(), value.clone());
        }
        if self.deprecated {
            op.deprecated();
        }
        if self.hidden {
            op.hidden();
        }
    }
}

// ---------------------------------------------------------------------------
// DocumentBuilder
// ---------------------------------------------------------------------------

/// Assembles a [`Document`] from application metadata and per-route operations.
///
/// This is what `App::new(cfg).openapi(|d| …)` hands the user, and what
/// `App::build()` drives while walking the composed router.
///
/// ```
/// use moso_openapi::{DocumentBuilder, SecurityScheme};
///
/// let mut d = DocumentBuilder::new();
/// d.title("Shop API")
///     .version("0.1.0")
///     .server("https://api.shop.example", "production")
///     .security_scheme("session", SecurityScheme::cookie("sid"));
///
/// let document = d.build().expect("a well-formed document");
/// assert_eq!(document.info.title, "Shop API");
/// ```
///
/// In an application this is reached through `App::new(cfg).openapi(|d| …)`
/// rather than constructed directly.
#[derive(Debug)]
pub struct DocumentBuilder {
    info: Info,
    json_schema_dialect: Option<String>,
    servers: Vec<Server>,
    tags: IndexMap<String, Tag>,
    security: Vec<SecurityRequirement>,
    security_schemes: IndexMap<String, SecurityScheme>,
    shared_responses: IndexMap<String, Response>,
    external_docs: Option<ExternalDocs>,
    extensions: IndexMap<String, Value>,
    generator: SchemaGenerator,
    paths: IndexMap<String, PathItem>,
    webhooks: IndexMap<String, PathItem>,
    /// `operationId` to `METHOD /path`, for duplicate detection.
    operation_ids: IndexMap<String, String>,
    /// `METHOD /path` to the source location that registered it.
    ///
    /// [`SourceLocation`] never reaches the document, but a route conflict has
    /// to be able to name *both* registrations, so it is kept here.
    sources: IndexMap<String, String>,
    errors: Vec<DocumentError>,
}

impl Default for DocumentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentBuilder {
    /// A builder with no metadata beyond the defaults.
    ///
    /// The schema generator is created with the `#/components/schemas/` ref
    /// prefix, which is what makes generated `$ref`s resolve inside the
    /// assembled document.
    pub fn new() -> Self {
        Self {
            info: Info::default(),
            json_schema_dialect: Some(crate::JSON_SCHEMA_DIALECT.to_owned()),
            servers: Vec::new(),
            tags: IndexMap::new(),
            security: Vec::new(),
            security_schemes: IndexMap::new(),
            shared_responses: IndexMap::new(),
            external_docs: None,
            extensions: IndexMap::new(),
            generator: SchemaGenerator::new(crate::COMPONENTS_SCHEMAS_PREFIX),
            paths: IndexMap::new(),
            webhooks: IndexMap::new(),
            operation_ids: IndexMap::new(),
            sources: IndexMap::new(),
            errors: Vec::new(),
        }
    }

    /// The API title.
    pub fn title(&mut self, title: impl Into<String>) -> &mut Self {
        self.info.title = title.into();
        self
    }

    /// The API version. Distinct from the OpenAPI version.
    pub fn version(&mut self, version: impl Into<String>) -> &mut Self {
        self.info.version = version.into();
        self
    }

    /// A one-line summary of the API.
    pub fn summary(&mut self, summary: impl Into<String>) -> &mut Self {
        self.info.summary = Some(summary.into());
        self
    }

    /// A long description. CommonMark; typically `include_str!`.
    pub fn description(&mut self, description: impl Into<String>) -> &mut Self {
        self.info.description = Some(description.into());
        self
    }

    /// A URL to the terms of service.
    pub fn terms_of_service(&mut self, url: impl Into<String>) -> &mut Self {
        self.info.terms_of_service = Some(url.into());
        self
    }

    /// Who to contact about this API.
    pub fn contact(&mut self, name: impl Into<String>, email: impl Into<String>) -> &mut Self {
        let contact = self.info.contact.get_or_insert_with(Contact::default);
        contact.name = Some(name.into());
        contact.email = Some(email.into());
        self
    }

    /// A URL for the contact, in addition to the name and email.
    pub fn contact_url(&mut self, url: impl Into<String>) -> &mut Self {
        self.info.contact.get_or_insert_with(Contact::default).url = Some(url.into());
        self
    }

    /// The licence, as an SPDX expression such as `Apache-2.0 OR MIT`.
    pub fn license_spdx(&mut self, identifier: impl Into<String>) -> &mut Self {
        self.info.license = Some(License::spdx(identifier));
        self
    }

    /// The licence, as a name and a URL to its text.
    pub fn license(&mut self, name: impl Into<String>, url: impl Into<String>) -> &mut Self {
        self.info.license = Some(License::named(name, url));
        self
    }

    /// Add a base URL the API is served from.
    ///
    /// A trailing `/` is stripped: operation paths already begin with `/`, so a
    /// server URL ending in one produces a doubled slash and fails the OpenAPI
    /// `no-server-trailing-slash` lint. The `url` crate normalises a host-only
    /// URL (`http://host`) to carry a `/` path, so this is the common case. The
    /// bare-root `"/"` is left untouched.
    pub fn server(&mut self, url: impl Into<String>, description: impl Into<String>) -> &mut Self {
        let url = url.into();
        let trimmed = url.trim_end_matches('/');
        let url = if trimmed.is_empty() {
            url
        } else {
            trimmed.to_owned()
        };
        self.servers.push(Server::new(url, description));
        self
    }

    /// Add a fully-specified server, with variables.
    pub fn server_spec(&mut self, server: Server) -> &mut Self {
        self.servers.push(server);
        self
    }

    /// Give a tag a human-readable description.
    ///
    /// Tags used by an operation but never described here still appear; this
    /// only adds prose.
    pub fn tag_description(
        &mut self,
        tag: impl Into<String>,
        description: impl Into<String>,
    ) -> &mut Self {
        let name = tag.into();
        let entry = self
            .tags
            .entry(name.clone())
            .or_insert_with(|| Tag::new(name));
        entry.description = Some(description.into());
        self
    }

    /// Add a fully-specified tag.
    pub fn tag(&mut self, tag: Tag) -> &mut Self {
        self.tags.insert(tag.name.clone(), tag);
        self
    }

    /// Declare a security scheme under a name that
    /// [`SecurityRequirement`]s refer to.
    pub fn security_scheme(
        &mut self,
        name: impl Into<String>,
        scheme: SecurityScheme,
    ) -> &mut Self {
        self.security_schemes.insert(name.into(), scheme);
        self
    }

    /// Require this of every operation that does not override it.
    pub fn security(&mut self, requirement: SecurityRequirement) -> &mut Self {
        if !self.security.contains(&requirement) {
            self.security.push(requirement);
        }
        self
    }

    /// Declare a reusable response in `components.responses`, referenced with
    /// [`ResponseSpec::shared`].
    pub fn shared_response(&mut self, name: impl Into<String>, response: Response) -> &mut Self {
        self.shared_responses.insert(name.into(), response);
        self
    }

    /// Point at prose documentation for the whole API.
    pub fn external_docs(
        &mut self,
        url: impl Into<String>,
        description: impl Into<String>,
    ) -> &mut Self {
        self.external_docs = Some(ExternalDocs::new(url, description));
        self
    }

    /// Override the JSON Schema dialect. Almost never correct to call.
    pub fn json_schema_dialect(&mut self, dialect: impl Into<String>) -> &mut Self {
        self.json_schema_dialect = Some(dialect.into());
        self
    }

    /// Attach an `x-*` extension at document level.
    pub fn extension(&mut self, key: &'static str, value: impl Into<Value>) -> &mut Self {
        self.extensions.insert(key.into(), value.into());
        self
    }

    /// The schema generator, for registering schemas outside an operation.
    pub fn generator(&mut self) -> &mut SchemaGenerator {
        &mut self.generator
    }

    /// Describe and register one operation.
    ///
    /// The closure receives an [`OperationBuilder`] that has been lent this
    /// builder's schema generator, so schemas registered while describing land
    /// in `components.schemas`. The generator is always returned, even if the
    /// closure panics is *not* guaranteed — but a panicking `describe` is a bug
    /// that fails the boot anyway.
    pub fn operation(
        &mut self,
        method: HttpMethod,
        path: impl Into<String>,
        describe: impl FnOnce(&mut OperationBuilder),
    ) -> &mut Self {
        let generator = core::mem::take(&mut self.generator);
        let mut builder = OperationBuilder::new(generator);
        describe(&mut builder);
        let (spec, generator) = builder.finish();
        self.generator = generator;
        self.insert_operation(method, path, spec)
    }

    /// Register an already-described operation.
    ///
    /// Hidden operations are dropped here. Conflicts and duplicate
    /// `operationId`s are recorded and reported together by
    /// [`DocumentBuilder::build`], because a boot error that reports one
    /// problem at a time makes people fix them one at a time.
    pub fn insert_operation(
        &mut self,
        method: HttpMethod,
        path: impl Into<String>,
        spec: OperationSpec,
    ) -> &mut Self {
        let path = path.into();
        if spec.hidden {
            return self;
        }

        let location = format!("{} {}", method.as_upper_str(), path);
        self.check_response_keys(&spec);
        self.claim_operation_id(&spec, &location);

        let source = spec.source.map(|source| source.to_string());
        if self
            .paths
            .get(&path)
            .and_then(|item| item.operation(method))
            .is_some()
        {
            let first = self.sources.get(&location).cloned();
            self.errors.push(DocumentError::RouteConflict {
                method,
                path,
                first,
                second: source,
            });
            return self;
        }

        if let Some(source) = source {
            self.sources.insert(location, source);
        }
        self.paths
            .entry(path)
            .or_default()
            .set_operation(method, spec.into_operation());
        self
    }

    /// Register a webhook the API expects to receive.
    pub fn webhook(
        &mut self,
        name: impl Into<String>,
        method: HttpMethod,
        spec: OperationSpec,
    ) -> &mut Self {
        let name = name.into();
        if spec.hidden {
            return self;
        }

        let location = format!("{} webhook `{}`", method.as_upper_str(), name);
        self.check_response_keys(&spec);
        self.claim_operation_id(&spec, &location);

        let source = spec.source.map(|source| source.to_string());
        if self
            .webhooks
            .get(&name)
            .and_then(|item| item.operation(method))
            .is_some()
        {
            let first = self.sources.get(&location).cloned();
            self.errors.push(DocumentError::RouteConflict {
                method,
                path: format!("webhook `{name}`"),
                first,
                second: source,
            });
            return self;
        }

        if let Some(source) = source {
            self.sources.insert(location, source);
        }
        self.webhooks
            .entry(name)
            .or_default()
            .set_operation(method, spec.into_operation());
        self
    }

    /// Record `spec`'s `operationId`, or report the operation that had it first.
    fn claim_operation_id(&mut self, spec: &OperationSpec, location: &str) {
        let Some(operation_id) = spec.operation_id.clone() else {
            return;
        };
        match self.operation_ids.entry(operation_id.clone()) {
            Entry::Occupied(occupied) => {
                let first = occupied.get().clone();
                self.errors.push(DocumentError::DuplicateOperationId {
                    operation_id,
                    first,
                    second: location.to_owned(),
                });
            }
            Entry::Vacant(vacant) => {
                vacant.insert(location.to_owned());
            }
        }
    }

    /// Report any response key that is not a status, an `NXX` range or `default`.
    fn check_response_keys(&mut self, spec: &OperationSpec) {
        for key in spec.responses.keys() {
            if !is_valid_response_key(key) {
                self.errors
                    .push(DocumentError::InvalidStatusKey { key: key.clone() });
            }
        }
    }

    /// Problems recorded so far.
    pub fn errors(&self) -> &[DocumentError] {
        &self.errors
    }

    /// The path items registered so far.
    ///
    /// `moso-core` reads this when reporting a route conflict, so the error can
    /// name the operation already installed at the contested path.
    pub fn paths(&self) -> &IndexMap<String, PathItem> {
        &self.paths
    }

    /// The webhooks registered so far.
    pub fn webhooks(&self) -> &IndexMap<String, PathItem> {
        &self.webhooks
    }

    /// The `operationId`s claimed so far, mapped to the `METHOD /path` that
    /// claimed each one.
    pub fn operation_ids(&self) -> &IndexMap<String, String> {
        &self.operation_ids
    }

    /// Assemble the document.
    ///
    /// Applies the canonical ordering, folds the schema generator's definitions
    /// into `components.schemas`, turns schema-name collisions into errors, and
    /// runs the same consistency checks as
    /// [`Document::validate_self`](crate::document::Document::validate_self) —
    /// structurally, so each problem comes back as a [`DocumentError`] naming
    /// the offending code rather than as a string.
    ///
    /// Every problem found is returned, not just the first.
    pub fn build(self) -> Result<Document, Vec<DocumentError>> {
        let (document, mut errors) = self.assemble();
        check_path_parameters(&document, &mut errors);
        check_security(&document, &mut errors);
        check_references(&document, &mut errors);
        if errors.is_empty() {
            Ok(document)
        } else {
            Err(errors)
        }
    }

    /// Assemble a document without the consistency checks.
    ///
    /// For tests and for `moso openapi export --force`, where seeing the broken
    /// output is more useful than being told it is broken.
    pub fn build_unchecked(self) -> Document {
        self.assemble().0
    }

    /// The components assembled so far, for inspection in tests.
    pub fn components_snapshot(&self) -> Components {
        let mut components = Components {
            schemas: self.generator.definitions().clone(),
            responses: self.shared_responses.clone(),
            security_schemes: self.security_schemes.clone(),
            ..Components::default()
        };
        components.sort();
        components
    }

    /// Move every member into a [`Document`], applying the canonical ordering.
    ///
    /// Shared by [`DocumentBuilder::build`] and
    /// [`DocumentBuilder::build_unchecked`]; the only difference between them is
    /// whether the returned errors are consulted.
    fn assemble(mut self) -> (Document, Vec<DocumentError>) {
        let mut errors = core::mem::take(&mut self.errors);
        for collision in self.generator.collisions() {
            errors.push(DocumentError::SchemaCollision {
                name: collision.name.clone(),
                first: collision.first.to_owned(),
                second: collision.second.to_owned(),
            });
        }

        let schemas = self.generator.take_definitions();

        let mut shared_responses = self.shared_responses;
        for (name, response) in &mut shared_responses {
            // `description` is required of every response that is not a `$ref`;
            // the component's own name is a better fallback than an invalid
            // document.
            if response.reference.is_none() && response.description.is_none() {
                response.description = Some(name.clone());
            }
        }

        let mut paths = self.paths;
        paths.retain(|_, item| !item.is_empty() || item.reference.is_some());

        let mut info = self.info;
        if info.title.is_empty() {
            info.title = DEFAULT_TITLE.to_owned();
        }
        if info.version.is_empty() {
            info.version = DEFAULT_VERSION.to_owned();
        }

        let tags = declared_and_used_tags(&self.tags, &paths, &self.webhooks);

        let mut document = Document {
            openapi: crate::OPENAPI_VERSION.to_owned(),
            info,
            json_schema_dialect: self.json_schema_dialect,
            servers: self.servers,
            paths,
            webhooks: self.webhooks,
            components: Components {
                schemas,
                responses: shared_responses,
                security_schemes: self.security_schemes,
                ..Components::default()
            },
            security: self.security,
            tags,
            external_docs: self.external_docs,
            extensions: self.extensions,
        };
        document.sort_for_output();
        (document, errors)
    }
}

/// Every declared tag, followed by every tag an operation used but nobody
/// declared.
///
/// A documentation UI groups by the `tags` list, so a tag that only appears on
/// an operation would otherwise be rendered under "default".
fn declared_and_used_tags(
    declared: &IndexMap<String, Tag>,
    paths: &IndexMap<String, PathItem>,
    webhooks: &IndexMap<String, PathItem>,
) -> Vec<Tag> {
    let mut tags: Vec<Tag> = declared.values().cloned().collect();
    for item in paths.values().chain(webhooks.values()) {
        for (_, operation) in item.operations() {
            for name in &operation.tags {
                if !tags.iter().any(|tag| &tag.name == name) {
                    tags.push(Tag::new(name.clone()));
                }
            }
        }
    }
    tags
}

// ---------------------------------------------------------------------------
// Consistency checks
// ---------------------------------------------------------------------------

/// Report paths whose `{placeholders}` and `in: path` parameters disagree.
fn check_path_parameters(document: &Document, errors: &mut Vec<DocumentError>) {
    for (path, item) in &document.paths {
        let placeholders = path_placeholders(path);
        for (_, operation) in item.operations() {
            let mut declared: Vec<&str> = Vec::new();
            for parameter in item.parameters.iter().chain(operation.parameters.iter()) {
                if parameter.location == ParameterLocation::Path
                    && !declared.contains(&parameter.name.as_str())
                {
                    declared.push(&parameter.name);
                }
            }

            let missing: Vec<String> = placeholders
                .iter()
                .filter(|name| !declared.contains(&name.as_str()))
                .cloned()
                .collect();
            let extra: Vec<String> = declared
                .iter()
                .filter(|name| !placeholders.iter().any(|known| known == *name))
                .map(|name| (*name).to_owned())
                .collect();

            if missing.is_empty() && extra.is_empty() {
                continue;
            }
            push_unique(
                errors,
                DocumentError::PathParameterMismatch {
                    path: path.clone(),
                    missing,
                    extra,
                },
            );
        }
    }
}

/// Report security requirements naming a scheme the document does not declare.
fn check_security(document: &Document, errors: &mut Vec<DocumentError>) {
    for requirement in &document.security {
        check_requirement(requirement, "the document", &document.components, errors);
    }
    for (path, item) in &document.paths {
        check_path_item_security(item, path, &document.components, errors);
    }
    for (name, item) in &document.webhooks {
        let location = format!("webhook `{name}`");
        check_path_item_security(item, &location, &document.components, errors);
    }
}

/// [`check_security`] for one path item's operations.
fn check_path_item_security(
    item: &PathItem,
    path: &str,
    components: &Components,
    errors: &mut Vec<DocumentError>,
) {
    for (method, operation) in item.operations() {
        let Some(requirements) = &operation.security else {
            continue;
        };
        let location = format!("{} {}", method.as_upper_str(), path);
        for requirement in requirements {
            check_requirement(requirement, &location, components, errors);
        }
    }
}

/// [`check_security`] for one requirement.
fn check_requirement(
    requirement: &SecurityRequirement,
    location: &str,
    components: &Components,
    errors: &mut Vec<DocumentError>,
) {
    for (scheme, _) in requirement.schemes() {
        if !components.security_schemes.contains_key(scheme) {
            push_unique(
                errors,
                DocumentError::UnknownSecurityScheme {
                    scheme: scheme.to_owned(),
                    location: location.to_owned(),
                },
            );
        }
    }
}

/// Report every `#/components/...` `$ref` that resolves to nothing.
fn check_references(document: &Document, errors: &mut Vec<DocumentError>) {
    let mut references: Vec<(String, String)> = Vec::new();
    for (name, schema) in &document.components.schemas {
        collect_schema_refs(
            schema,
            &format!("components.schemas.{name}"),
            &mut references,
        );
    }
    for (path, item) in &document.paths {
        collect_path_item_refs(item, path, &mut references);
    }
    for (name, item) in &document.webhooks {
        collect_path_item_refs(item, &format!("webhook `{name}`"), &mut references);
    }

    for (reference, location) in references {
        if !reference.starts_with("#/components/") {
            // External and `#/$defs/...` references are outside this document's
            // authority; reporting them would be guessing.
            continue;
        }
        if !component_exists(&document.components, &reference) {
            push_unique(
                errors,
                DocumentError::DanglingRef {
                    reference,
                    location,
                },
            );
        }
    }
}

/// Whether an internal `#/components/<bucket>/<name>` pointer resolves.
pub(crate) fn component_exists(components: &Components, reference: &str) -> bool {
    let Some(rest) = reference.strip_prefix("#/components/") else {
        return true;
    };
    let Some((bucket, name)) = rest.split_once('/') else {
        return false;
    };
    let name = unescape_pointer_token(name);
    match bucket {
        "schemas" => components.schemas.contains_key(&name),
        "responses" => components.responses.contains_key(&name),
        "parameters" => components.parameters.contains_key(&name),
        "examples" => components.examples.contains_key(&name),
        "requestBodies" => components.request_bodies.contains_key(&name),
        "headers" => components.headers.contains_key(&name),
        "securitySchemes" => components.security_schemes.contains_key(&name),
        "links" => components.links.contains_key(&name),
        "pathItems" => components.path_items.contains_key(&name),
        _ => false,
    }
}

/// RFC 6901 token decoding: `~1` is `/` and `~0` is `~`, in that order.
pub(crate) fn unescape_pointer_token(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

/// Collect every `$ref` reachable from one path item.
fn collect_path_item_refs(item: &PathItem, path: &str, out: &mut Vec<(String, String)>) {
    if let Some(reference) = &item.reference {
        out.push((reference.clone(), path.to_owned()));
    }
    for parameter in &item.parameters {
        collect_parameter_refs(parameter, path, out);
    }
    for (method, operation) in item.operations() {
        let location = format!("{} {}", method.as_upper_str(), path);
        for parameter in &operation.parameters {
            collect_parameter_refs(parameter, &location, out);
        }
        if let Some(body) = &operation.request_body {
            let location = format!("{location} requestBody");
            if let Some(reference) = &body.reference {
                out.push((reference.clone(), location.clone()));
            }
            collect_content_refs(&body.content, &location, out);
        }
        for (key, response) in &operation.responses {
            let location = format!("{location} response {key}");
            if let Some(reference) = &response.reference {
                out.push((reference.clone(), location.clone()));
            }
            for (name, header) in &response.headers {
                let location = format!("{location} header `{name}`");
                if let Some(reference) = &header.reference {
                    out.push((reference.clone(), location.clone()));
                }
                if let Some(schema) = &header.schema {
                    collect_schema_refs(schema, &location, out);
                }
                collect_content_refs(&header.content, &location, out);
            }
            collect_content_refs(&response.content, &location, out);
            for (name, link) in &response.links {
                if let Some(reference) = &link.reference {
                    out.push((reference.clone(), format!("{location} link `{name}`")));
                }
            }
        }
    }
}

/// Collect every `$ref` reachable from one parameter.
fn collect_parameter_refs(parameter: &Parameter, location: &str, out: &mut Vec<(String, String)>) {
    let location = format!("{location} parameter `{}`", parameter.name);
    if let Some(schema) = &parameter.schema {
        collect_schema_refs(schema, &location, out);
    }
    collect_content_refs(&parameter.content, &location, out);
}

/// Collect every `$ref` reachable from a media-type map.
fn collect_content_refs(
    content: &IndexMap<String, MediaType>,
    location: &str,
    out: &mut Vec<(String, String)>,
) {
    for (content_type, media) in content {
        if let Some(schema) = &media.schema {
            collect_schema_refs(schema, &format!("{location} ({content_type})"), out);
        }
    }
}

/// Collect every `$ref` reachable from a schema node, recursively.
pub(crate) fn collect_schema_refs(
    node: &SchemaNode,
    location: &str,
    out: &mut Vec<(String, String)>,
) {
    if let Some(reference) = &node.reference {
        out.push((reference.clone(), location.to_owned()));
    }
    if let Some(items) = &node.items {
        collect_schema_refs(items, location, out);
    }
    for item in &node.prefix_items {
        collect_schema_refs(item, location, out);
    }
    for (name, property) in &node.properties {
        collect_schema_refs(property, &format!("{location}.{name}"), out);
    }
    if let Some(AdditionalProperties::Schema(schema)) = &node.additional_properties {
        collect_schema_refs(schema, location, out);
    }
    for branch in node.one_of.iter().chain(&node.any_of).chain(&node.all_of) {
        collect_schema_refs(branch, location, out);
    }
    if let Some(not) = &node.not {
        collect_schema_refs(not, location, out);
    }
    for (name, def) in &node.defs {
        collect_schema_refs(def, &format!("{location}.$defs.{name}"), out);
    }
}

/// Push `error` unless an identical one is already recorded.
///
/// One mistake in one place is one boot error, however many operations it
/// touches.
fn push_unique(errors: &mut Vec<DocumentError>, error: DocumentError) {
    if !errors.contains(&error) {
        errors.push(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_strings_are_the_wire_values() {
        assert_eq!(ContentType::Json.as_str(), "application/json");
        assert_eq!(
            ContentType::ProblemJson.as_str(),
            "application/problem+json"
        );
        assert_eq!(ContentType::EventStream.as_str(), "text/event-stream");
        assert_eq!(
            ContentType::custom("application/cbor").as_str(),
            "application/cbor"
        );
    }

    #[test]
    fn path_parameters_are_required_regardless_of_the_caller() {
        let param = Param::path("id").required(false);
        assert!(param.as_parameter().required);
    }

    #[test]
    fn query_parameters_default_to_optional() {
        let param = Param::query("limit");
        assert!(!param.as_parameter().required);
    }

    #[test]
    fn first_writer_wins_for_scalars() {
        let mut op = OperationBuilder::new(SchemaGenerator::default());
        op.summary("from the doc comment");
        op.summary("from somewhere else");
        assert_eq!(op.spec().summary.as_deref(), Some("from the doc comment"));
    }

    #[test]
    fn tags_are_deduplicated() {
        let mut op = OperationBuilder::new(SchemaGenerator::default());
        op.tag("users").tag("users").tag("admin");
        assert_eq!(op.spec().tags, ["users", "admin"]);
    }

    #[test]
    fn hidden_and_validated_are_sticky() {
        let mut op = OperationBuilder::new(SchemaGenerator::default());
        op.hidden();
        op.mark_validated();
        assert!(op.spec().hidden);
        assert!(op.is_validated());
    }

    // ── helpers ─────────────────────────────────────────────────────────

    fn string_node() -> SchemaNode {
        StringBuilder::new().build()
    }

    fn json_of(schema: SchemaNode, description: &str) -> ResponseSpec {
        ResponseSpec::with_content(ContentType::Json, schema).description(description)
    }

    fn builder_with_metadata() -> DocumentBuilder {
        let mut builder = DocumentBuilder::new();
        builder.title("Shop API").version("1.2.3");
        builder
    }

    // ── problem schemas ─────────────────────────────────────────────────

    #[test]
    fn problem_schema_carries_the_rfc_9457_members() {
        let schema = problem_schema();
        for member in [
            "type",
            "title",
            "status",
            "detail",
            "instance",
            "request_id",
        ] {
            assert!(
                schema.properties.contains_key(member),
                "`Problem` is missing `{member}`"
            );
        }
        assert_eq!(schema.required, ["type", "title", "status"]);
        assert!(!schema.properties.contains_key("errors"));
        assert_eq!(
            schema.additional_properties,
            Some(AdditionalProperties::Any(true))
        );
    }

    #[test]
    fn validation_problem_extends_problem_with_a_required_errors_array() {
        let schema = validation_problem_schema();
        assert_eq!(schema.required, ["type", "title", "status", "errors"]);

        let errors = schema
            .properties
            .get("errors")
            .expect("`errors` is the whole point");
        let entry = errors
            .items
            .as_deref()
            .expect("`errors` describes its items");
        for member in ["pointer", "code", "message", "params"] {
            assert!(
                entry.properties.contains_key(member),
                "a field error is missing `{member}`"
            );
        }
        assert_eq!(entry.required, ["pointer", "code", "message"]);
        assert_eq!(
            entry.properties["pointer"].format.as_deref(),
            Some("json-pointer")
        );
    }

    #[test]
    fn validation_problem_is_self_contained() {
        // It must not `$ref` `Problem`: an operation may document a 422
        // without ever documenting a plain problem response.
        let schema = validation_problem_schema();
        let mut refs = Vec::new();
        collect_schema_refs(&schema, "ValidationProblem", &mut refs);
        assert!(refs.is_empty(), "unexpected references: {refs:?}");
    }

    #[test]
    fn a_schema_registered_twice_yields_one_component_entry() {
        let mut builder = builder_with_metadata();
        builder.operation(HttpMethod::Get, "/a", |op| {
            op.response(404, ResponseSpec::problem("No such thing"));
        });
        builder.operation(HttpMethod::Get, "/b", |op| {
            op.response(409, ResponseSpec::problem("Already exists"));
        });

        let document = builder.build_unchecked();
        assert_eq!(document.components.schemas.len(), 1);
        assert!(
            document
                .components
                .schemas
                .contains_key(PROBLEM_SCHEMA_NAME)
        );
        assert_eq!(
            document.operation(HttpMethod::Get, "/a").unwrap().responses["404"].content
                ["application/problem+json"]
                .schema
                .as_ref()
                .unwrap()
                .reference
                .as_deref(),
            Some("#/components/schemas/Problem")
        );
    }

    // ── response specs ──────────────────────────────────────────────────

    #[test]
    fn redirect_documents_a_required_location_header() {
        let response =
            ResponseSpec::redirect("Go here instead").build(&mut SchemaGenerator::default());
        let location = response
            .headers
            .get("location")
            .expect("no location header");
        assert!(location.required);
        assert_eq!(
            location.schema.as_ref().unwrap().format.as_deref(),
            Some("uri-reference")
        );
        assert!(response.content.is_empty());
    }

    #[test]
    fn sse_events_accumulate_under_one_extension() {
        let spec = ResponseSpec::sse("A stream of updates")
            .sse_event("created", string_node())
            .sse_event("deleted", string_node());
        let response = spec.build(&mut SchemaGenerator::default());

        let events = response.extensions[SSE_EVENTS_EXTENSION]
            .as_object()
            .expect("x-sse-events is an object");
        assert_eq!(events.len(), 2);
        assert!(events.contains_key("created") && events.contains_key("deleted"));
        assert!(response.content.contains_key("text/event-stream"));
    }

    #[test]
    fn binary_responses_are_octet_stream_with_a_binary_format() {
        let response = ResponseSpec::binary("The file").build(&mut SchemaGenerator::default());
        let media = &response.content["application/octet-stream"];
        assert_eq!(
            media.schema.as_ref().unwrap().format.as_deref(),
            Some("binary")
        );
    }

    // ── merge semantics ─────────────────────────────────────────────────

    #[test]
    fn the_same_status_twice_keeps_the_first_description_and_unions_content() {
        let mut op = OperationBuilder::new(SchemaGenerator::default());
        op.response(200, json_of(string_node(), "The user"));
        op.response(
            200,
            ResponseSpec::with_content(ContentType::Xml, string_node())
                .description("Something else entirely")
                .header("x-total-count", string_node()),
        );

        let response = &op.spec().responses["200"];
        assert_eq!(response.description.as_deref(), Some("The user"));
        assert_eq!(response.content.len(), 2);
        assert!(response.content.contains_key("application/json"));
        assert!(response.content.contains_key("application/xml"));
        assert!(response.headers.contains_key("x-total-count"));
    }

    #[test]
    fn parameters_are_deduplicated_by_location_and_name() {
        let mut op = OperationBuilder::new(SchemaGenerator::default());
        op.parameter(
            Param::query("limit")
                .description("How many to return")
                .schema_node(string_node()),
        );
        op.parameter(Param::query("limit").description("Ignored").required(true));
        op.parameter(Param::header("limit"));

        let parameters = &op.spec().parameters;
        assert_eq!(parameters.len(), 2, "query and header `limit` are distinct");
        assert_eq!(
            parameters[0].description.as_deref(),
            Some("How many to return")
        );
        assert!(parameters[0].schema.is_some());
        assert!(
            parameters[0].required,
            "required unions across contributors"
        );
        assert_eq!(parameters[1].location, ParameterLocation::Header);
    }

    #[test]
    fn the_first_request_body_owns_required_and_later_ones_only_add_content() {
        let mut spec = OperationSpec::default();
        let mut optional = RequestBody {
            required: false,
            description: Some("The patch".to_owned()),
            ..RequestBody::default()
        };
        optional
            .content
            .insert("application/json".to_owned(), MediaType::new(string_node()));
        spec.merge_request_body(optional);

        let mut second = RequestBody {
            required: true,
            description: Some("Ignored".to_owned()),
            ..RequestBody::default()
        };
        second.content.insert(
            "application/x-www-form-urlencoded".to_owned(),
            MediaType::new(string_node()),
        );
        spec.merge_request_body(second);

        let body = spec.request_body.expect("a body was contributed");
        assert!(!body.required, "an optional body stays optional");
        assert_eq!(body.description.as_deref(), Some("The patch"));
        assert_eq!(body.content.len(), 2);
    }

    #[test]
    fn security_requirements_accumulate_as_a_set() {
        let mut op = OperationBuilder::new(SchemaGenerator::default());
        op.security(SecurityRequirement::scheme("session"));
        op.security(SecurityRequirement::scheme("session"));
        op.security(SecurityRequirement::scopes("oauth", ["users.read"]));

        let security = op
            .spec()
            .security
            .as_ref()
            .expect("security was contributed");
        assert_eq!(security.len(), 2);
    }

    #[test]
    fn public_records_an_explicit_empty_requirement() {
        let mut op = OperationBuilder::new(SchemaGenerator::default());
        op.public();
        assert_eq!(op.spec().security.as_deref(), Some(&[][..]));
    }

    #[test]
    fn responses_sort_numeric_then_range_then_default() {
        let mut spec = OperationSpec::default();
        for key in ["default", "5XX", "404", "200", "4XX", "201"] {
            spec.merge_response(key, Response::new("x"));
        }
        spec.sort_responses();
        let keys: Vec<&str> = spec.responses.keys().map(String::as_str).collect();
        assert_eq!(keys, ["200", "201", "404", "4XX", "5XX", "default"]);
    }

    #[test]
    fn into_operation_fills_in_missing_response_descriptions() {
        let mut spec = OperationSpec::default();
        spec.merge_response("404", Response::default());
        spec.merge_response("default", Response::default());
        spec.merge_response("2XX", Response::default());

        let operation = spec.into_operation();
        assert_eq!(
            operation.responses["404"].description.as_deref(),
            Some("Not Found")
        );
        assert_eq!(
            operation.responses["2XX"].description.as_deref(),
            Some("Success")
        );
        assert_eq!(
            operation.responses["default"].description.as_deref(),
            Some("Unexpected error")
        );
    }

    // ── router metadata ─────────────────────────────────────────────────

    #[test]
    fn router_metadata_only_fills_gaps() {
        let mut op = OperationBuilder::new(SchemaGenerator::default());
        op.summary("Create a user");
        op.tag("users");
        op.response(201, json_of(string_node(), "The created user"));

        let mut meta = RouteMetadata::new();
        meta.tag("admin")
            .tag("users")
            .security(SecurityRequirement::scheme("session"))
            .responds(201, ResponseSpec::empty("Router says otherwise"))
            .responds(429, ResponseSpec::problem("Rate limited"))
            .deprecate();
        meta.apply(&mut op);

        let spec = op.spec();
        assert_eq!(spec.summary.as_deref(), Some("Create a user"));
        assert_eq!(spec.tags, ["users", "admin"]);
        assert_eq!(
            spec.responses["201"].description.as_deref(),
            Some("The created user"),
            "the handler's description survives"
        );
        assert!(spec.responses.contains_key("429"));
        assert!(spec.deprecated);
        assert_eq!(spec.security.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn nested_metadata_lists_the_inner_router_first() {
        let mut inner = RouteMetadata::new();
        inner
            .tag("users")
            .responds(404, ResponseSpec::problem("Gone"));
        let mut outer = RouteMetadata::new();
        outer
            .tag("v1")
            .tag("users")
            .responds(429, ResponseSpec::problem("Slow down"))
            .hide();

        inner.extend_from(&outer);
        assert_eq!(inner.tags, ["users", "v1"]);
        assert_eq!(inner.responses.len(), 2);
        assert_eq!(inner.responses[0].0, 404);
        assert_eq!(inner.responses[1].0, 429);
        assert!(inner.hidden);
        assert!(!inner.deprecated);
    }

    #[test]
    fn overlay_fills_gaps_and_unions_collections() {
        let mut spec = OperationSpec {
            summary: Some("Handler summary".to_owned()),
            ..OperationSpec::default()
        };
        spec.merge_tag("users");
        spec.merge_response("200", Response::new("The handler's 200"));

        let mut other = OperationSpec {
            summary: Some("Router summary".to_owned()),
            description: Some("Router description".to_owned()),
            deprecated: true,
            ..OperationSpec::default()
        };
        other.merge_tag("admin");
        other.merge_response("200", Response::new("The router's 200"));
        other.merge_response("429", Response::new("Rate limited"));

        spec.overlay(&other);

        assert_eq!(spec.summary.as_deref(), Some("Handler summary"));
        assert_eq!(spec.description.as_deref(), Some("Router description"));
        assert_eq!(spec.tags, ["users", "admin"]);
        assert_eq!(
            spec.responses["200"].description.as_deref(),
            Some("The handler's 200")
        );
        assert!(spec.responses.contains_key("429"));
        assert!(spec.deprecated);
    }

    #[test]
    fn overlay_never_removes_an_explicit_opt_out() {
        let mut spec = OperationSpec {
            security: Some(Vec::new()),
            ..OperationSpec::default()
        };
        let other = OperationSpec {
            security: Some(Vec::new()),
            ..OperationSpec::default()
        };
        spec.overlay(&other);
        assert_eq!(spec.security.as_deref(), Some(&[][..]));
    }

    // ── operation ids and paths ─────────────────────────────────────────

    #[test]
    fn operation_ids_are_derived_from_the_method_and_path() {
        assert_eq!(
            derive_operation_id(HttpMethod::Get, "/users/{id}/posts"),
            "get_users_by_id_posts"
        );
        assert_eq!(
            derive_operation_id(HttpMethod::Post, "/users"),
            "post_users"
        );
        assert_eq!(derive_operation_id(HttpMethod::Get, "/"), "get");
        assert_eq!(
            derive_operation_id(HttpMethod::Get, "/static/{*path}"),
            "get_static_by_path"
        );
        assert_eq!(
            derive_operation_id(HttpMethod::Delete, "/api/v1/sessions.json"),
            "delete_api_v1_sessions_json"
        );
    }

    #[test]
    fn path_placeholders_strip_the_catch_all_marker() {
        assert_eq!(
            path_placeholders("/users/{id}/posts/{post_id}"),
            ["id", "post_id"]
        );
        assert_eq!(path_placeholders("/static/{*path}"), ["path"]);
        assert!(path_placeholders("/users").is_empty());
    }

    #[test]
    fn response_keys_are_classified() {
        assert!(is_valid_response_key("200"));
        assert!(is_valid_response_key("4XX"));
        assert!(is_valid_response_key("default"));
        assert!(!is_valid_response_key("99"));
        assert!(!is_valid_response_key("600"));
        assert!(!is_valid_response_key("okay"));
        assert!(!is_valid_response_key("6XX"));
    }

    // ── document assembly ───────────────────────────────────────────────

    #[test]
    fn document_metadata_lands_in_info() {
        let mut builder = DocumentBuilder::new();
        builder
            .title("Shop API")
            .version("1.2.3")
            .description("Everything you can buy")
            .contact("API team", "api@shop.example")
            .license_spdx("Apache-2.0")
            .server("https://api.shop.example", "production")
            .tag_description("users", "Account management")
            .security_scheme("session", SecurityScheme::cookie("sid"))
            .external_docs("https://shop.example/docs", "Guides")
            .extension("x-audience", "public");

        let document = builder.build().expect("nothing is inconsistent");
        assert_eq!(document.openapi, crate::OPENAPI_VERSION);
        assert_eq!(document.info.title, "Shop API");
        assert_eq!(document.info.version, "1.2.3");
        assert_eq!(
            document.info.license.unwrap().identifier.as_deref(),
            Some("Apache-2.0")
        );
        assert_eq!(document.servers.len(), 1);
        assert_eq!(
            document.tags[0].description.as_deref(),
            Some("Account management")
        );
        assert!(document.components.security_schemes.contains_key("session"));
        assert_eq!(document.extensions["x-audience"], Value::from("public"));
    }

    #[test]
    fn an_untitled_document_still_serialises() {
        let document = DocumentBuilder::new().build_unchecked();
        assert_eq!(document.info.title, DEFAULT_TITLE);
        assert_eq!(document.info.version, DEFAULT_VERSION);
    }

    #[test]
    fn paths_and_schemas_come_out_sorted() {
        let mut builder = builder_with_metadata();
        for path in ["/zebra", "/apple", "/mango"] {
            builder.operation(HttpMethod::Get, path, |op| {
                op.response(200, ResponseSpec::empty("ok"));
            });
        }
        let document = builder.build().expect("nothing is inconsistent");
        let paths: Vec<&str> = document.paths.keys().map(String::as_str).collect();
        assert_eq!(paths, ["/apple", "/mango", "/zebra"]);
    }

    #[test]
    fn tags_used_by_an_operation_are_declared() {
        let mut builder = builder_with_metadata();
        builder.tag_description("users", "Account management");
        builder.operation(HttpMethod::Get, "/orders", |op| {
            op.tag("orders");
            op.response(200, ResponseSpec::empty("ok"));
        });
        let document = builder.build().expect("nothing is inconsistent");
        let names: Vec<&str> = document.tags.iter().map(|tag| tag.name.as_str()).collect();
        assert_eq!(names, ["users", "orders"]);
    }

    #[test]
    fn hidden_operations_never_reach_the_document() {
        let mut builder = builder_with_metadata();
        builder.operation(HttpMethod::Get, "/internal/metrics", |op| {
            op.hidden();
            op.response(200, ResponseSpec::empty("ok"));
        });
        let document = builder.build().expect("nothing is inconsistent");
        assert!(document.paths.is_empty());
    }

    #[test]
    fn a_route_registered_twice_is_a_document_error() {
        let mut builder = builder_with_metadata();
        let mut spec = OperationSpec {
            source: Some(SourceLocation {
                file: "src/routes/users.rs",
                line: 12,
            }),
            ..OperationSpec::default()
        };
        spec.merge_response("200", Response::new("ok"));
        builder.insert_operation(HttpMethod::Get, "/users", spec.clone());
        spec.source = Some(SourceLocation {
            file: "src/routes/admin.rs",
            line: 40,
        });
        builder.insert_operation(HttpMethod::Get, "/users", spec);

        let errors = builder.build().expect_err("the conflict must be reported");
        assert!(errors.iter().any(|error| matches!(
            error,
            DocumentError::RouteConflict { method: HttpMethod::Get, path, first, second }
                if path == "/users"
                    && first.as_deref() == Some("src/routes/users.rs:12")
                    && second.as_deref() == Some("src/routes/admin.rs:40")
        )));
    }

    #[test]
    fn two_operations_may_not_share_an_id() {
        let mut builder = builder_with_metadata();
        for path in ["/users", "/people"] {
            builder.operation(HttpMethod::Get, path, |op| {
                op.operation_id("list_users");
                op.response(200, ResponseSpec::empty("ok"));
            });
        }
        let errors = builder
            .build()
            .expect_err("the duplicate id must be reported");
        assert!(errors.iter().any(|error| matches!(
            error,
            DocumentError::DuplicateOperationId { operation_id, first, second }
                if operation_id == "list_users" && first == "GET /users" && second == "GET /people"
        )));
    }

    #[test]
    fn an_unrecognised_response_key_is_a_document_error() {
        let mut builder = builder_with_metadata();
        builder.operation(HttpMethod::Get, "/users", |op| {
            op.response_key("okay", ResponseSpec::empty("ok"));
        });
        let errors = builder.build().expect_err("the bad key must be reported");
        assert!(errors.iter().any(
            |error| matches!(error, DocumentError::InvalidStatusKey { key } if key == "okay")
        ));
    }

    #[test]
    fn a_path_placeholder_without_a_parameter_is_a_document_error() {
        let mut builder = builder_with_metadata();
        builder.operation(HttpMethod::Get, "/users/{id}", |op| {
            op.response(200, ResponseSpec::empty("ok"));
        });
        let errors = builder
            .build()
            .expect_err("the missing parameter must be reported");
        assert!(errors.iter().any(|error| matches!(
            error,
            DocumentError::PathParameterMismatch { path, missing, extra }
                if path == "/users/{id}" && missing == &["id"] && extra.is_empty()
        )));
    }

    #[test]
    fn a_declared_path_parameter_satisfies_its_placeholder() {
        let mut builder = builder_with_metadata();
        builder.operation(HttpMethod::Get, "/users/{id}", |op| {
            op.parameter(Param::path("id").schema_node(string_node()));
            op.response(200, ResponseSpec::empty("ok"));
        });
        builder.build().expect("the parameter is declared");
    }

    #[test]
    fn a_requirement_naming_an_undeclared_scheme_is_a_document_error() {
        let mut builder = builder_with_metadata();
        builder.operation(HttpMethod::Delete, "/users", |op| {
            op.security(SecurityRequirement::scheme("session"));
            op.response(204, ResponseSpec::empty("Deleted"));
        });
        let errors = builder
            .build()
            .expect_err("the unknown scheme must be reported");
        assert!(errors.iter().any(|error| matches!(
            error,
            DocumentError::UnknownSecurityScheme { scheme, location }
                if scheme == "session" && location == "DELETE /users"
        )));
    }

    #[test]
    fn a_dangling_component_reference_is_a_document_error() {
        let mut builder = builder_with_metadata();
        builder.operation(HttpMethod::Get, "/users", |op| {
            op.response(
                200,
                json_of(SchemaNode::reference("#/components/schemas/User"), "Users"),
            );
        });
        let errors = builder
            .build()
            .expect_err("the dangling ref must be reported");
        assert!(errors.iter().any(|error| matches!(
            error,
            DocumentError::DanglingRef { reference, .. }
                if reference == "#/components/schemas/User"
        )));
    }

    #[test]
    fn an_external_reference_is_left_alone() {
        let mut builder = builder_with_metadata();
        builder.operation(HttpMethod::Get, "/users", |op| {
            op.response(
                200,
                json_of(
                    SchemaNode::reference("https://schemas.example/User.json"),
                    "Users",
                ),
            );
        });
        builder
            .build()
            .expect("external refs are not this crate's business");
    }

    #[test]
    fn a_shared_response_without_a_description_is_named_after_its_component() {
        let mut builder = builder_with_metadata();
        builder.shared_response("Unauthenticated", Response::default());
        let document = builder.build_unchecked();
        assert_eq!(
            document.components.responses["Unauthenticated"]
                .description
                .as_deref(),
            Some("Unauthenticated")
        );
    }

    #[test]
    fn components_snapshot_shows_what_has_been_registered() {
        let mut builder = builder_with_metadata();
        builder.security_scheme("session", SecurityScheme::cookie("sid"));
        builder.operation(HttpMethod::Get, "/users", |op| {
            op.response(422, ResponseSpec::validation_problem_of::<Never>());
        });
        let snapshot = builder.components_snapshot();
        assert!(
            snapshot
                .schemas
                .contains_key(VALIDATION_PROBLEM_SCHEMA_NAME)
        );
        assert!(snapshot.security_schemes.contains_key("session"));
    }

    #[test]
    fn webhooks_are_registered_under_their_event_name() {
        let mut builder = builder_with_metadata();
        let mut spec = OperationSpec::default();
        spec.merge_response("200", Response::new("Acknowledged"));
        builder.webhook("order.paid", HttpMethod::Post, spec);
        let document = builder.build().expect("nothing is inconsistent");
        assert!(document.webhooks["order.paid"].post.is_some());
    }

    /// A `Schema` that is never instantiated, only named.
    ///
    /// `ResponseSpec::validation_problem_of::<T>` uses `T` for the response
    /// description alone, so a type that describes itself as nothing is enough
    /// to exercise it without depending on a derive.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Never;

    impl moso_schema::Validate for Never {
        fn validate(
            &self,
            _ctx: &mut moso_schema::ValidationCtx,
        ) -> Result<(), moso_schema::ValidationErrors> {
            Ok(())
        }
    }

    impl Schema for Never {
        fn schema_name() -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("Never")
        }

        fn json_schema(_generator: &mut SchemaGenerator) -> SchemaNode {
            SchemaNode::any()
        }
    }
}
