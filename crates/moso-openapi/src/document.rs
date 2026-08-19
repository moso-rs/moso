//! The top-level OpenAPI document and its metadata objects.
//!
//! These types are a faithful, owned model of the OpenAPI 3.1.1 specification's
//! root objects. They are `Serialize` *and* `Deserialize` because
//! `moso openapi check` has to read a committed `openapi.json` back in and
//! [`diff`](mod@crate::diff) it against the freshly assembled one.
//!
//! Unknown keys are not an error: every object keeps an `extensions` map
//! flattened into it, so `x-*` members round-trip verbatim.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::path::{
    Example, Header, HttpMethod, Link, MediaType, Operation, Parameter, ParameterLocation,
    PathItem, RequestBody, Response,
};
use crate::security::{OAuthFlow, SecurityRequirement, SecurityScheme};
use moso_schema::json_schema::{AdditionalProperties, SchemaNode};

/// A complete OpenAPI 3.1.1 document.
///
/// Built once, at boot, by [`DocumentBuilder`](crate::builder::DocumentBuilder)
/// and then serialised once and served from a cached byte slice.
///
/// ```
/// use moso_openapi::{DocumentBuilder, OPENAPI_VERSION};
///
/// let mut builder = DocumentBuilder::new();
/// builder.title("Shop API").version("0.1.0");
///
/// let document = builder.build().expect("a well-formed document");
/// assert_eq!(document.openapi, OPENAPI_VERSION);
/// assert_eq!(document.info.title, "Shop API");
///
/// // Serialisation is deterministic, so the committed `openapi.json` diffs cleanly.
/// let once = serde_json::to_string(&document).unwrap();
/// let twice = serde_json::to_string(&document).unwrap();
/// assert_eq!(once, twice);
/// ```
///
/// An application never builds one directly: `App::build()` walks the composed
/// router and produces it, and `app.openapi()` hands it back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    /// The OpenAPI version string. Always [`OPENAPI_VERSION`](crate::OPENAPI_VERSION).
    #[serde(default = "default_openapi_version")]
    pub openapi: String,

    /// Title, version and other API-level metadata.
    pub info: Info,

    /// The JSON Schema dialect every schema in this document conforms to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_schema_dialect: Option<String>,

    /// Base URLs the API is served from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<Server>,

    /// Operations, keyed by templated path (`/users/{id}`).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub paths: IndexMap<String, PathItem>,

    /// Incoming webhooks the API expects to receive, keyed by event name.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub webhooks: IndexMap<String, PathItem>,

    /// Reusable schemas, responses, parameters and security schemes.
    #[serde(default, skip_serializing_if = "Components::is_empty")]
    pub components: Components,

    /// Security requirements applied to every operation that does not override them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security: Vec<SecurityRequirement>,

    /// Tag declarations, giving human-readable descriptions to operation groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<Tag>,

    /// A pointer to prose documentation for the API as a whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_docs: Option<ExternalDocs>,

    /// `x-*` specification extensions, round-tripped verbatim.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

fn default_openapi_version() -> String {
    crate::OPENAPI_VERSION.to_owned()
}

impl Default for Document {
    fn default() -> Self {
        Self {
            openapi: default_openapi_version(),
            info: Info::default(),
            // Omitted by default so strict tooling accepts the document; see the
            // note in `DocumentBuilder::new`. `.json_schema_dialect(..)` sets it.
            json_schema_dialect: None,
            servers: Vec::new(),
            paths: IndexMap::new(),
            webhooks: IndexMap::new(),
            components: Components::default(),
            security: Vec::new(),
            tags: Vec::new(),
            external_docs: None,
            extensions: IndexMap::new(),
        }
    }
}

impl Document {
    /// A new, empty document carrying the given [`Info`].
    pub fn new(info: Info) -> Self {
        Self {
            info,
            ..Self::default()
        }
    }

    /// Borrow the [`PathItem`] registered at `path`, if any.
    pub fn path_item(&self, path: &str) -> Option<&PathItem> {
        self.paths.get(path)
    }

    /// Borrow the operation registered at `method` `path`, if any.
    pub fn operation(&self, method: HttpMethod, path: &str) -> Option<&Operation> {
        self.paths.get(path).and_then(|item| item.operation(method))
    }

    /// Iterate every operation in the document in `(path, method, operation)` order.
    ///
    /// Paths are visited in map order and methods in the canonical order of
    /// [`HttpMethod::ALL`], so the iteration is deterministic.
    pub fn operations(&self) -> impl Iterator<Item = (&str, HttpMethod, &Operation)> {
        self.paths.iter().flat_map(|(path, item)| {
            item.operations()
                .map(move |(method, op)| (path.as_str(), method, op))
        })
    }

    /// Apply the canonical output ordering.
    ///
    /// Paths, webhooks and every component map are sorted lexicographically by
    /// key; each operation's responses are sorted by status with `default`
    /// last. This is what makes a committed `openapi.json` diff cleanly
    /// regardless of route-registration order.
    pub fn sort_for_output(&mut self) {
        self.paths.sort_unstable_keys();
        self.webhooks.sort_unstable_keys();
        self.components.sort();
        for item in self.paths.values_mut().chain(self.webhooks.values_mut()) {
            for method in HttpMethod::ALL {
                if let Some(operation) = item.operation_mut(method) {
                    operation.responses.sort_by(|left, _, right, _| {
                        response_key_rank(left).cmp(&response_key_rank(right))
                    });
                }
            }
        }
    }

    /// Drop every path that does not start with `prefix`, and strip the prefix.
    ///
    /// Backs `moso openapi export --prefix /api/v1`.
    ///
    /// The prefix only matches on a segment boundary: `/api/v1` keeps
    /// `/api/v1/users` and drops `/api/v11/users`. A path that *is* the prefix
    /// becomes `/`. Relative order is preserved, and `components` is left
    /// alone — a schema that becomes unreferenced is still valid, and dropping
    /// it would make two exports of the same application disagree about what
    /// `#/components/schemas/User` means.
    pub fn filter_prefix(&mut self, prefix: &str) {
        let prefix = prefix.trim_end_matches('/');
        if prefix.is_empty() {
            return;
        }
        let mut kept = IndexMap::with_capacity(self.paths.len());
        for (path, item) in core::mem::take(&mut self.paths) {
            let Some(rest) = path.strip_prefix(prefix) else {
                continue;
            };
            if !rest.is_empty() && !rest.starts_with('/') {
                continue;
            }
            let stripped = if rest.is_empty() {
                "/".to_owned()
            } else {
                rest.to_owned()
            };
            kept.insert(stripped, item);
        }
        self.paths = kept;
    }

