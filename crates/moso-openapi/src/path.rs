//! Path items, operations, parameters, bodies and responses.
//!
//! One [`PathItem`] per templated path, up to eight [`Operation`]s per path
//! item. Everything here is the OpenAPI wire model: it is produced by
//! [`OperationSpec::into_operation`](crate::builder::OperationSpec::into_operation)
//! and consumed by `serde`, the [`ui`](crate::ui) and [`diff`](mod@crate::diff).

use core::fmt;
use core::str::FromStr;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::document::{ExternalDocs, Server};
use crate::security::SecurityRequirement;
use moso_schema::json_schema::SchemaNode;

/// An HTTP method that can carry an [`Operation`].
///
/// `TRACE` is modelled because OpenAPI models it; Moso's router does not route
/// it, and nothing in the framework emits one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `PUT`
    Put,
    /// `POST`
    Post,
    /// `DELETE`
    Delete,
    /// `OPTIONS`
    Options,
    /// `HEAD`
    Head,
    /// `PATCH`
    Patch,
    /// `TRACE`
    Trace,
}

impl HttpMethod {
    /// Every method, in the canonical order used for deterministic iteration
    /// and serialisation.
    pub const ALL: [HttpMethod; 8] = [
        HttpMethod::Get,
        HttpMethod::Put,
        HttpMethod::Post,
        HttpMethod::Delete,
        HttpMethod::Options,
        HttpMethod::Head,
        HttpMethod::Patch,
        HttpMethod::Trace,
    ];

    /// The lowercase spelling used as a [`PathItem`] member name.
    pub const fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "get",
            HttpMethod::Put => "put",
            HttpMethod::Post => "post",
            HttpMethod::Delete => "delete",
            HttpMethod::Options => "options",
            HttpMethod::Head => "head",
            HttpMethod::Patch => "patch",
            HttpMethod::Trace => "trace",
        }
    }

    /// The uppercase spelling used on the wire and in diagnostics.
    pub const fn as_upper_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Put => "PUT",
            HttpMethod::Post => "POST",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Options => "OPTIONS",
            HttpMethod::Head => "HEAD",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Trace => "TRACE",
        }
    }

    /// Whether a request with this method may carry a body that the API
    /// documents. Used to suppress a `requestBody` on `GET`/`HEAD`/`DELETE`.
    pub const fn allows_request_body(self) -> bool {
        matches!(self, HttpMethod::Post | HttpMethod::Put | HttpMethod::Patch)
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_upper_str())
    }
}

/// Returned by [`HttpMethod::from_str`] when the input names no HTTP method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownMethod(pub String);

impl fmt::Display for UnknownMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not an HTTP method that can carry an operation",
            self.0
        )
    }
}

impl core::error::Error for UnknownMethod {}

impl FromStr for HttpMethod {
    type Err = UnknownMethod;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        HttpMethod::ALL
            .into_iter()
            .find(|method| method.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| UnknownMethod(s.to_owned()))
    }
}