    /// Check the document's internal consistency.
    ///
    /// Catches, in this order:
    ///
    /// 1. duplicate `operationId`s (naming both operations),
    /// 2. `$ref`s that point at a component this document does not define,
    /// 3. path templates whose `{placeholders}` do not match the declared
    ///    `in: path` parameters, in either direction,
    /// 4. responses that are neither a `$ref` nor carry the required
    ///    `description`,
    /// 5. security requirements naming a scheme that is not in
    ///    `components.securitySchemes`, giving scopes to a scheme that takes
    ///    none, or naming a scope no OAuth flow declares,
    /// 6. schemas whose `required` array names a property they do not declare,
    ///    or names the same property twice,
    /// 7. OAuth flows missing the URL their flow type requires, and server
    ///    variables whose `enum` does not contain their `default`.
    ///
    /// Returns every problem found rather than the first, because a boot error
    /// that reports one issue at a time is a bad boot error.
    ///
    /// Not detected: a literally empty `required: []`. `SchemaNode::required`
    /// is `#[serde(default, skip_serializing_if = "Vec::is_empty")]`, so an
    /// empty array and an absent member deserialise to the same value and
    /// Moso's own emitter can never produce one.
    pub fn validate_self(&self) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        self.check_operation_ids(&mut problems);
        self.check_references(&mut problems);
        self.check_path_parameters(&mut problems);
        self.check_responses(&mut problems);
        self.check_security(&mut problems);
        self.check_schemas(&mut problems);
        self.check_servers(&mut problems);
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    /// Serialise to compact JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Serialise to indented JSON, the form committed to a repository.
    ///
    /// A trailing newline is included so the file is well-formed for `diff`,
    /// `git` and POSIX tooling.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        let mut out = serde_json::to_string_pretty(self)?;
        out.push('\n');
        Ok(out)
    }

    /// Serialise to the pre-rendered bytes served at `/openapi.json`.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Serialise to YAML, as served at `/openapi.yaml`.
    ///
    /// Implemented with a small deterministic emitter over the JSON value tree
    /// rather than a YAML dependency: the subset of YAML needed to represent a
    /// JSON document is tiny, and a dependency here would be paid for by every
    /// Moso application.
    pub fn to_yaml(&self) -> Result<String, serde_json::Error> {
        let value = serde_json::to_value(self)?;
        let mut out = String::new();
        match &value {
            Value::Object(map) if !map.is_empty() => yaml::block_map(map, 0, &mut out),
            other => {
                out.push_str(&yaml::scalar(other));
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// Parse a document from JSON, as `moso openapi check` does for the
    /// committed file.
    pub fn from_json(source: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(source)
    }
}

/// A weak `ETag` for a serialised document body.
///
/// Stable for identical bytes and cheap enough to compute once at boot. The
/// returned value includes the quotes and the `W/` prefix, ready to be used as
/// a header value.
pub fn etag_for(bytes: &[u8]) -> String {
    // FNV-1a, 64 bit. Not a cryptographic hash and not meant to be: an ETag
    // only has to change when the bytes change, and this one is a dependency-
    // free 30-line-equivalent that runs over a 200 kB document in microseconds.
    // The length is included so that two documents of different sizes cannot
    // collide on the hash alone.
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("W/\"{:x}-{hash:016x}\"", bytes.len())
}

/// API-level metadata: the `info` object.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Info {
    /// Human-readable title of the API.
    pub title: String,
    /// A short summary, one line, no markup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Long-form description. CommonMark is permitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// URL of the terms of service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms_of_service: Option<String>,
    /// Who to contact about this API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact: Option<Contact>,
    /// The licence the API is offered under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<License>,
    /// Version of *the API document*, not of the OpenAPI specification.
    pub version: String,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl Info {
    /// An `info` object with the two required members.
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            ..Self::default()
        }
    }
}

/// Contact details for the people responsible for the API.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Contact {
    /// Identifying name of the contact person or organisation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// URL pointing to the contact information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Email address of the contact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

/// The licence the API is made available under.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct License {
    /// Licence name, e.g. `Apache 2.0`.
    pub name: String,
    /// An SPDX expression, e.g. `Apache-2.0 OR MIT`. Mutually exclusive with [`License::url`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// URL of the licence text. Mutually exclusive with [`License::identifier`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl License {
    /// A licence identified by an SPDX expression.
    pub fn spdx(identifier: impl Into<String>) -> Self {
        let identifier = identifier.into();
        Self {
            name: identifier.clone(),
            identifier: Some(identifier),
            url: None,
            extensions: IndexMap::new(),
        }
    }

    /// A licence identified by name and a URL to its text.
    pub fn named(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            identifier: None,
            url: Some(url.into()),
            extensions: IndexMap::new(),
        }
    }
}

/// A base URL the API is served from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Server {
    /// The base URL. May contain `{variable}` placeholders.
    pub url: String,
    /// What this server is, e.g. `production`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Substitutions for the `{variable}` placeholders in [`Server::url`].
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub variables: IndexMap<String, ServerVariable>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl Server {
    /// A server with a URL and a description.
    pub fn new(url: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            description: Some(description.into()),
            variables: IndexMap::new(),
            extensions: IndexMap::new(),
        }
    }

    /// Add a substitution for a `{variable}` in the URL.
    pub fn variable(mut self, name: impl Into<String>, variable: ServerVariable) -> Self {
        self.variables.insert(name.into(), variable);
        self
    }
}

/// A substitution for a `{variable}` placeholder in a [`Server`] URL.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ServerVariable {
    /// The permitted values. Must be non-empty if present, and must contain [`ServerVariable::default_value`].
    #[serde(rename = "enum", skip_serializing_if = "Vec::is_empty")]
    pub enumeration: Vec<String>,
    /// The value used when the client supplies none.
    #[serde(rename = "default")]
    pub default_value: String,
    /// What this variable selects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl ServerVariable {
    /// A free-form variable with a default value.
    pub fn new(default_value: impl Into<String>) -> Self {
        Self {
            default_value: default_value.into(),
            ..Self::default()
        }
    }

    /// Restrict the variable to a closed set of values.
    pub fn options(mut self, values: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.enumeration = values.into_iter().map(Into::into).collect();
        self
    }
}

/// A named group of operations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Tag {
    /// The tag name, as referenced from [`Operation::tags`].
    pub name: String,
    /// What this group of operations is for. CommonMark is permitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A pointer to prose documentation for this group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_docs: Option<ExternalDocs>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl Tag {
    /// A tag with only a name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Attach a description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// A pointer to documentation hosted outside the OpenAPI document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExternalDocs {
    /// What the linked documentation covers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The URL of the documentation.
    pub url: String,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl ExternalDocs {
    /// A link with a URL and a description.
    pub fn new(url: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            description: Some(description.into()),
            url: url.into(),
            extensions: IndexMap::new(),
        }
    }
}

/// The reusable objects a document refers to by `$ref`.
///
/// `components.schemas` is populated wholesale from the
/// [`SchemaGenerator`](moso_schema::json_schema::SchemaGenerator) that every
/// [`OperationBuilder`](crate::builder::OperationBuilder) writes into, which is
/// why two distinct Rust types with the same `schema_name()` are a boot error
/// rather than a silent overwrite.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Components {
    /// Named JSON Schemas, referenced as `#/components/schemas/{name}`.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub schemas: IndexMap<String, SchemaNode>,
    /// Named responses, referenced as `#/components/responses/{name}`.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub responses: IndexMap<String, Response>,
    /// Named parameters, referenced as `#/components/parameters/{name}`.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub parameters: IndexMap<String, Parameter>,
    /// Named examples, referenced as `#/components/examples/{name}`.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub examples: IndexMap<String, Example>,
    /// Named request bodies, referenced as `#/components/requestBodies/{name}`.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub request_bodies: IndexMap<String, crate::path::RequestBody>,
    /// Named headers, referenced as `#/components/headers/{name}`.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub headers: IndexMap<String, Header>,
    /// Named security schemes, referenced from [`SecurityRequirement`]s by key.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub security_schemes: IndexMap<String, SecurityScheme>,
    /// Named links, referenced as `#/components/links/{name}`.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub links: IndexMap<String, Link>,
    /// Named path items, referenced as `#/components/pathItems/{name}`.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub path_items: IndexMap<String, PathItem>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl Components {
    /// `true` when nothing is registered, in which case the member is omitted entirely.
    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty()
            && self.responses.is_empty()
            && self.parameters.is_empty()
            && self.examples.is_empty()
            && self.request_bodies.is_empty()
            && self.headers.is_empty()
            && self.security_schemes.is_empty()
            && self.links.is_empty()
            && self.path_items.is_empty()
            && self.extensions.is_empty()
    }

    /// Resolve an internal `#/components/...` JSON pointer to `true` if the
    /// target exists.
    ///
    /// Only internal references are understood; an external `$ref` (one with a
    /// URI part) is reported as resolvable, because this crate does not fetch.
    pub fn contains_ref(&self, reference: &str) -> bool {
        let Some(pointer) = reference.strip_prefix("#/") else {
            // Either an external document (`schemas.json#/Foo`), which this
            // crate does not fetch, or the whole-document reference `#`.
            // Neither is ours to refute.
            return true;
        };
        let mut tokens = pointer.split('/');
        if tokens.next() != Some("components") {
            // A pointer into `paths` or `webhooks`. Legal, but resolving it
            // means walking the document, and nothing Moso emits produces one.
            return true;
        }
        let (Some(bucket), Some(name)) = (tokens.next(), tokens.next()) else {
            return false;
        };
        // Any remaining tokens address something *inside* the component; if
        // the component itself exists, the reference is as resolved as this
        // check gets.
        let name = unescape_pointer_token(name);
        match bucket {
            "schemas" => self.schemas.contains_key(&name),
            "responses" => self.responses.contains_key(&name),
            "parameters" => self.parameters.contains_key(&name),
            "examples" => self.examples.contains_key(&name),
            "requestBodies" => self.request_bodies.contains_key(&name),
            "headers" => self.headers.contains_key(&name),
            "securitySchemes" => self.security_schemes.contains_key(&name),
            "links" => self.links.contains_key(&name),
            "pathItems" => self.path_items.contains_key(&name),
            _ => false,
        }
    }

    /// Sort every bucket lexicographically by key.
    pub fn sort(&mut self) {
        self.schemas.sort_unstable_keys();
        self.responses.sort_unstable_keys();
        self.parameters.sort_unstable_keys();
        self.examples.sort_unstable_keys();
        self.request_bodies.sort_unstable_keys();
        self.headers.sort_unstable_keys();
        self.security_schemes.sort_unstable_keys();
        self.links.sort_unstable_keys();
        self.path_items.sort_unstable_keys();
    }
}

/// Undo RFC 6901 token escaping: `~1` is `/` and `~0` is `~`.
fn unescape_pointer_token(token: &str) -> String {
    if !token.contains('~') {
        return token.to_owned();
    }
    token.replace("~1", "/").replace("~0", "~")
}

/// Something that makes a document invalid, detected while assembling it.
///
/// These are **boot** errors: an application that cannot describe itself
/// coherently does not start. Each variant carries enough context to name the
/// offending code, which is the point.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum DocumentError {
    /// Two operations claimed the same `operationId`.
    DuplicateOperationId {
        /// The contested identifier.
        operation_id: String,
        /// `METHOD /path` of the operation that claimed it first.
        first: String,
        /// `METHOD /path` of the operation that claimed it second.
        second: String,
    },
    /// The same method and path were registered twice.
    RouteConflict {
        /// The HTTP method.
        method: HttpMethod,
        /// The templated path.
        path: String,
        /// Source location of the first registration, if known.
        first: Option<String>,
        /// Source location of the second registration, if known.
        second: Option<String>,
    },
    /// Two Rust types produced the same `schema_name()`.
    SchemaCollision {
        /// The contested schema name.
        name: String,
        /// The first type that claimed it.
        first: String,
        /// The second type that claimed it.
        second: String,
    },
    /// A `$ref` points at a component that is not defined.
    DanglingRef {
        /// The unresolvable reference.
        reference: String,
        /// Where in the document it appeared.
        location: String,
    },
    /// A path template and its `in: path` parameters disagree.
    PathParameterMismatch {
        /// The templated path.
        path: String,
        /// Placeholders in the path with no matching parameter.
        missing: Vec<String>,
        /// Declared path parameters with no matching placeholder.
        extra: Vec<String>,
    },
    /// A response was registered under a key that is not a status code,
    /// a `NXX` range, or `default`.
    InvalidStatusKey {
        /// The offending key.
        key: String,
    },
    /// A [`SecurityRequirement`] names a scheme the document does not declare.
    UnknownSecurityScheme {
        /// The named scheme.
        scheme: String,
        /// Where the requirement appeared.
        location: String,
    },
}

impl core::fmt::Display for DocumentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DocumentError::DuplicateOperationId {
                operation_id,
                first,
                second,
            } => write!(
                f,
                "duplicate operationId `{operation_id}`: claimed by `{first}` and by `{second}` \
                 — rename one with `#[endpoint(operation_id = \"...\")]`"
            ),
            DocumentError::RouteConflict {
                method,
                path,
                first,
                second,
            } => {
                write!(f, "`{method} {path}` is registered twice")?;
                match (first.as_deref(), second.as_deref()) {
                    (Some(first), Some(second)) => {
                        write!(f, " — first at {first}, then at {second}")
                    }
                    (Some(first), None) => write!(f, " — the first registration is at {first}"),
                    (None, Some(second)) => write!(f, " — the second registration is at {second}"),
                    (None, None) => Ok(()),
                }
            }
            DocumentError::SchemaCollision {
                name,
                first,
                second,
            } => write!(
                f,
                "two types produce the schema name `{name}`: `{first}` and `{second}` \
                 — rename one with `#[schema(rename = \"...\")]`"
            ),
            DocumentError::DanglingRef {
                reference,
                location,
            } => write!(
                f,
                "`{reference}` at {location} does not resolve to a component this document defines"
            ),
            DocumentError::PathParameterMismatch {
                path,
                missing,
                extra,
            } => {
                write!(f, "path `{path}` and its parameters disagree")?;
                if !missing.is_empty() {
                    write!(f, " — no `in: path` parameter for {}", quoted_list(missing))?;
                }
                if !extra.is_empty() {
                    write!(
                        f,
                        " — no `{{placeholder}}` in the path template for {}",
                        quoted_list(extra)
                    )?;
                }
                Ok(())
            }
            DocumentError::InvalidStatusKey { key } => write!(
                f,
                "`{key}` is not a valid response key — expected a status code, \
                 an `NXX` range, or `default`"
            ),
            DocumentError::UnknownSecurityScheme { scheme, location } => write!(
                f,
                "the security requirement at {location} names `{scheme}`, which is not declared \
                 — add it with `.security_scheme(\"{scheme}\", ...)` in the `openapi` block"
            ),
        }
    }
}

/// `` `a`, `b`, `c` `` — the rendering every list in a diagnostic uses.
fn quoted_list<S: AsRef<str>>(items: &[S]) -> String {
    let mut out = String::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push('`');
        out.push_str(item.as_ref());
        out.push('`');
    }
    out
}

impl core::error::Error for DocumentError {}

// ---------------------------------------------------------------------------
// Self-consistency checks
// ---------------------------------------------------------------------------

impl Document {
    /// Every operation in the document, paired with a human-readable location.
    ///
    /// Webhooks share the `operationId` namespace with paths, so both are
    /// visited by the checks below.
    fn located_operations(&self) -> impl Iterator<Item = (String, &Operation)> {
        let paths = self.paths.iter().flat_map(|(path, item)| {
            item.operations()
                .map(move |(method, op)| (format!("{method} {path}"), op))
        });
        let webhooks = self.webhooks.iter().flat_map(|(name, item)| {
            item.operations()
                .map(move |(method, op)| (format!("webhook `{name}` {method}"), op))
        });
        paths.chain(webhooks)
    }

    fn check_operation_ids(&self, problems: &mut Vec<String>) {
        let mut claimed: IndexMap<&str, String> = IndexMap::new();
        for (location, operation) in self.located_operations() {
            let Some(id) = operation.operation_id.as_deref() else {
                continue;
            };
            match claimed.get(id) {
                Some(first) => problems.push(format!(
                    "duplicate operationId `{id}`: claimed by `{first}` and by `{location}`"
                )),
                None => {
                    claimed.insert(id, location);
                }
            }
        }
    }

    fn check_references(&self, problems: &mut Vec<String>) {
        let mut found: Vec<(String, String)> = Vec::new();

        for (path, item) in &self.paths {
            visit_path_item(item, &format!("paths.{path}"), &mut found);
        }
        for (name, item) in &self.webhooks {
            visit_path_item(item, &format!("webhooks.{name}"), &mut found);
        }

        let components = &self.components;
        for (name, schema) in &components.schemas {
            visit_schema(schema, &format!("components.schemas.{name}"), &mut found);
        }
        for (name, response) in &components.responses {
            visit_response(
                response,
                &format!("components.responses.{name}"),
                &mut found,
            );
        }
        for (name, parameter) in &components.parameters {
            visit_parameter(
                parameter,
                &format!("components.parameters.{name}"),
                &mut found,
            );
        }
        for (name, example) in &components.examples {
            visit_example(example, &format!("components.examples.{name}"), &mut found);
        }
        for (name, body) in &components.request_bodies {
            visit_request_body(
                body,
                &format!("components.requestBodies.{name}"),
                &mut found,
            );
        }
        for (name, header) in &components.headers {
            visit_header(header, &format!("components.headers.{name}"), &mut found);
        }
        for (name, link) in &components.links {
            visit_link(link, &format!("components.links.{name}"), &mut found);
        }
        for (name, item) in &components.path_items {
            visit_path_item(item, &format!("components.pathItems.{name}"), &mut found);
        }

        for (reference, location) in found {
            if !components.contains_ref(&reference) {
                problems.push(format!(
                    "`{reference}` at {location} does not resolve to a component this document \
                     defines"
                ));
            }
        }
    }