/// The operations available at one templated path.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PathItem {
    /// A reference to a path item defined in `components.pathItems`.
    #[serde(rename = "$ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// A short summary applying to every operation at this path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// A description applying to every operation at this path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The `GET` operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get: Option<Operation>,
    /// The `PUT` operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub put: Option<Operation>,
    /// The `POST` operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<Operation>,
    /// The `DELETE` operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete: Option<Operation>,
    /// The `OPTIONS` operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Operation>,
    /// The `HEAD` operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<Operation>,
    /// The `PATCH` operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<Operation>,
    /// The `TRACE` operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<Operation>,
    /// Servers overriding the document-level ones for this path.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<Server>,
    /// Parameters shared by every operation at this path.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl PathItem {
    /// Borrow the operation for `method`, if one is registered.
    pub fn operation(&self, method: HttpMethod) -> Option<&Operation> {
        match method {
            HttpMethod::Get => self.get.as_ref(),
            HttpMethod::Put => self.put.as_ref(),
            HttpMethod::Post => self.post.as_ref(),
            HttpMethod::Delete => self.delete.as_ref(),
            HttpMethod::Options => self.options.as_ref(),
            HttpMethod::Head => self.head.as_ref(),
            HttpMethod::Patch => self.patch.as_ref(),
            HttpMethod::Trace => self.trace.as_ref(),
        }
    }

    /// Mutably borrow the operation for `method`, if one is registered.
    pub fn operation_mut(&mut self, method: HttpMethod) -> Option<&mut Operation> {
        self.slot_mut(method).as_mut()
    }

    /// Install `operation` at `method`, returning whatever was there before.
    pub fn set_operation(&mut self, method: HttpMethod, operation: Operation) -> Option<Operation> {
        self.slot_mut(method).replace(operation)
    }

    /// Remove and return the operation at `method`.
    pub fn take_operation(&mut self, method: HttpMethod) -> Option<Operation> {
        self.slot_mut(method).take()
    }

    /// Iterate the registered operations in [`HttpMethod::ALL`] order.
    pub fn operations(&self) -> impl Iterator<Item = (HttpMethod, &Operation)> {
        HttpMethod::ALL
            .into_iter()
            .filter_map(|method| self.operation(method).map(|op| (method, op)))
    }

    /// `true` when no operation is registered at this path.
    pub fn is_empty(&self) -> bool {
        HttpMethod::ALL
            .into_iter()
            .all(|method| self.operation(method).is_none())
    }

    fn slot_mut(&mut self, method: HttpMethod) -> &mut Option<Operation> {
        match method {
            HttpMethod::Get => &mut self.get,
            HttpMethod::Put => &mut self.put,
            HttpMethod::Post => &mut self.post,
            HttpMethod::Delete => &mut self.delete,
            HttpMethod::Options => &mut self.options,
            HttpMethod::Head => &mut self.head,
            HttpMethod::Patch => &mut self.patch,
            HttpMethod::Trace => &mut self.trace,
        }
    }
}

/// A single API operation: one handler, described.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Operation {
    /// Tags grouping this operation in the documentation UI.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// One-line summary, taken from the first line of the handler's doc comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Long description, taken from the rest of the handler's doc comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A pointer to prose documentation for this operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_docs: Option<ExternalDocs>,
    /// Unique identifier, used as the method name by client generators.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Path, query, header and cookie parameters.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    /// The request body, if this operation reads one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<RequestBody>,
    /// Responses keyed by status code, `NXX` range, or `default`.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub responses: IndexMap<String, Response>,
    /// Out-of-band requests this operation may make, keyed by name.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub callbacks: IndexMap<String, IndexMap<String, PathItem>>,
    /// Whether clients should migrate away from this operation.
    #[serde(skip_serializing_if = "crate::is_false")]
    pub deprecated: bool,
    /// Security requirements.
    ///
    /// `None` inherits the document-level requirements; `Some(vec![])`
    /// explicitly opts out of them, which is how a public endpoint inside an
    /// otherwise-authenticated API is expressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<Vec<SecurityRequirement>>,
    /// Servers overriding the path- and document-level ones.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<Server>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl Operation {
    /// Borrow the response registered for `status`.
    pub fn response(&self, status: u16) -> Option<&Response> {
        self.responses.get(&status.to_string())
    }

    /// Borrow the parameter at `location` named `name`.
    pub fn parameter(&self, location: ParameterLocation, name: &str) -> Option<&Parameter> {
        self.parameters
            .iter()
            .find(|p| p.location == location && p.name == name)
    }
}

/// Where a [`Parameter`] is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParameterLocation {
    /// In the query string.
    Query,
    /// In a request header.
    Header,
    /// Substituted into a `{placeholder}` in the path. Always required.
    Path,
    /// In the `Cookie` header, as one cookie.
    Cookie,
}

impl ParameterLocation {
    /// The wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            ParameterLocation::Query => "query",
            ParameterLocation::Header => "header",
            ParameterLocation::Path => "path",
            ParameterLocation::Cookie => "cookie",
        }
    }

    /// The style OpenAPI defaults to for this location when none is given.
    pub const fn default_style(self) -> ParameterStyle {
        match self {
            ParameterLocation::Query | ParameterLocation::Cookie => ParameterStyle::Form,
            ParameterLocation::Header | ParameterLocation::Path => ParameterStyle::Simple,
        }
    }
}