    fn check_path_parameters(&self, problems: &mut Vec<String>) {
        for (path, item) in &self.paths {
            let placeholders = path_placeholders(path);
            for (method, operation) in item.operations() {
                let mut declared: Vec<&str> = Vec::new();
                for parameter in item.parameters.iter().chain(operation.parameters.iter()) {
                    if parameter.location != ParameterLocation::Path {
                        continue;
                    }
                    if !declared.contains(&parameter.name.as_str()) {
                        declared.push(&parameter.name);
                    }
                    if !parameter.required {
                        problems.push(format!(
                            "`{method} {path}`: the path parameter `{}` is declared optional, but \
                             a path parameter is always required",
                            parameter.name
                        ));
                    }
                }

                let missing: Vec<&str> = placeholders
                    .iter()
                    .map(String::as_str)
                    .filter(|name| !declared.contains(name))
                    .collect();
                let extra: Vec<&str> = declared
                    .iter()
                    .copied()
                    .filter(|name| !placeholders.iter().any(|p| p == name))
                    .collect();

                if !missing.is_empty() {
                    problems.push(format!(
                        "`{method} {path}`: the path template declares {} with no matching \
                         `in: path` parameter",
                        quoted_list(&missing)
                    ));
                }
                if !extra.is_empty() {
                    problems.push(format!(
                        "`{method} {path}`: the `in: path` parameter {} has no matching \
                         `{{placeholder}}` in the path template",
                        quoted_list(&extra)
                    ));
                }
            }
        }
    }

    fn check_responses(&self, problems: &mut Vec<String>) {
        for (location, operation) in self.located_operations() {
            for (key, response) in &operation.responses {
                if !is_valid_response_key(key) {
                    problems.push(format!(
                        "`{location}`: `{key}` is not a valid response key — expected a status \
                         code, an `NXX` range, or `default`"
                    ));
                }
                if response.reference.is_none() && response.description.is_none() {
                    problems.push(format!(
                        "`{location}`: the `{key}` response is neither a `$ref` nor carries the \
                         required `description`"
                    ));
                }
            }
        }
        for (name, response) in &self.components.responses {
            if response.reference.is_none() && response.description.is_none() {
                problems.push(format!(
                    "`components.responses.{name}` is neither a `$ref` nor carries the required \
                     `description`"
                ));
            }
        }
    }

    fn check_security(&self, problems: &mut Vec<String>) {
        self.check_requirements(&self.security, "the document-level `security`", problems);
        for (location, operation) in self.located_operations() {
            let Some(requirements) = operation.security.as_deref() else {
                continue;
            };
            self.check_requirements(requirements, &format!("`{location}`"), problems);
        }

        for (name, scheme) in &self.components.security_schemes {
            let SecurityScheme::OAuth2 { flows, .. } = scheme else {
                continue;
            };
            let declarations: [(&str, Option<&OAuthFlow>, bool, bool); 4] = [
                ("implicit", flows.implicit.as_ref(), true, false),
                ("password", flows.password.as_ref(), false, true),
                (
                    "clientCredentials",
                    flows.client_credentials.as_ref(),
                    false,
                    true,
                ),
                (
                    "authorizationCode",
                    flows.authorization_code.as_ref(),
                    true,
                    true,
                ),
            ];
            for (flow_name, flow, needs_authorization, needs_token) in declarations {
                let Some(flow) = flow else { continue };
                if needs_authorization && flow.authorization_url.is_none() {
                    problems.push(format!(
                        "`components.securitySchemes.{name}`: the `{flow_name}` flow requires an \
                         `authorizationUrl`"
                    ));
                }
                if needs_token && flow.token_url.is_none() {
                    problems.push(format!(
                        "`components.securitySchemes.{name}`: the `{flow_name}` flow requires a \
                         `tokenUrl`"
                    ));
                }
            }
        }
    }

    fn check_requirements(
        &self,
        requirements: &[SecurityRequirement],
        location: &str,
        problems: &mut Vec<String>,
    ) {
        for requirement in requirements {
            for (name, scopes) in requirement.schemes() {
                let Some(scheme) = self.components.security_schemes.get(name) else {
                    problems.push(format!(
                        "{location}: the security requirement names `{name}`, which is not \
                         declared in `components.securitySchemes`"
                    ));
                    continue;
                };
                if !scopes.is_empty() && !scheme.accepts_scopes() {
                    problems.push(format!(
                        "{location}: the security requirement gives scopes to `{name}`, which is \
                         a `{}` scheme and takes none",
                        scheme.kind()
                    ));
                    continue;
                }
                let known = scheme.known_scopes();
                if known.is_empty() {
                    continue;
                }
                for scope in scopes {
                    if !known.contains(&scope.as_str()) {
                        problems.push(format!(
                            "{location}: the security requirement asks `{name}` for the scope \
                             `{scope}`, which none of its flows declares"
                        ));
                    }
                }
            }
        }
    }

    fn check_schemas(&self, problems: &mut Vec<String>) {
        for (name, schema) in &self.components.schemas {
            walk_schema(
                schema,
                &format!("components.schemas.{name}"),
                &mut |node, location| check_required_names(node, location, problems),
            );
        }
    }

    fn check_servers(&self, problems: &mut Vec<String>) {
        for server in &self.servers {
            for (name, variable) in &server.variables {
                if variable.enumeration.is_empty() {
                    continue;
                }
                if !variable.enumeration.contains(&variable.default_value) {
                    problems.push(format!(
                        "server `{}`: the variable `{name}` has an `enum` that does not contain \
                         its `default` (`{}`)",
                        server.url, variable.default_value
                    ));
                }
            }
        }
    }
}

/// Flag a schema whose `required` array disagrees with its `properties`.
fn check_required_names(node: &SchemaNode, location: &str, problems: &mut Vec<String>) {
    for (index, name) in node.required.iter().enumerate() {
        if node.required[..index].contains(name) {
            problems.push(format!(
                "{location}: `required` names `{name}` twice; the array must have unique items"
            ));
        }
    }

    // A schema that composes (`allOf`/`anyOf`/`oneOf`) or defers (`$ref`) can
    // legitimately require a property it does not itself declare.
    let composes = !node.all_of.is_empty()
        || !node.any_of.is_empty()
        || !node.one_of.is_empty()
        || node.reference.is_some();
    if composes || node.properties.is_empty() {
        return;
    }
    for name in &node.required {
        if !node.properties.contains_key(name) {
            problems.push(format!(
                "{location}: `required` names `{name}`, which is not one of its properties"
            ));
        }
    }
}

/// The placeholder names in a templated path, in order and without duplicates.
///
/// A catch-all placeholder is spelled `{*rest}` in Moso's router (and in Axum
/// 0.8); the parameter it binds is named `rest`, so the leading `*` is dropped.
fn path_placeholders(path: &str) -> Vec<String> {
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

/// Whether `key` is a status code, an `NXX` range, or `default`.
fn is_valid_response_key(key: &str) -> bool {
    if key == "default" {
        return true;
    }
    let bytes = key.as_bytes();
    if bytes.len() != 3 {
        return false;
    }
    if let Ok(status) = key.parse::<u16>() {
        return (100..=599).contains(&status);
    }
    (b'1'..=b'5').contains(&bytes[0])
        && bytes[1].eq_ignore_ascii_case(&b'X')
        && bytes[2].eq_ignore_ascii_case(&b'X')
}

/// The sort key that puts status codes first, then `NXX` ranges, then
/// `default`, then anything unrecognised.
///
/// Unrecognised keys tie, and [`IndexMap::sort_by`] is stable, so they keep
/// their relative order rather than being shuffled.
pub(crate) fn response_key_rank(key: &str) -> (u8, u32) {
    if key == "default" {
        return (2, 0);
    }
    if let Ok(status) = key.parse::<u32>() {
        return (0, status);
    }
    let bytes = key.as_bytes();
    if bytes.len() == 3
        && bytes[0].is_ascii_digit()
        && bytes[1].eq_ignore_ascii_case(&b'X')
        && bytes[2].eq_ignore_ascii_case(&b'X')
    {
        return (1, u32::from(bytes[0] - b'0') * 100);
    }
    (3, 0)
}

// ---------------------------------------------------------------------------
// Reference collection
// ---------------------------------------------------------------------------

/// Visit every schema in the tree rooted at `node`, deepest last.
///
/// `$ref` targets are *not* followed, so this always terminates: the model is a
/// tree and every cycle in JSON Schema goes through a reference.
fn walk_schema(node: &SchemaNode, location: &str, visit: &mut impl FnMut(&SchemaNode, &str)) {
    visit(node, location);
    for (name, property) in &node.properties {
        walk_schema(property, &format!("{location}.properties.{name}"), visit);
    }
    if let Some(items) = &node.items {
        walk_schema(items, &format!("{location}.items"), visit);
    }
    for (index, item) in node.prefix_items.iter().enumerate() {
        walk_schema(item, &format!("{location}.prefixItems[{index}]"), visit);
    }
    for (index, variant) in node.one_of.iter().enumerate() {
        walk_schema(variant, &format!("{location}.oneOf[{index}]"), visit);
    }
    for (index, variant) in node.any_of.iter().enumerate() {
        walk_schema(variant, &format!("{location}.anyOf[{index}]"), visit);
    }
    for (index, part) in node.all_of.iter().enumerate() {
        walk_schema(part, &format!("{location}.allOf[{index}]"), visit);
    }
    if let Some(not) = &node.not {
        walk_schema(not, &format!("{location}.not"), visit);
    }
    if let Some(AdditionalProperties::Schema(schema)) = &node.additional_properties {
        walk_schema(schema, &format!("{location}.additionalProperties"), visit);
    }
    for (name, definition) in &node.defs {
        walk_schema(definition, &format!("{location}.$defs.{name}"), visit);
    }
}

type Refs = Vec<(String, String)>;

fn record(reference: &Option<String>, location: &str, out: &mut Refs) {
    if let Some(reference) = reference {
        out.push((reference.clone(), location.to_owned()));
    }
}

fn visit_schema(node: &SchemaNode, location: &str, out: &mut Refs) {
    walk_schema(node, location, &mut |node, location| {
        record(&node.reference, location, out);
    });
}

fn visit_path_item(item: &PathItem, location: &str, out: &mut Refs) {
    record(&item.reference, location, out);
    for (index, parameter) in item.parameters.iter().enumerate() {
        visit_parameter(parameter, &format!("{location}.parameters[{index}]"), out);
    }
    for (method, operation) in item.operations() {
        visit_operation(operation, &format!("{location}.{}", method.as_str()), out);
    }
}

fn visit_operation(operation: &Operation, location: &str, out: &mut Refs) {
    for (index, parameter) in operation.parameters.iter().enumerate() {
        visit_parameter(parameter, &format!("{location}.parameters[{index}]"), out);
    }
    if let Some(body) = &operation.request_body {
        visit_request_body(body, &format!("{location}.requestBody"), out);
    }
    for (key, response) in &operation.responses {
        visit_response(response, &format!("{location}.responses.{key}"), out);
    }
    for (name, callback) in &operation.callbacks {
        for (expression, item) in callback {
            visit_path_item(
                item,
                &format!("{location}.callbacks.{name}.{expression}"),
                out,
            );
        }
    }
}

fn visit_parameter(parameter: &Parameter, location: &str, out: &mut Refs) {
    if let Some(schema) = &parameter.schema {
        visit_schema(schema, &format!("{location}.schema"), out);
    }
    for (name, example) in &parameter.examples {
        visit_example(example, &format!("{location}.examples.{name}"), out);
    }
    for (content_type, media) in &parameter.content {
        visit_media_type(media, &format!("{location}.content.{content_type}"), out);
    }
}

fn visit_request_body(body: &RequestBody, location: &str, out: &mut Refs) {
    record(&body.reference, location, out);
    for (content_type, media) in &body.content {
        visit_media_type(media, &format!("{location}.content.{content_type}"), out);
    }
}

fn visit_response(response: &Response, location: &str, out: &mut Refs) {
    record(&response.reference, location, out);
    for (name, header) in &response.headers {
        visit_header(header, &format!("{location}.headers.{name}"), out);
    }
    for (content_type, media) in &response.content {
        visit_media_type(media, &format!("{location}.content.{content_type}"), out);
    }
    for (name, link) in &response.links {
        visit_link(link, &format!("{location}.links.{name}"), out);
    }
}

fn visit_media_type(media: &MediaType, location: &str, out: &mut Refs) {
    if let Some(schema) = &media.schema {
        visit_schema(schema, &format!("{location}.schema"), out);
    }
    for (name, example) in &media.examples {
        visit_example(example, &format!("{location}.examples.{name}"), out);
    }
    for (property, encoding) in &media.encoding {
        for (name, header) in &encoding.headers {
            visit_header(
                header,
                &format!("{location}.encoding.{property}.headers.{name}"),
                out,
            );
        }
    }
}

fn visit_header(header: &Header, location: &str, out: &mut Refs) {
    record(&header.reference, location, out);
    if let Some(schema) = &header.schema {
        visit_schema(schema, &format!("{location}.schema"), out);
    }
    for (name, example) in &header.examples {
        visit_example(example, &format!("{location}.examples.{name}"), out);
    }
    for (content_type, media) in &header.content {
        visit_media_type(media, &format!("{location}.content.{content_type}"), out);
    }
}

fn visit_example(example: &Example, location: &str, out: &mut Refs) {
    record(&example.reference, location, out);
}

fn visit_link(link: &Link, location: &str, out: &mut Refs) {
    record(&link.reference, location, out);
}

// ---------------------------------------------------------------------------
// YAML emission
// ---------------------------------------------------------------------------

/// A deterministic YAML 1.2 emitter for the JSON value tree.
///
/// YAML is a superset of JSON, so the only thing an emitter has to decide is
/// how much of the pretty block syntax it is willing to use. This one uses
/// block mappings and block sequences, plain scalars where they are
/// unambiguous, and JSON's own double-quoted form everywhere else — which is
/// exactly YAML's double-quoted style, escapes included.
///
/// A YAML dependency would be paid for by every Moso application, and the
/// subset needed to represent a JSON document is this file.
mod yaml {
    use serde_json::{Map, Value};

    /// Two spaces per level, as everything in the OpenAPI ecosystem uses.
    const INDENT: &str = "  ";

    pub(super) fn block_map(map: &Map<String, Value>, indent: usize, out: &mut String) {
        for (key, value) in map {
            push_indent(out, indent);
            out.push_str(&scalar_string(key));
            out.push(':');
            after_marker(value, indent, out);
        }
    }

    fn block_seq(items: &[Value], indent: usize, out: &mut String) {
        for item in items {
            push_indent(out, indent);
            out.push('-');
            match item {
                Value::Object(map) if !map.is_empty() => {
                    // The first key rides on the `- ` marker, which is exactly
                    // two columns wide, so the rest indent one level deeper.
                    for (index, (key, value)) in map.iter().enumerate() {
                        if index == 0 {
                            out.push(' ');
                        } else {
                            push_indent(out, indent + 1);
                        }
                        out.push_str(&scalar_string(key));
                        out.push(':');
                        after_marker(value, indent + 1, out);
                    }
                }
                Value::Array(nested) if !nested.is_empty() => {
                    out.push('\n');
                    block_seq(nested, indent + 1, out);
                }
                other => {
                    out.push(' ');
                    out.push_str(&scalar(other));
                    out.push('\n');
                }
            }
        }
    }

    /// Write the value that follows a `key:` or `-` already emitted at
    /// `indent`, always terminating the line.
    fn after_marker(value: &Value, indent: usize, out: &mut String) {
        match value {
            Value::Object(map) if !map.is_empty() => {
                out.push('\n');
                block_map(map, indent + 1, out);
            }
            Value::Array(items) if !items.is_empty() => {
                out.push('\n');
                block_seq(items, indent + 1, out);
            }
            other => {
                out.push(' ');
                out.push_str(&scalar(other));
                out.push('\n');
            }
        }
    }

    pub(super) fn scalar(value: &Value) -> String {
        match value {
            Value::Null => "null".to_owned(),
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            Value::String(value) => scalar_string(value),
            // Only ever reached for the empty cases; non-empty containers are
            // handled by the block writers above.
            Value::Array(_) => "[]".to_owned(),
            Value::Object(_) => "{}".to_owned(),
        }
    }

    fn scalar_string(value: &str) -> String {
        if is_plain_safe(value) {
            value.to_owned()
        } else {
            // `serde_json` emits exactly YAML's double-quoted style.
            Value::String(value.to_owned()).to_string()
        }
    }

    /// Whether `value` can be written as a plain (unquoted) YAML scalar in
    /// block context without changing its meaning.
    fn is_plain_safe(value: &str) -> bool {
        if value.is_empty() {
            return false;
        }
        // Anything a YAML 1.1 reader would resolve to a non-string.
        const RESERVED: [&str; 12] = [
            "true", "false", "null", "~", "yes", "no", "on", "off", "y", "n", "nan", "inf",
        ];
        if RESERVED.iter().any(|word| value.eq_ignore_ascii_case(word)) {
            return false;
        }
        if value.parse::<f64>().is_ok() {
            return false;
        }

        let bytes = value.as_bytes();
        // Leading indicator characters change the node type.
        if b"-?:,[]{}#&*!|>'\"%@`".contains(&bytes[0]) {
            return false;
        }
        if value.starts_with(' ') || value.ends_with(' ') || value.ends_with(':') {
            return false;
        }
        for (index, ch) in value.char_indices() {
            if ch.is_control() {
                return false;
            }
            // `: ` ends a plain scalar and ` #` starts a comment.
            if ch == ':' && value[index + 1..].starts_with(' ') {
                return false;
            }
            if ch == '#' && index > 0 && bytes[index - 1] == b' ' {
                return false;
            }
        }
        true
    }

    fn push_indent(out: &mut String, indent: usize) {
        for _ in 0..indent {
            out.push_str(INDENT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::MediaType;
    use crate::security::{OAuthFlow, OAuthFlows};
    use moso_schema::json_schema::JsonType;
    use serde_json::json;

    fn user_schema() -> SchemaNode {
        let mut node = SchemaNode::of_type(JsonType::Object);
        node.properties
            .insert("id".to_owned(), SchemaNode::of_type(JsonType::String));
        node.properties.insert(
            "email".to_owned(),
            SchemaNode::of_type(JsonType::String).with_format("email"),
        );
        node.required = vec!["id".to_owned(), "email".to_owned()];
        node
    }

    /// `GET /users/{id}` returning a `User`, with everything wired up
    /// correctly. Every `validate_self` test starts from this and breaks one
    /// thing.
    fn sample() -> Document {
        let mut document = Document::new(Info::new("Shop API", "1.4.0"));
        document
            .servers
            .push(Server::new("https://api.shop.example", "production"));
        document
            .components
            .schemas
            .insert("User".to_owned(), user_schema());
        document
            .components
            .security_schemes
            .insert("session".to_owned(), SecurityScheme::cookie("sid"));
        document
            .security
            .push(SecurityRequirement::scheme("session"));

        let mut response = Response::new("the user");
        response.content.insert(
            "application/json".to_owned(),
            MediaType::new(SchemaNode::reference("#/components/schemas/User")),
        );

        let mut operation = Operation {
            summary: Some("Fetch one user".to_owned()),
            operation_id: Some("get_user".to_owned()),
            ..Operation::default()
        };
        operation
            .parameters
            .push(Parameter::new("id", ParameterLocation::Path));
        operation.responses.insert("200".to_owned(), response);

        let mut item = PathItem::default();
        item.set_operation(HttpMethod::Get, operation);
        document.paths.insert("/users/{id}".to_owned(), item);
        document
    }

    // ── round-trip and golden output ────────────────────────────────────

    #[test]
    fn documents_round_trip_through_json() {
        let document = sample();
        let text = document.to_json_pretty().unwrap();
        let back = Document::from_json(&text).unwrap();
        assert_eq!(document, back);
    }

    #[test]
    fn unknown_and_extension_members_round_trip_verbatim() {
        let text = r#"{
          "openapi": "3.1.1",
          "info": { "title": "T", "version": "1", "x-owner": "team-api" },
          "x-audience": "internal",
          "paths": { "/p": { "get": { "responses": {}, "x-flag": 1 } } }
        }"#;
        let document = Document::from_json(text).unwrap();
        assert_eq!(document.extensions["x-audience"], json!("internal"));
        assert_eq!(document.info.extensions["x-owner"], json!("team-api"));
        assert_eq!(
            document.paths["/p"].get.as_ref().unwrap().extensions["x-flag"],
            json!(1)
        );
        let again = Document::from_json(&document.to_json().unwrap()).unwrap();
        assert_eq!(document, again);
    }

    #[test]
    fn pretty_json_matches_the_golden_document() {
        let expected = r##"{
  "openapi": "3.1.1",
  "info": {
    "title": "Shop API",
    "version": "1.4.0"
  },
  "servers": [
    {
      "url": "https://api.shop.example",
      "description": "production"
    }
  ],
  "paths": {
    "/users/{id}": {
      "get": {
        "summary": "Fetch one user",
        "operationId": "get_user",
        "parameters": [
          {
            "name": "id",
            "in": "path",
            "required": true
          }
        ],
        "responses": {
          "200": {
            "description": "the user",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/User"
                }
              }
            }
          }
        }
      }
    }
  },
  "components": {
    "schemas": {
      "User": {
        "type": "object",
        "properties": {
          "id": {
            "type": "string"
          },
          "email": {
            "type": "string",
            "format": "email"
          }
        },
        "required": [
          "id",
          "email"
        ]
      }
    },
    "securitySchemes": {
      "session": {
        "type": "apiKey",
        "name": "sid",
        "in": "cookie"
      }
    }
  },
  "security": [
    {
      "session": []
    }
  ]
}
"##;
        assert_eq!(sample().to_json_pretty().unwrap(), expected);
    }

    #[test]
    fn pretty_json_ends_in_exactly_one_newline() {
        let text = sample().to_json_pretty().unwrap();
        assert!(text.ends_with("}\n"));
        assert!(!text.ends_with("\n\n"));
    }

    // ── YAML ────────────────────────────────────────────────────────────

    #[test]
    fn yaml_matches_the_golden_document() {
        let expected = r##"openapi: 3.1.1
info:
  title: Shop API
  version: 1.4.0
servers:
  - url: https://api.shop.example
    description: production
paths:
  /users/{id}:
    get:
      summary: Fetch one user
      operationId: get_user
      parameters:
        - name: id
          in: path
          required: true
      responses:
        "200":
          description: the user
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/User"
components:
  schemas:
    User:
      type: object
      properties:
        id:
          type: string
        email:
          type: string
          format: email
      required:
        - id
        - email
  securitySchemes:
    session:
      type: apiKey
      name: sid
      in: cookie
security:
  - session: []
"##;
        assert_eq!(sample().to_yaml().unwrap(), expected);
    }

    #[test]
    fn yaml_quotes_scalars_that_would_change_meaning() {
        let mut document = Document::new(Info::new("Ops", "1.0"));
        document.info.summary = Some("yes".to_owned());
        document.info.description = Some("line one\nline two".to_owned());
        document.info.terms_of_service = Some("# not a comment".to_owned());
        document.extensions.insert("x-empty".to_owned(), json!(""));
        document
            .extensions
            .insert("x-colon".to_owned(), json!("a: b"));
        document
            .extensions
            .insert("x-plain".to_owned(), json!("a:b"));

        let yaml = document.to_yaml().unwrap();
        assert!(yaml.contains("version: \"1.0\""), "{yaml}");
        assert!(yaml.contains("summary: \"yes\""), "{yaml}");
        assert!(
            yaml.contains("description: \"line one\\nline two\""),
            "{yaml}"
        );
        assert!(
            yaml.contains("termsOfService: \"# not a comment\""),
            "{yaml}"
        );
        assert!(yaml.contains("x-empty: \"\""), "{yaml}");
        assert!(yaml.contains("x-colon: \"a: b\""), "{yaml}");
        assert!(yaml.contains("x-plain: a:b"), "{yaml}");
    }

    #[test]
    fn yaml_renders_empty_containers_inline() {
        let mut document = Document::new(Info::new("Ops", "1"));
        document.extensions.insert("x-list".to_owned(), json!([]));
        document.extensions.insert("x-map".to_owned(), json!({}));
        document.extensions.insert("x-null".to_owned(), Value::Null);
        let yaml = document.to_yaml().unwrap();
        assert!(yaml.contains("x-list: []"), "{yaml}");
        assert!(yaml.contains("x-map: {}"), "{yaml}");
        assert!(yaml.contains("x-null: null"), "{yaml}");
    }

    #[test]
    fn yaml_nests_sequences_of_sequences() {
        let mut document = Document::new(Info::new("Ops", "1"));
        document
            .extensions
            .insert("x-matrix".to_owned(), json!([["a", "b"], ["c"]]));
        let yaml = document.to_yaml().unwrap();
        assert!(
            yaml.contains("x-matrix:\n  -\n    - a\n    - b\n  -\n    - c\n"),
            "{yaml}"
        );
    }

    // ── ETag ────────────────────────────────────────────────────────────

    #[test]
    fn etags_are_weak_quoted_and_content_addressed() {
        let etag = etag_for(b"{\"openapi\":\"3.1.1\"}");
        assert!(etag.starts_with("W/\""), "{etag}");
        assert!(etag.ends_with('"'), "{etag}");
        assert_eq!(etag, etag_for(b"{\"openapi\":\"3.1.1\"}"));
        assert_ne!(etag, etag_for(b"{\"openapi\":\"3.1.0\"}"));
        assert_ne!(etag_for(b""), etag_for(b"a"));
    }

    // ── ordering ────────────────────────────────────────────────────────

    #[test]
    fn sort_for_output_orders_paths_components_and_responses() {
        let mut document = Document::new(Info::new("T", "1"));
        for path in ["/z", "/a", "/m"] {
            let mut item = PathItem::default();
            let mut operation = Operation::default();
            for key in ["default", "5XX", "404", "200", "201"] {
                operation
                    .responses
                    .insert(key.to_owned(), Response::new("x"));
            }
            item.set_operation(HttpMethod::Get, operation);
            document.paths.insert(path.to_owned(), item);
        }
        document
            .components
            .schemas
            .insert("Zeta".to_owned(), SchemaNode::any());
        document
            .components
            .schemas
            .insert("Alpha".to_owned(), SchemaNode::any());

        document.sort_for_output();

        assert_eq!(
            document.paths.keys().collect::<Vec<_>>(),
            ["/a", "/m", "/z"]
        );
        assert_eq!(
            document.components.schemas.keys().collect::<Vec<_>>(),
            ["Alpha", "Zeta"]
        );
        let responses = &document.paths["/a"].get.as_ref().unwrap().responses;
        assert_eq!(
            responses.keys().collect::<Vec<_>>(),
            ["200", "201", "404", "5XX", "default"]
        );
    }

    #[test]
    fn sorting_is_idempotent_and_makes_registration_order_irrelevant() {
        let mut first = Document::new(Info::new("T", "1"));
        let mut second = Document::new(Info::new("T", "1"));
        for path in ["/a", "/b", "/c"] {
            first.paths.insert(path.to_owned(), PathItem::default());
        }
        for path in ["/c", "/a", "/b"] {
            second.paths.insert(path.to_owned(), PathItem::default());
        }
        first.sort_for_output();
        second.sort_for_output();
        assert_eq!(first.to_json().unwrap(), second.to_json().unwrap());
        second.sort_for_output();
        assert_eq!(first.to_json().unwrap(), second.to_json().unwrap());
    }

    #[test]
    fn response_keys_rank_numerically_then_by_range_then_default() {
        assert!(response_key_rank("200") < response_key_rank("404"));
        assert!(response_key_rank("599") < response_key_rank("2XX"));
        assert!(response_key_rank("2XX") < response_key_rank("5xx"));
        assert!(response_key_rank("5XX") < response_key_rank("default"));
        assert!(response_key_rank("default") < response_key_rank("nonsense"));
    }

    // ── prefix filtering ────────────────────────────────────────────────

    #[test]
    fn filter_prefix_keeps_and_strips_on_segment_boundaries() {
        let mut document = Document::new(Info::new("T", "1"));
        for path in ["/api/v1/users", "/api/v1", "/api/v11/users", "/health"] {
            document.paths.insert(path.to_owned(), PathItem::default());
        }
        document.filter_prefix("/api/v1");
        assert_eq!(document.paths.keys().collect::<Vec<_>>(), ["/users", "/"]);
    }

    #[test]
    fn filter_prefix_ignores_a_trailing_slash_and_an_empty_prefix() {
        let mut document = Document::new(Info::new("T", "1"));
        document
            .paths
            .insert("/api/users".to_owned(), PathItem::default());
        document.filter_prefix("/api/");
        assert_eq!(document.paths.keys().collect::<Vec<_>>(), ["/users"]);

        let before = document.clone();
        document.filter_prefix("");
        assert_eq!(document, before);
    }

    // ── component reference resolution ──────────────────────────────────

    #[test]
    fn contains_ref_walks_every_bucket() {
        let mut components = Components::default();
        components
            .schemas
            .insert("User".to_owned(), SchemaNode::any());
        components
            .responses
            .insert("NotFound".to_owned(), Response::new("gone"));

        assert!(components.contains_ref("#/components/schemas/User"));
        assert!(components.contains_ref("#/components/responses/NotFound"));
        assert!(!components.contains_ref("#/components/schemas/Missing"));
        assert!(!components.contains_ref("#/components/parameters/User"));
        assert!(!components.contains_ref("#/components/nonsense/User"));
        assert!(
            components.contains_ref("#/components/schemas/User/properties/id"),
            "a deeper pointer resolves as far as the component"
        );
    }

    #[test]
    fn contains_ref_does_not_refute_what_it_cannot_check() {
        let components = Components::default();
        assert!(components.contains_ref("https://example.com/schemas.json#/User"));
        assert!(components.contains_ref("common.yaml#/components/schemas/User"));
        assert!(components.contains_ref("#/paths/~1users/get"));
    }

    // ── validate_self, one test per error class ─────────────────────────

    #[test]
    fn a_consistent_document_validates() {
        assert_eq!(sample().validate_self(), Ok(()));
    }

    fn problems(document: &Document) -> Vec<String> {
        document.validate_self().unwrap_err()
    }

    #[test]
    fn validate_self_catches_duplicate_operation_ids() {
        let mut document = sample();
        let mut operation = Operation {
            operation_id: Some("get_user".to_owned()),
            ..Operation::default()
        };
        operation
            .responses
            .insert("200".to_owned(), Response::new("ok"));
        let mut item = PathItem::default();
        item.set_operation(HttpMethod::Get, operation);
        document.paths.insert("/me".to_owned(), item);

        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("duplicate operationId `get_user`"));
        assert!(problems[0].contains("GET /users/{id}"));
        assert!(problems[0].contains("GET /me"));
    }

    #[test]
    fn validate_self_catches_dangling_refs() {
        let mut document = sample();
        document.components.schemas.shift_remove("User");
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("#/components/schemas/User"));
        assert!(
            problems[0].contains("paths./users/{id}.get.responses.200.content.application/json"),
            "{}",
            problems[0]
        );
    }

    #[test]
    fn validate_self_catches_a_dangling_ref_nested_in_a_component_schema() {
        let mut document = sample();
        document
            .components
            .schemas
            .get_mut("User")
            .unwrap()
            .properties
            .insert(
                "manager".to_owned(),
                SchemaNode::reference("#/components/schemas/Manager"),
            );
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("components.schemas.User.properties.manager"),
            "{}",
            problems[0]
        );
    }

    #[test]
    fn validate_self_catches_a_placeholder_with_no_parameter() {
        let mut document = sample();
        document.paths["/users/{id}"]
            .get
            .as_mut()
            .unwrap()
            .parameters
            .clear();
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("the path template declares `id`"),
            "{problems:#?}"
        );
    }

    #[test]
    fn validate_self_catches_a_parameter_with_no_placeholder() {
        let mut document = sample();
        document.paths["/users/{id}"]
            .get
            .as_mut()
            .unwrap()
            .parameters
            .push(Parameter::new("slug", ParameterLocation::Path));
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("the `in: path` parameter `slug`"),
            "{problems:#?}"
        );
    }

    #[test]
    fn validate_self_accepts_a_parameter_declared_on_the_path_item() {
        let mut document = sample();
        let item = document.paths.get_mut("/users/{id}").unwrap();
        item.get.as_mut().unwrap().parameters.clear();
        item.parameters
            .push(Parameter::new("id", ParameterLocation::Path));
        assert_eq!(document.validate_self(), Ok(()));
    }

    #[test]
    fn validate_self_accepts_a_catch_all_placeholder() {
        let mut document = Document::new(Info::new("T", "1"));
        let mut operation = Operation::default();
        operation
            .parameters
            .push(Parameter::new("rest", ParameterLocation::Path));
        operation
            .responses
            .insert("200".to_owned(), Response::new("ok"));
        let mut item = PathItem::default();
        item.set_operation(HttpMethod::Get, operation);
        document.paths.insert("/files/{*rest}".to_owned(), item);
        assert_eq!(document.validate_self(), Ok(()));
    }

    #[test]
    fn validate_self_catches_an_optional_path_parameter() {
        let mut document = sample();
        document.paths["/users/{id}"]
            .get
            .as_mut()
            .unwrap()
            .parameters[0]
            .required = false;
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("always required"), "{problems:#?}");
    }

    #[test]
    fn validate_self_catches_a_response_with_no_description() {
        let mut document = sample();
        document.paths["/users/{id}"]
            .get
            .as_mut()
            .unwrap()
            .responses["200"]
            .description = None;
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("neither a `$ref` nor carries the required `description`"),
            "{problems:#?}"
        );
    }

    #[test]
    fn validate_self_accepts_a_response_that_is_a_reference() {
        let mut document = sample();
        document
            .components
            .responses
            .insert("User".to_owned(), Response::new("the user"));
        document.paths["/users/{id}"]
            .get
            .as_mut()
            .unwrap()
            .responses
            .insert("404".to_owned(), Response::reference("User"));
        assert_eq!(document.validate_self(), Ok(()));
    }

    #[test]
    fn validate_self_catches_an_invalid_status_key() {
        let mut document = sample();
        document.paths["/users/{id}"]
            .get
            .as_mut()
            .unwrap()
            .responses
            .insert("2xx!".to_owned(), Response::new("nope"));
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("is not a valid response key"),
            "{problems:#?}"
        );
        assert!(is_valid_response_key("200"));
        assert!(is_valid_response_key("4XX"));
        assert!(is_valid_response_key("default"));
        assert!(!is_valid_response_key("099"));
        assert!(!is_valid_response_key("600"));
        assert!(!is_valid_response_key("0XX"));
    }

    #[test]
    fn validate_self_catches_an_undeclared_security_scheme() {
        let mut document = sample();
        document.paths["/users/{id}"].get.as_mut().unwrap().security =
            Some(vec![SecurityRequirement::scheme("bearer")]);
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("names `bearer`"), "{problems:#?}");
        assert!(
            problems[0].contains("components.securitySchemes"),
            "{problems:#?}"
        );
    }

    #[test]
    fn validate_self_catches_scopes_on_a_scopeless_scheme() {
        let mut document = sample();
        document.security = vec![SecurityRequirement::scopes("session", ["read"])];
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("takes none"), "{problems:#?}");
    }

    #[test]
    fn validate_self_catches_an_undeclared_oauth_scope() {
        let mut document = sample();
        document.components.security_schemes.insert(
            "oauth".to_owned(),
            SecurityScheme::oauth2(OAuthFlows::client_credentials(
                OAuthFlow::token_only("https://id.example/token").scope("read", "read"),
            )),
        );
        document.security = vec![SecurityRequirement::scopes("oauth", ["read", "admin"])];
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("`admin`"), "{problems:#?}");
    }

    #[test]
    fn validate_self_catches_an_oauth_flow_missing_its_url() {
        let mut document = sample();
        document.components.security_schemes.insert(
            "oauth".to_owned(),
            SecurityScheme::oauth2(OAuthFlows::authorization_code(
                OAuthFlow::token_only("https://id.example/token").scope("read", "read"),
            )),
        );
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("`authorizationCode` flow requires an `authorizationUrl`"),
            "{problems:#?}"
        );
    }

    #[test]
    fn validate_self_catches_required_naming_an_undeclared_property() {
        let mut document = sample();
        document
            .components
            .schemas
            .get_mut("User")
            .unwrap()
            .required
            .push("nickname".to_owned());
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("`required` names `nickname`"),
            "{problems:#?}"
        );
        assert!(
            problems[0].contains("components.schemas.User"),
            "{problems:#?}"
        );
    }

    #[test]
    fn validate_self_catches_a_repeated_required_entry() {
        let mut document = sample();
        document
            .components
            .schemas
            .get_mut("User")
            .unwrap()
            .required
            .push("id".to_owned());
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("twice"), "{problems:#?}");
    }

    #[test]
    fn validate_self_leaves_composed_schemas_alone() {
        let mut document = sample();
        let mut composed =
            SchemaNode::all_of(vec![SchemaNode::reference("#/components/schemas/User")]);
        composed.required = vec!["id".to_owned()];
        document
            .components
            .schemas
            .insert("Detailed".to_owned(), composed);
        assert_eq!(document.validate_self(), Ok(()));
    }

    #[test]
    fn validate_self_catches_a_server_variable_default_outside_its_enum() {
        let mut document = sample();
        document.servers.push(
            Server::new("https://{region}.shop.example", "regional")
                .variable("region", ServerVariable::new("mars").options(["eu", "us"])),
        );
        let problems = problems(&document);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("`region`"), "{problems:#?}");
    }

    #[test]
    fn validate_self_reports_every_problem_at_once() {
        let mut document = sample();
        document.components.schemas.shift_remove("User");
        document.paths["/users/{id}"]
            .get
            .as_mut()
            .unwrap()
            .parameters
            .clear();
        document.paths["/users/{id}"]
            .get
            .as_mut()
            .unwrap()
            .responses["200"]
            .description = None;
        assert_eq!(problems(&document).len(), 3);
    }

    // ── diagnostics ─────────────────────────────────────────────────────

    #[test]
    fn document_errors_name_the_users_code() {
        let rendered = DocumentError::DuplicateOperationId {
            operation_id: "list_users".to_owned(),
            first: "GET /users".to_owned(),
            second: "GET /accounts".to_owned(),
        }
        .to_string();
        assert!(rendered.contains("list_users"));
        assert!(rendered.contains("GET /accounts"));
        assert!(rendered.contains("#[endpoint(operation_id"));

        let rendered = DocumentError::SchemaCollision {
            name: "User".to_owned(),
            first: "app::api::User".to_owned(),
            second: "app::db::User".to_owned(),
        }
        .to_string();
        assert!(rendered.contains("app::db::User"));
        assert!(rendered.contains("#[schema(rename"));

        let rendered = DocumentError::RouteConflict {
            method: HttpMethod::Post,
            path: "/users".to_owned(),
            first: Some("src/routes.rs:10".to_owned()),
            second: Some("src/routes.rs:40".to_owned()),
        }
        .to_string();
        assert!(rendered.contains("`POST /users` is registered twice"));
        assert!(rendered.contains("src/routes.rs:40"));

        let rendered = DocumentError::PathParameterMismatch {
            path: "/users/{id}".to_owned(),
            missing: vec!["id".to_owned()],
            extra: Vec::new(),
        }
        .to_string();
        assert!(rendered.contains("no `in: path` parameter for `id`"));

        for error in [
            DocumentError::DanglingRef {
                reference: "#/components/schemas/Ghost".to_owned(),
                location: "paths./users.get".to_owned(),
            },
            DocumentError::InvalidStatusKey {
                key: "2xx!".to_owned(),
            },
            DocumentError::UnknownSecurityScheme {
                scheme: "bearer".to_owned(),
                location: "GET /users".to_owned(),
            },
        ] {
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn pointer_tokens_are_unescaped() {
        assert_eq!(unescape_pointer_token("User"), "User");
        assert_eq!(unescape_pointer_token("a~1b"), "a/b");
        assert_eq!(unescape_pointer_token("a~0b"), "a~b");
    }

    #[test]
    fn path_placeholders_are_ordered_and_deduplicated() {
        assert_eq!(path_placeholders("/users"), Vec::<String>::new());
        assert_eq!(
            path_placeholders("/users/{id}/posts/{post}"),
            ["id", "post"]
        );
        assert_eq!(path_placeholders("/files/{*rest}"), ["rest"]);
        assert_eq!(path_placeholders("/a/{x}/b/{x}"), ["x"]);
        assert_eq!(path_placeholders("/broken/{id"), Vec::<String>::new());
    }

    #[test]
    fn empty_components_are_omitted_entirely() {
        let document = Document::new(Info::new("T", "1"));
        assert!(document.components.is_empty());
        assert!(!document.to_json().unwrap().contains("components"));
    }
}