impl fmt::Display for ParameterLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a parameter's value is serialised into the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParameterStyle {
    /// `;name=value`, path only.
    Matrix,
    /// `.value`, path only.
    Label,
    /// `name=value`, the query and cookie default.
    Form,
    /// `value`, the path and header default.
    Simple,
    /// `value value`, arrays in the query.
    SpaceDelimited,
    /// `value|value`, arrays in the query.
    PipeDelimited,
    /// `name[key]=value`, objects in the query. What `#[schema(flatten_bracket)]` emits.
    DeepObject,
}

impl ParameterStyle {
    /// The wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            ParameterStyle::Matrix => "matrix",
            ParameterStyle::Label => "label",
            ParameterStyle::Form => "form",
            ParameterStyle::Simple => "simple",
            ParameterStyle::SpaceDelimited => "spaceDelimited",
            ParameterStyle::PipeDelimited => "pipeDelimited",
            ParameterStyle::DeepObject => "deepObject",
        }
    }
}

impl fmt::Display for ParameterStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One path, query, header or cookie parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parameter {
    /// The parameter name. For a path parameter, matches a `{placeholder}`.
    pub name: String,
    /// Where the parameter is carried.
    #[serde(rename = "in")]
    pub location: ParameterLocation,
    /// What the parameter means. Taken from the field's doc comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the request is invalid without it. Always `true` for path parameters.
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub required: bool,
    /// Whether clients should stop sending it.
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub deprecated: bool,
    /// Whether an empty value is meaningful. Query parameters only.
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub allow_empty_value: bool,
    /// Serialisation style. Omitted when it equals [`ParameterLocation::default_style`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<ParameterStyle>,
    /// Whether array and object members get one occurrence each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explode: Option<bool>,
    /// Whether RFC 3986 reserved characters may appear unescaped.
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub allow_reserved: bool,
    /// The parameter's schema, carrying its type and every constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<SchemaNode>,
    /// A single example value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<Value>,
    /// Named example values.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub examples: IndexMap<String, Example>,
    /// A media-type-keyed alternative to [`Parameter::schema`], for parameters
    /// whose value is itself a serialised document.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub content: IndexMap<String, MediaType>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl Parameter {
    /// A parameter with only the two required members.
    pub fn new(name: impl Into<String>, location: ParameterLocation) -> Self {
        Self {
            name: name.into(),
            location,
            description: None,
            required: location == ParameterLocation::Path,
            deprecated: false,
            allow_empty_value: false,
            style: None,
            explode: None,
            allow_reserved: false,
            schema: None,
            example: None,
            examples: IndexMap::new(),
            content: IndexMap::new(),
            extensions: IndexMap::new(),
        }
    }

    /// Fill in members this parameter lacks from `other`, keeping its own.
    ///
    /// Used when two describers contribute the same `(location, name)`: the
    /// first contribution wins on every member it set, and the second supplies
    /// only what is still missing.
    pub fn merge_missing(&mut self, other: Parameter) {
        let Parameter {
            name: _,
            location: _,
            description,
            required,
            deprecated,
            allow_empty_value,
            style,
            explode,
            allow_reserved,
            schema,
            example,
            examples,
            content,
            extensions,
        } = other;

        if self.description.is_none() {
            self.description = description;
        }
        if self.style.is_none() {
            self.style = style;
        }
        if self.explode.is_none() {
            self.explode = explode;
        }
        if self.schema.is_none() {
            self.schema = schema;
        }
        if self.example.is_none() {
            self.example = example;
        }

        // The booleans are unions rather than first-writer-wins: if any
        // contributor says a parameter is required, it is required. A path
        // parameter is required by construction and stays that way.
        self.required |= required;
        self.deprecated |= deprecated;
        self.allow_empty_value |= allow_empty_value;
        self.allow_reserved |= allow_reserved;

        for (key, value) in examples {
            self.examples.entry(key).or_insert(value);
        }
        for (key, value) in content {
            self.content.entry(key).or_insert(value);
        }
        for (key, value) in extensions {
            self.extensions.entry(key).or_insert(value);
        }
    }
}

/// The body of a request, keyed by media type.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RequestBody {
    /// A reference to a request body defined in `components.requestBodies`.
    #[serde(rename = "$ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// What the body carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Representations of the body, keyed by media type.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub content: IndexMap<String, MediaType>,
    /// Whether a body must be present.
    #[serde(skip_serializing_if = "crate::is_false")]
    pub required: bool,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

/// One response of an operation.
///
/// Either a `$ref` to `components.responses`, or an inline response — in which
/// case [`Response::description`] is required by the specification.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Response {
    /// A reference to a response defined in `components.responses`.
    #[serde(rename = "$ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// What this response means. Required unless [`Response::reference`] is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Response headers, keyed by name. `Content-Type` is not listed here.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub headers: IndexMap<String, Header>,
    /// Representations of the body, keyed by media type.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub content: IndexMap<String, MediaType>,
    /// Operations that can be called from values in this response.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub links: IndexMap<String, Link>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl Response {
    /// An inline response carrying only a description.
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: Some(description.into()),
            ..Self::default()
        }
    }

    /// A `$ref` to a response in `components.responses`.
    pub fn reference(name: &str) -> Self {
        Self {
            reference: Some(format!("#/components/responses/{name}")),
            ..Self::default()
        }
    }

    /// Fill in members this response lacks from `other`, keeping its own.
    ///
    /// This is what makes registering the same status twice idempotent: the
    /// description of the first registration survives, and content types the
    /// first did not describe are added rather than replacing it.
    pub fn merge_missing(&mut self, other: Response) {
        let Response {
            reference,
            description,
            headers,
            content,
            links,
            extensions,
        } = other;

        if self.reference.is_none() {
            self.reference = reference;
        }
        if self.description.is_none() {
            self.description = description;
        }

        for (key, value) in headers {
            self.headers.entry(key).or_insert(value);
        }
        for (key, value) in content {
            self.content.entry(key).or_insert(value);
        }
        for (key, value) in links {
            self.links.entry(key).or_insert(value);
        }
        for (key, value) in extensions {
            self.extensions.entry(key).or_insert(value);
        }
    }
}

/// One representation of a body: a schema plus examples.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MediaType {
    /// The schema of the payload in this representation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<SchemaNode>,
    /// A single example payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<Value>,
    /// Named example payloads.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub examples: IndexMap<String, Example>,
    /// Per-property encoding rules. `multipart/*` and form bodies only.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub encoding: IndexMap<String, Encoding>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl MediaType {
    /// A representation described by `schema`.
    pub fn new(schema: SchemaNode) -> Self {
        Self {
            schema: Some(schema),
            ..Self::default()
        }
    }

    /// A representation with no schema — an intentionally unmodelled payload.
    pub fn opaque() -> Self {
        Self::default()
    }
}

/// A response header. A [`Parameter`] without a name or a location.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Header {
    /// A reference to a header defined in `components.headers`.
    #[serde(rename = "$ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// What the header carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the header is always sent.
    #[serde(skip_serializing_if = "crate::is_false")]
    pub required: bool,
    /// Whether clients should stop relying on it.
    #[serde(skip_serializing_if = "crate::is_false")]
    pub deprecated: bool,
    /// Serialisation style. Only [`ParameterStyle::Simple`] is valid for headers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<ParameterStyle>,
    /// Whether array and object members get one occurrence each.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explode: Option<bool>,
    /// The header value's schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<SchemaNode>,
    /// A single example value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<Value>,
    /// Named example values.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub examples: IndexMap<String, Example>,
    /// A media-type-keyed alternative to [`Header::schema`].
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub content: IndexMap<String, MediaType>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl Header {
    /// A header described by `schema`.
    pub fn new(schema: SchemaNode) -> Self {
        Self {
            schema: Some(schema),
            ..Self::default()
        }
    }

    /// Attach a description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Mark the header as always present.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
}

/// How one property of a `multipart` or form request body is encoded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Encoding {
    /// The media type of this part.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Extra headers on this part. `multipart` only.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub headers: IndexMap<String, Header>,
    /// Serialisation style. Form bodies only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<ParameterStyle>,
    /// Whether array members get one occurrence each.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explode: Option<bool>,
    /// Whether RFC 3986 reserved characters may appear unescaped.
    #[serde(skip_serializing_if = "crate::is_false")]
    pub allow_reserved: bool,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

/// A named example value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Example {
    /// A reference to an example defined in `components.examples`.
    #[serde(rename = "$ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// A one-line label for the example.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// What the example demonstrates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The example value itself. Mutually exclusive with [`Example::external_value`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// A URL to the example value, for payloads that cannot be inlined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_value: Option<String>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl Example {
    /// An example carrying an inline value.
    pub fn value(value: impl Into<Value>) -> Self {
        Self {
            value: Some(value.into()),
            ..Self::default()
        }
    }
}

/// A design-time link from a response to another operation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Link {
    /// A reference to a link defined in `components.links`.
    #[serde(rename = "$ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// A URI reference to the target operation. Mutually exclusive with [`Link::operation_id`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_ref: Option<String>,
    /// The `operationId` of the target operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Values or runtime expressions for the target operation's parameters.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub parameters: IndexMap<String, Value>,
    /// A value or runtime expression for the target operation's request body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<Value>,
    /// What following this link achieves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A server to use for the target operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<Server>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl Link {
    /// A link to the operation with the given `operationId`.
    pub fn to_operation(operation_id: impl Into<String>) -> Self {
        Self {
            operation_id: Some(operation_id.into()),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moso_schema::json_schema::JsonType;
    use serde_json::json;

    fn string_schema() -> SchemaNode {
        SchemaNode::of_type(JsonType::String)
    }

    #[test]
    fn methods_parse_case_insensitively() {
        assert_eq!("get".parse::<HttpMethod>().unwrap(), HttpMethod::Get);
        assert_eq!("GET".parse::<HttpMethod>().unwrap(), HttpMethod::Get);
        assert_eq!("PaTcH".parse::<HttpMethod>().unwrap(), HttpMethod::Patch);
        assert_eq!("TRACE".parse::<HttpMethod>().unwrap(), HttpMethod::Trace);
    }

    #[test]
    fn an_unknown_method_names_itself_in_the_error() {
        let error = "CONNECT".parse::<HttpMethod>().unwrap_err();
        assert_eq!(error, UnknownMethod("CONNECT".to_owned()));
        assert!(error.to_string().contains("CONNECT"));
    }

    #[test]
    fn every_method_round_trips_through_its_wire_spelling() {
        for method in HttpMethod::ALL {
            assert_eq!(method.as_str().parse::<HttpMethod>().unwrap(), method);
            assert_eq!(method.as_upper_str().parse::<HttpMethod>().unwrap(), method);
        }
    }

    #[test]
    fn path_items_round_trip_through_json() {
        let mut item = PathItem::default();
        let mut operation = Operation {
            summary: Some("List users".to_owned()),
            operation_id: Some("list_users".to_owned()),
            ..Operation::default()
        };
        operation
            .responses
            .insert("200".to_owned(), Response::new("the users"));
        item.set_operation(HttpMethod::Get, operation);

        let text = serde_json::to_string(&item).unwrap();
        let back: PathItem = serde_json::from_str(&text).unwrap();
        assert_eq!(item, back);
    }

    #[test]
    fn extensions_round_trip_verbatim() {
        let text = r#"{"get":{"x-internal":true,"responses":{"200":{"description":"ok"}}}}"#;
        let item: PathItem = serde_json::from_str(text).unwrap();
        let operation = item.operation(HttpMethod::Get).unwrap();
        assert_eq!(operation.extensions.get("x-internal"), Some(&json!(true)));
        let round_tripped: PathItem = serde_json::from_str(&serde_json::to_string(&item).unwrap())
            .expect("re-parses after a serialise");
        assert_eq!(item, round_tripped);
    }

    #[test]
    fn operations_iterate_in_canonical_order() {
        let mut item = PathItem::default();
        item.set_operation(HttpMethod::Delete, Operation::default());
        item.set_operation(HttpMethod::Get, Operation::default());
        item.set_operation(HttpMethod::Post, Operation::default());
        let methods: Vec<_> = item.operations().map(|(method, _)| method).collect();
        assert_eq!(
            methods,
            [HttpMethod::Get, HttpMethod::Post, HttpMethod::Delete]
        );
    }

    #[test]
    fn parameter_merge_keeps_the_first_contribution() {
        let mut first = Parameter::new("limit", ParameterLocation::Query);
        first.description = Some("how many".to_owned());

        let mut second = Parameter::new("limit", ParameterLocation::Query);
        second.description = Some("clobbered".to_owned());
        second.schema = Some(string_schema());
        second.required = true;
        second.explode = Some(true);

        first.merge_missing(second);

        assert_eq!(first.description.as_deref(), Some("how many"));
        assert_eq!(first.schema, Some(string_schema()));
        assert!(first.required, "required is a union, not first-writer-wins");
        assert_eq!(first.explode, Some(true));
    }

    #[test]
    fn parameter_merge_never_unsets_a_required_path_parameter() {
        let mut path = Parameter::new("id", ParameterLocation::Path);
        let optional = Parameter::new("id", ParameterLocation::Path);
        path.merge_missing(optional);
        assert!(path.required);
    }

    #[test]
    fn parameter_merge_adds_only_absent_map_entries() {
        let mut first = Parameter::new("limit", ParameterLocation::Query);
        first.examples.insert("small".to_owned(), Example::value(1));
        first.extensions.insert("x-a".to_owned(), json!("kept"));

        let mut second = Parameter::new("limit", ParameterLocation::Query);
        second
            .examples
            .insert("small".to_owned(), Example::value(9));
        second
            .examples
            .insert("large".to_owned(), Example::value(99));
        second.extensions.insert("x-a".to_owned(), json!("dropped"));
        second.extensions.insert("x-b".to_owned(), json!("added"));

        first.merge_missing(second);

        assert_eq!(first.examples["small"].value, Some(json!(1)));
        assert_eq!(first.examples["large"].value, Some(json!(99)));
        assert_eq!(first.extensions["x-a"], json!("kept"));
        assert_eq!(first.extensions["x-b"], json!("added"));
    }

    #[test]
    fn response_merge_keeps_its_description_and_gains_content() {
        let mut first = Response::new("the user");
        first.content.insert(
            "application/json".to_owned(),
            MediaType::new(string_schema()),
        );

        let mut second = Response::new("clobbered");
        second
            .content
            .insert("application/json".to_owned(), MediaType::opaque());
        second
            .content
            .insert("text/plain; charset=utf-8".to_owned(), MediaType::opaque());
        second
            .headers
            .insert("x-total".to_owned(), Header::new(string_schema()));

        first.merge_missing(second);

        assert_eq!(first.description.as_deref(), Some("the user"));
        assert_eq!(
            first.content["application/json"].schema,
            Some(string_schema()),
            "the first contribution's media type survives"
        );
        assert!(first.content.contains_key("text/plain; charset=utf-8"));
        assert!(first.headers.contains_key("x-total"));
    }

    #[test]
    fn response_merge_fills_an_absent_description() {
        let mut first = Response::default();
        first.merge_missing(Response::new("supplied later"));
        assert_eq!(first.description.as_deref(), Some("supplied later"));
    }

    #[test]
    fn parameter_required_is_skipped_when_false() {
        let parameter = Parameter::new("limit", ParameterLocation::Query);
        let text = serde_json::to_string(&parameter).unwrap();
        assert!(!text.contains("required"), "{text}");
        assert!(text.contains(r#""in":"query""#), "{text}");
    }
}
