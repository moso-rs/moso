//! `Query<T>` — typed, validated, documented query strings.
//!
//! Query strings are where frameworks quietly disagree with one another. Moso's
//! behaviour is normative and tested case by case:
//!
//! | Input | Field type | Result |
//! | --- | --- | --- |
//! | `?tags=a&tags=b` | `Vec<String>` | `["a", "b"]` |
//! | `?tags=a,b` | `Vec<String>` with `#[schema(delimiter = ",")]` | `["a", "b"]` |
//! | `?filter[status]=open` | any nested struct field, no attribute needed | `filter.status = "open"` |
//! | field absent | `Option<T>` | `None` |
//! | field absent | `#[schema(default = …)]` | the default, and it is in the document |
//! | field absent | required | 422, `{"pointer": "/limit", "code": "required"}` |
//! | `?limit=abc` | `u32` | 422 with `code: "type"` — **not** 400 |
//! | unknown parameter | — | ignored, unless `#[schema(deny_unknown)]` |
//! | `?flag` | `bool` | `true` |
//!
//! Two of those deserve their reasons stated. A malformed *value* is a 422 and
//! not a 400 because the request is syntactically fine and semantically wrong,
//! which is exactly what 422 means; treating it as 400 loses the field pointer.
//! Unknown parameters are ignored by default because clients add tracking
//! parameters (`utm_source`, `fbclid`) to URLs and rejecting those would break
//! real traffic.
//!
//! # Pointers
//!
//! Every failure is reported at `/query/<field>` — a synthetic root, since a
//! query parameter has no position in the request body that RFC 6901 could
//! address. The root is applied to deserialisation failures and to validation
//! failures alike, so a client sees one addressing scheme.
//!
//! # Delimited lists
//!
//! A bare scalar is **not** silently split: `?tags=a,b` for a plain
//! `Vec<String>` yields `["a,b"]`, because a comma is a legal character in a
//! tag. Splitting is opt-in per field, which is what `#[schema(delimiter = ",")]`
//! expands to:
//!
//! ```
//! use moso::prelude::*;
//!
//! /// The query string this listing accepts.
//! #[derive(Schema)]
//! pub struct ListPosts {
//!     /// Repeatable, or one comma-separated value.
//!     #[schema(delimiter = ",")]
//!     pub tags: Vec<String>,
//! }
//! # fn main() {
//! let q: ListPosts = serde_urlencoded::from_str("tags=rust,web").unwrap();
//! assert_eq!(q.tags, ["rust", "web"]);
//! # }
//! ```
//!
//! [`comma_delimited`], [`pipe_delimited`] and [`space_delimited`] also accept
//! the repeated-key form, so a field declared with a delimiter still handles
//! `?tags=a&tags=b`.
//!
//! # Depth limit
//!
//! Bracket nesting is capped at `http.query_depth_max` (default 8) before
//! parsing, so a crafted `a[b][c][d]…` cannot drive the parser into a deep
//! recursion.

use core::fmt;
use core::marker::PhantomData;

use moso_openapi::{OperationBuilder, Param, ParameterStyle, SchemaNode};
use moso_schema::json_schema::JsonType;
use moso_schema::{Schema, codes, parse_serde_message, push_token};
use serde::de::{
    DeserializeOwned, DeserializeSeed, Deserializer, EnumAccess, Error as _, IntoDeserializer,
    MapAccess, SeqAccess, Unexpected, VariantAccess, Visitor,
};

use crate::ctx::RequestCtx;
use crate::error::{Error, Result};
use crate::extract::Extract;

/// The query string, deserialised into `T` and validated.
///
/// ```
/// use moso::prelude::*;
/// # /// A post, as the API returns one.
/// # #[derive(Schema)] pub struct PostOut { /// URL-safe identifier.
/// #     pub slug: Slug }
/// /// The query string this listing accepts.
/// #[derive(Schema)]
/// pub struct ListPosts {
///     /// Free-text filter.
///     #[schema(len = ..=100)]
///     pub search: Option<String>,
///     /// Where to resume from.
///     pub cursor: Option<Cursor>,
///     /// How many rows to return.
///     #[schema(range = 1..=100, default = 20)]
///     pub limit: u32,
/// }
///
/// /// List posts.
/// #[endpoint]
/// async fn list(Query(q): Query<ListPosts>) -> Result<Page<PostOut>> {
///     let _ = q.search;
///     Ok(Page::empty())
/// }
/// # fn main() { assert_eq!(Router::new().get("/posts", moso::ep!(list)).len(), 1); }
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Query<T>(pub T);

impl<T> Query<T> {
    /// The deserialised parameters.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> core::ops::Deref for Query<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

/// The JSON Pointer root every query-parameter failure is reported under.
pub const QUERY_POINTER_ROOT: &str = "/query";

impl<T: Schema> Extract for Query<T> {
    fn describe(op: &mut OperationBuilder) {
        let node = T::json_schema(op.generator());
        for property in properties_of(&node) {
            op.parameter(query_param(&property));
        }
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        let raw = parts.uri.query().unwrap_or_default();
        let map = QueryMap::parse(raw, ctx.limits().query_depth_max)?;
        let value: T = map.deserialize(DeOptions::QUERY, QUERY_POINTER_ROOT, identity_name)?;
        let mut validation = ctx.validation(QUERY_POINTER_ROOT);
        value.validate(&mut validation).map_err(Error::validation)?;
        Ok(Query(value))
    }
}

// ---------------------------------------------------------------------------
// describe helpers
// ---------------------------------------------------------------------------

/// One property of an object schema, as `describe` needs to see it.
pub(crate) struct SchemaProperty<'a> {
    /// The property name, which is also the parameter name.
    pub(crate) name: &'a str,
    /// The property's schema.
    pub(crate) schema: &'a SchemaNode,
    /// Whether the object lists it in `required`.
    pub(crate) required: bool,
}

/// Every property of `node`, following `allOf` composition one level deep.
///
/// `#[schema(flatten)]` describes itself as `allOf`, so a flattened field's
/// properties are parameters of the same operation and must be walked too.
pub(crate) fn properties_of(node: &SchemaNode) -> Vec<SchemaProperty<'_>> {
    let mut out = Vec::new();
    collect_properties(node, &mut out);
    out
}

fn collect_properties<'a>(node: &'a SchemaNode, out: &mut Vec<SchemaProperty<'a>>) {
    for (name, schema) in &node.properties {
        let required = node.required.iter().any(|r| r == name);
        out.push(SchemaProperty {
            name,
            schema,
            required,
        });
    }
    for part in &node.all_of {
        collect_properties(part, out);
    }
}

/// Whether a schema node describes a JSON array.
pub(crate) fn is_array(node: &SchemaNode) -> bool {
    node.types.contains(JsonType::Array) || node.items.is_some() || !node.prefix_items.is_empty()
}

/// Whether a schema node describes a JSON object.
pub(crate) fn is_object(node: &SchemaNode) -> bool {
    node.types.contains(JsonType::Object) || !node.properties.is_empty()
}

/// Build the `Param` describing one query parameter.
///
/// A property carrying a `default` is documented as optional even when the
/// object lists it in `required`: serde fills it in, so the client need not
/// send it, and documenting it as required would make a generated client demand
/// a value the server does not need.
fn query_param(property: &SchemaProperty<'_>) -> Param {
    let required = property.required && property.schema.default.is_none();
    let mut param = Param::query(property.name)
        .required(required)
        .schema_node(property.schema.clone());
    if let Some(description) = &property.schema.description {
        param = param.description(description.to_string());
    }
    if property.schema.deprecated {
        param = param.deprecated(true);
    }
    if is_object(property.schema) {
        param = param.deep_object();
    } else if is_array(property.schema) {
        param = param.style(ParameterStyle::Form).explode(true);
    }
    param
}

/// The identity parameter-name mapping, used by everything but `Headers<T>`.
pub(crate) fn identity_name(field: &str) -> String {
    field.to_owned()
}

// ---------------------------------------------------------------------------
// QueryMap
// ---------------------------------------------------------------------------

/// The intermediate form a query string is parsed into before deserialisation.
///
/// Repeated keys, comma-delimited lists and `filter[status]` brackets all
/// collapse into this, so the deserialiser downstream sees one shape rather
/// than three special cases.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryMap {
    entries: Vec<(String, QueryValue)>,
}

/// One parsed query value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryValue {
    /// `?a=b`, and `?a` (which is the empty string, coerced to `true` for
    /// a boolean field).
    Scalar(String),
    /// `?a=b&a=c`, or a delimited `?a=b,c`.
    List(Vec<String>),
    /// `?a[b]=c`.
    Map(Vec<(String, QueryValue)>),
}

impl QueryMap {
    /// Parse a raw query string.
    ///
    /// `max_depth` caps bracket nesting; exceeding it is a 400, because a query
    /// that deep is not a client mistake, it is an attempt.
    ///
    /// A key used both as a scalar and as an object (`?a=1&a[b]=2`) is a 400
    /// as well: the two readings are irreconcilable and picking one silently
    /// would hand the handler a value the client did not send.
    pub fn parse(query: &str, max_depth: usize) -> Result<Self> {
        let mut map = QueryMap::default();
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            let (base, path) = split_key(&key);
            if path.len() + 1 > max_depth.max(1) {
                return Err(Error::bad_request(format!(
                    "query parameter `{base}` nests deeper than the {max_depth}-level limit"
                )));
            }
            map.insert(base, &path, value.into_owned())?;
        }
        Ok(map)
    }

    /// Build a map from already-decoded entries.
    ///
    /// Used by [`Form`](crate::extract::Form), [`Headers`](crate::extract::Headers)
    /// and [`Path`](crate::extract::Path), which share this crate's deserialiser
    /// but obtain their pairs from somewhere other than a URI.
    pub fn from_entries(entries: Vec<(String, QueryValue)>) -> Self {
        Self { entries }
    }

    /// The value for `key`.
    pub fn get(&self, key: &str) -> Option<&QueryValue> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// The keys, in first-seen order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(name, _)| name.as_str())
    }

    /// The entries, in first-seen order.
    pub fn entries(&self) -> &[(String, QueryValue)] {
        &self.entries
    }

    /// How many distinct keys were present.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the query string was empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Deserialise into `T`, reporting failures at `root` with `rename` applied
    /// to any field name that appears in a pointer.
    pub(crate) fn deserialize<T: DeserializeOwned>(
        &self,
        options: DeOptions,
        root: &str,
        rename: fn(&str) -> String,
    ) -> Result<T> {
        let deserializer = MapDeserializer::new(&self.entries, options);
        serde_path_to_error::deserialize(deserializer)
            .map_err(|error| deserialisation_error(root, rename, error))
    }

    fn insert(&mut self, base: &str, path: &[Segment], value: String) -> Result<()> {
        if let Some((_, existing)) = self.entries.iter_mut().find(|(name, _)| name == base) {
            return merge(existing, base, path, value);
        }
        self.entries.push((base.to_owned(), build(path, value)));
        Ok(())
    }
}

/// One step of a bracketed key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// `a[b]` — descend into the member named `b`.
    Named(String),
    /// `a[]` — append to a list.
    Append,
}

/// Split `filter[status]` into `("filter", [Named("status")])`.
///
/// A key with unbalanced brackets is treated as a literal name rather than
/// rejected: `utm[source` is a tracking parameter nobody meant as a nested
/// object, and a 400 for it would break real traffic for no benefit.
fn split_key(key: &str) -> (&str, Vec<Segment>) {
    let Some(open) = key.find('[') else {
        return (key, Vec::new());
    };
    let base = &key[..open];
    let mut segments = Vec::new();
    let mut rest = &key[open..];
    while let Some(stripped) = rest.strip_prefix('[') {
        let Some(close) = stripped.find(']') else {
            return (key, Vec::new());
        };
        let name = &stripped[..close];
        segments.push(if name.is_empty() {
            Segment::Append
        } else {
            Segment::Named(name.to_owned())
        });
        rest = &stripped[close + 1..];
    }
    if rest.is_empty() {
        (base, segments)
    } else {
        (key, Vec::new())
    }
}

fn build(path: &[Segment], value: String) -> QueryValue {
    match path {
        [] => QueryValue::Scalar(value),
        [Segment::Append] => QueryValue::List(vec![value]),
        [Segment::Append, rest @ ..] => QueryValue::Map(vec![(String::new(), build(rest, value))]),
        [Segment::Named(name), rest @ ..] => {
            QueryValue::Map(vec![(name.clone(), build(rest, value))])
        }
    }
}

fn merge(target: &mut QueryValue, key: &str, path: &[Segment], value: String) -> Result<()> {
    match path {
        [] => match target {
            QueryValue::Scalar(existing) => {
                *target = QueryValue::List(vec![core::mem::take(existing), value]);
                Ok(())
            }
            QueryValue::List(items) => {
                items.push(value);
                Ok(())
            }
            QueryValue::Map(_) => Err(shape_conflict(key)),
        },
        [Segment::Append] => match target {
            QueryValue::List(items) => {
                items.push(value);
                Ok(())
            }
            QueryValue::Scalar(existing) => {
                *target = QueryValue::List(vec![core::mem::take(existing), value]);
                Ok(())
            }
            QueryValue::Map(_) => Err(shape_conflict(key)),
        },
        [head, rest @ ..] => {
            let name = match head {
                Segment::Named(name) => name.as_str(),
                Segment::Append => "",
            };
            let QueryValue::Map(members) = target else {
                return Err(shape_conflict(key));
            };
            if let Some((_, existing)) = members.iter_mut().find(|(member, _)| member == name) {
                merge(existing, key, rest, value)
            } else {
                members.push((name.to_owned(), build(rest, value)));
                Ok(())
            }
        }
    }
}

fn shape_conflict(key: &str) -> Error {
    Error::bad_request(format!(
        "query parameter `{key}` is used both as a value and as an object"
    ))
}

// ---------------------------------------------------------------------------
// Delimited list helpers
// ---------------------------------------------------------------------------

/// Deserialise `?tags=a,b` into `["a", "b"]`.
///
/// The expansion of `#[schema(delimiter = ",")]`, and usable by hand:
///
/// ```
/// use serde::Deserialize;
///
/// /// The query string this listing accepts.
/// #[derive(Deserialize)]
/// struct Filter {
///     #[serde(deserialize_with = "moso_core::extract::query::comma_delimited")]
///     tags: Vec<String>,
/// }
///
/// let filter: Filter = serde_urlencoded::from_str("tags=rust,web").unwrap();
/// assert_eq!(filter.tags, ["rust", "web"]);
///
/// // One value with no delimiter is a one-element list, not an error.
/// let single: Filter = serde_urlencoded::from_str("tags=rust").unwrap();
/// assert_eq!(single.tags, ["rust"]);
/// ```
///
/// Prefer `#[schema(delimiter = ",")]`, which expands to exactly this.
///
/// The repeated-key form (`?tags=a&tags=b`) still works, so declaring a
/// delimiter widens what a field accepts rather than narrowing it.
///
/// # Errors
/// Propagates whatever the underlying deserialiser reports, including an
/// element that does not parse as `T`.
pub fn comma_delimited<'de, D, T>(deserializer: D) -> core::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    delimited(deserializer, ',')
}

/// Deserialise `?tags=a|b` into `["a", "b"]`. See [`comma_delimited`].
///
/// # Errors
/// As [`comma_delimited`].
pub fn pipe_delimited<'de, D, T>(deserializer: D) -> core::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    delimited(deserializer, '|')
}

/// Deserialise `?tags=a%20b` into `["a", "b"]`. See [`comma_delimited`].
///
/// # Errors
/// As [`comma_delimited`].
pub fn space_delimited<'de, D, T>(deserializer: D) -> core::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    delimited(deserializer, ' ')
}

fn delimited<'de, D, T>(deserializer: D, separator: char) -> core::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    struct DelimitedVisitor<T> {
        separator: char,
        marker: PhantomData<fn() -> T>,
    }

    impl<'de, T: DeserializeOwned> Visitor<'de> for DelimitedVisitor<T> {
        type Value = Vec<T>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "a `{}`-delimited string or a sequence", self.separator)
        }

        fn visit_str<E: serde::de::Error>(self, value: &str) -> core::result::Result<Vec<T>, E> {
            if value.is_empty() {
                return Ok(Vec::new());
            }
            value
                .split(self.separator)
                .map(|part| T::deserialize(part.into_deserializer()))
                .collect()
        }

        fn visit_unit<E: serde::de::Error>(self) -> core::result::Result<Vec<T>, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: serde::de::Error>(self) -> core::result::Result<Vec<T>, E> {
            Ok(Vec::new())
        }

        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> core::result::Result<Vec<T>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(item) = seq.next_element::<T>()? {
                out.push(item);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_any(DelimitedVisitor {
        separator,
        marker: PhantomData,
    })
}

// ---------------------------------------------------------------------------
// The deserialiser
// ---------------------------------------------------------------------------

/// How a scalar is interpreted, which differs slightly per source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeOptions {
    /// A present-but-empty value deserialises a `bool` as `true`.
    ///
    /// True for a query string, where `?flag` *is* the value; false for a form,
    /// where an unticked checkbox is absent rather than empty.
    pub(crate) empty_is_true: bool,
    /// A present-but-empty value deserialises an `Option<T>` as `None`.
    ///
    /// `?search=` means "no search" in every API anyone has ever shipped.
    pub(crate) empty_is_none: bool,
}

impl DeOptions {
    /// Query-string semantics.
    pub(crate) const QUERY: Self = Self {
        empty_is_true: true,
        empty_is_none: true,
    };
    /// `application/x-www-form-urlencoded` semantics.
    pub(crate) const FORM: Self = Self {
        empty_is_true: false,
        empty_is_none: true,
    };
    /// Request-header semantics.
    pub(crate) const HEADER: Self = Self {
        empty_is_true: false,
        empty_is_none: true,
    };
    /// Path-parameter semantics: an empty segment is an empty string, not a
    /// missing value, because the route matched it.
    pub(crate) const PATH: Self = Self {
        empty_is_true: false,
        empty_is_none: false,
    };
}

/// What went wrong, in the vocabulary of `moso_schema::codes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeErrorKind {
    /// A field the target type requires was absent.
    Required,
    /// A value was present but could not be read as the target type.
    Type,
    /// A key the target type rejects — `#[schema(deny_unknown)]`.
    UnknownField,
    /// Anything else, including a constrained type's own rejection.
    Custom,
}

/// The error type the query/form/header/path deserialiser reports.
///
/// A dedicated type rather than `serde::de::value::Error` because the *kind* of
/// failure decides the validation code, and recovering it by matching on a
/// message string would be a bug waiting to happen.
#[derive(Debug, Clone)]
pub(crate) struct DeError {
    kind: DeErrorKind,
    field: Option<String>,
    message: String,
}

impl DeError {
    fn new(kind: DeErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            field: None,
            message: message.into(),
        }
    }

    /// A value that did not parse as the target type.
    pub(crate) fn wrong_type(message: impl Into<String>) -> Self {
        Self::new(DeErrorKind::Type, message)
    }

    /// The source and the target disagree about how many values there are.
    ///
    /// Reported as [`DeErrorKind::Required`] because it is the same class of
    /// problem as a missing field — the *application* is wired wrong, and no
    /// request the client could send would succeed.
    pub(crate) fn arity(message: impl Into<String>) -> Self {
        Self::new(DeErrorKind::Required, message)
    }

    /// What kind of failure this is.
    pub(crate) fn kind(&self) -> DeErrorKind {
        self.kind
    }

    /// The field name, when the failure names one.
    pub(crate) fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    /// The human-readable message.
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl core::error::Error for DeError {}

impl serde::de::Error for DeError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::new(DeErrorKind::Custom, message.to_string())
    }

    fn missing_field(field: &'static str) -> Self {
        Self {
            kind: DeErrorKind::Required,
            field: Some(field.to_owned()),
            message: "this field is required".to_owned(),
        }
    }

    fn unknown_field(field: &str, _expected: &'static [&'static str]) -> Self {
        Self {
            kind: DeErrorKind::UnknownField,
            field: Some(field.to_owned()),
            message: "unknown field".to_owned(),
        }
    }

    fn invalid_type(unexpected: Unexpected<'_>, expected: &dyn serde::de::Expected) -> Self {
        Self::wrong_type(format!("invalid type: {unexpected}, expected {expected}"))
    }

    fn invalid_value(unexpected: Unexpected<'_>, expected: &dyn serde::de::Expected) -> Self {
        Self::wrong_type(format!("invalid value: {unexpected}, expected {expected}"))
    }

    fn invalid_length(length: usize, expected: &dyn serde::de::Expected) -> Self {
        Self::wrong_type(format!("invalid length {length}, expected {expected}"))
    }
}

/// Turn a deserialisation failure into the 422 a client sees.
///
/// The pointer is `root` plus the path `serde_path_to_error` recorded, plus the
/// field name when the failure names one that the path could not reach — which
/// is the case for a missing or unknown field, since neither has a value the
/// deserialiser ever descended into.
pub(crate) fn deserialisation_error(
    root: &str,
    rename: fn(&str) -> String,
    error: serde_path_to_error::Error<DeError>,
) -> Error {
    let pointer = error_pointer(root, rename, &error);
    let inner = error.into_inner();
    let (code, message) = match inner.kind {
        DeErrorKind::Required => (codes::REQUIRED, inner.message.clone()),
        DeErrorKind::Type => (codes::TYPE, inner.message.clone()),
        DeErrorKind::UnknownField => ("custom:unknown_field", inner.message.clone()),
        DeErrorKind::Custom => match parse_serde_message(&inner.message) {
            Some((code, message)) => {
                return Error::validation(moso_schema::ValidationErrors::one(
                    pointer,
                    code.to_owned(),
                    message.to_owned(),
                ));
            }
            None => (codes::TYPE, inner.message.clone()),
        },
    };
    Error::validation(moso_schema::ValidationErrors::one(pointer, code, message))
}

/// The RFC 6901 pointer a deserialisation failure is reported at.
pub(crate) fn error_pointer(
    root: &str,
    rename: fn(&str) -> String,
    error: &serde_path_to_error::Error<DeError>,
) -> String {
    let mut pointer = pointer_for_path(root, error.path());
    let Some(field) = error.inner().field() else {
        return pointer;
    };
    let mut token = String::new();
    push_token(&mut token, &rename(field));
    // A missing field is raised at the struct, so the path stops one level
    // short and the name has to be appended. An *unknown* field is raised while
    // reading the key, which `serde_path_to_error` has already recorded — so
    // appending again would produce `/query/nope/nope`.
    if error.inner().kind() == DeErrorKind::UnknownField && pointer.ends_with(&token) {
        return pointer;
    }
    pointer.push_str(&token);
    pointer
}

/// Render a `serde_path_to_error` path as an RFC 6901 JSON Pointer under `root`.
///
/// Shared with [`Json`](crate::extract::Json), whose pointers come from
/// `serde_json` rather than from this module's deserialiser but must address
/// the same document the same way.
pub(crate) fn pointer_for_path(root: &str, path: &serde_path_to_error::Path) -> String {
    let mut pointer = root.to_owned();
    for segment in path.iter() {
        match segment {
            serde_path_to_error::Segment::Seq { index } => {
                pointer.push('/');
                pointer.push_str(itoa(*index).as_str());
            }
            serde_path_to_error::Segment::Map { key } => push_token(&mut pointer, key),
            serde_path_to_error::Segment::Enum { variant } => push_token(&mut pointer, variant),
            _ => push_token(&mut pointer, "?"),
        }
    }
    pointer
}

/// `usize` to `&str` without an allocation, for pointer segments.
fn itoa(mut value: usize) -> ArrayIndex {
    let mut buf = [0u8; 20];
    let mut index = buf.len();
    loop {
        index -= 1;
        buf[index] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        value /= 10;
        if value == 0 {
            break;
        }
    }
    ArrayIndex { buf, start: index }
}

struct ArrayIndex {
    buf: [u8; 20],
    start: usize,
}

impl ArrayIndex {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[self.start..]).unwrap_or("0")
    }
}

/// Deserialises a list of `(key, value)` entries as a map or a struct.
pub(crate) struct MapDeserializer<'a> {
    entries: &'a [(String, QueryValue)],
    options: DeOptions,
}

impl<'a> MapDeserializer<'a> {
    pub(crate) fn new(entries: &'a [(String, QueryValue)], options: DeOptions) -> Self {
        Self { entries, options }
    }
}

impl<'de, 'a> Deserializer<'de> for MapDeserializer<'a> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_map(EntriesAccess::new(self.entries, self.options))
    }

    fn deserialize_option<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map struct enum
        identifier ignored_any
    }
}

struct EntriesAccess<'a> {
    iter: core::slice::Iter<'a, (String, QueryValue)>,
    value: Option<&'a QueryValue>,
    options: DeOptions,
}

impl<'a> EntriesAccess<'a> {
    fn new(entries: &'a [(String, QueryValue)], options: DeOptions) -> Self {
        Self {
            iter: entries.iter(),
            value: None,
            options,
        }
    }
}

impl<'de, 'a> MapAccess<'de> for EntriesAccess<'a> {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> core::result::Result<Option<K::Value>, DeError> {
        match self.iter.next() {
            Some((key, value)) => {
                self.value = Some(value);
                seed.deserialize(key.as_str().into_deserializer()).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> core::result::Result<V::Value, DeError> {
        let value = self
            .value
            .take()
            .ok_or_else(|| DeError::custom("a value was requested before its key"))?;
        seed.deserialize(ValueDeserializer::new(value, self.options))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

/// Deserialises one parsed value: a scalar, a repeated list, or an object.
pub(crate) struct ValueDeserializer<'a> {
    value: &'a QueryValue,
    options: DeOptions,
}

impl<'a> ValueDeserializer<'a> {
    pub(crate) fn new(value: &'a QueryValue, options: DeOptions) -> Self {
        Self { value, options }
    }

    fn scalar<'de, V: Visitor<'de>>(&self, visitor: &V) -> core::result::Result<&'a str, DeError> {
        match self.value {
            QueryValue::Scalar(value) => Ok(value.as_str()),
            QueryValue::List(items) => match items.as_slice() {
                [only] => Ok(only.as_str()),
                _ => Err(DeError::invalid_type(Unexpected::Seq, visitor)),
            },
            QueryValue::Map(_) => Err(DeError::invalid_type(Unexpected::Map, visitor)),
        }
    }
}

macro_rules! forward_scalar {
    ($($method:ident),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> core::result::Result<V::Value, DeError> {
            let scalar = self.scalar(&visitor)?;
            ScalarDeserializer::new(scalar, self.options).$method(visitor)
        }
    )*};
}

impl<'de, 'a> Deserializer<'de> for ValueDeserializer<'a> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        match self.value {
            QueryValue::Scalar(value) => visitor.visit_str(value),
            QueryValue::List(items) => visitor.visit_seq(ListAccess::new(items, self.options)),
            QueryValue::Map(members) => {
                visitor.visit_map(EntriesAccess::new(members, self.options))
            }
        }
    }

    forward_scalar!(
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_unit,
        deserialize_identifier,
    );

    fn deserialize_option<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        match self.value {
            QueryValue::Scalar(value) if value.is_empty() && self.options.empty_is_none => {
                visitor.visit_none()
            }
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        match self.value {
            QueryValue::List(items) => visitor.visit_seq(ListAccess::new(items, self.options)),
            QueryValue::Scalar(value) => visitor.visit_seq(ListAccess::single(value, self.options)),
            QueryValue::Map(members) => match indexed_values(members) {
                Some(items) => visitor.visit_seq(RefListAccess::new(items, self.options)),
                None => Err(DeError::invalid_type(Unexpected::Map, &visitor)),
            },
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        match self.value {
            QueryValue::Map(members) => {
                visitor.visit_map(EntriesAccess::new(members, self.options))
            }
            QueryValue::Scalar(_) => Err(DeError::invalid_type(
                Unexpected::Other("a value"),
                &visitor,
            )),
            QueryValue::List(_) => Err(DeError::invalid_type(Unexpected::Seq, &visitor)),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        match self.value {
            QueryValue::Map(members) => match members.as_slice() {
                [(variant, value)] => visitor.visit_enum(SingleVariant {
                    variant: variant.as_str(),
                    value,
                    options: self.options,
                }),
                _ => Err(DeError::invalid_type(Unexpected::Map, &visitor)),
            },
            _ => {
                let scalar = self.scalar(&visitor)?;
                ScalarDeserializer::new(scalar, self.options)
                    .deserialize_enum(name, variants, visitor)
            }
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_unit()
    }
}

/// The values of a map whose keys are all decimal indices, in index order.
///
/// `?a[0]=x&a[1]=y` is a list written the long way; refusing it would send
/// people to a different framework over punctuation.
fn indexed_values(members: &[(String, QueryValue)]) -> Option<Vec<&QueryValue>> {
    let mut indexed: Vec<(usize, &QueryValue)> = Vec::with_capacity(members.len());
    for (key, value) in members {
        indexed.push((key.parse().ok()?, value));
    }
    indexed.sort_by_key(|(index, _)| *index);
    Some(indexed.into_iter().map(|(_, value)| value).collect())
}

struct ListAccess<'a> {
    items: core::slice::Iter<'a, String>,
    single: Option<&'a str>,
    options: DeOptions,
}

impl<'a> ListAccess<'a> {
    fn new(items: &'a [String], options: DeOptions) -> Self {
        Self {
            items: items.iter(),
            single: None,
            options,
        }
    }

    fn single(value: &'a str, options: DeOptions) -> Self {
        const EMPTY: &[String] = &[];
        Self {
            items: EMPTY.iter(),
            single: Some(value),
            options,
        }
    }
}

impl<'de, 'a> SeqAccess<'de> for ListAccess<'a> {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> core::result::Result<Option<T::Value>, DeError> {
        if let Some(value) = self.single.take() {
            return seed
                .deserialize(ScalarDeserializer::new(value, self.options))
                .map(Some);
        }
        match self.items.next() {
            Some(value) => seed
                .deserialize(ScalarDeserializer::new(value, self.options))
                .map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len() + usize::from(self.single.is_some()))
    }
}

struct RefListAccess<'a> {
    items: std::vec::IntoIter<&'a QueryValue>,
    options: DeOptions,
}

impl<'a> RefListAccess<'a> {
    fn new(items: Vec<&'a QueryValue>, options: DeOptions) -> Self {
        Self {
            items: items.into_iter(),
            options,
        }
    }
}

impl<'de, 'a> SeqAccess<'de> for RefListAccess<'a> {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> core::result::Result<Option<T::Value>, DeError> {
        match self.items.next() {
            Some(value) => seed
                .deserialize(ValueDeserializer::new(value, self.options))
                .map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len())
    }
}

/// `?kind[circle][radius]=3` — an externally tagged enum with one variant.
struct SingleVariant<'a> {
    variant: &'a str,
    value: &'a QueryValue,
    options: DeOptions,
}

impl<'de, 'a> EnumAccess<'de> for SingleVariant<'a> {
    type Error = DeError;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> core::result::Result<(V::Value, Self), DeError> {
        let variant = seed.deserialize(self.variant.into_deserializer())?;
        Ok((variant, self))
    }
}

impl<'de, 'a> VariantAccess<'de> for SingleVariant<'a> {
    type Error = DeError;

    fn unit_variant(self) -> core::result::Result<(), DeError> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> core::result::Result<T::Value, DeError> {
        seed.deserialize(ValueDeserializer::new(self.value, self.options))
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        ValueDeserializer::new(self.value, self.options).deserialize_seq(visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        ValueDeserializer::new(self.value, self.options).deserialize_map(visitor)
    }
}

/// Deserialises one string, which is what every leaf of a query string is.
pub(crate) struct ScalarDeserializer<'a> {
    value: &'a str,
    options: DeOptions,
}

impl<'a> ScalarDeserializer<'a> {
    pub(crate) fn new(value: &'a str, options: DeOptions) -> Self {
        Self { value, options }
    }
}

macro_rules! parse_scalar {
    ($($method:ident => $visit:ident, $ty:ty);* $(;)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> core::result::Result<V::Value, DeError> {
            match self.value.trim().parse::<$ty>() {
                Ok(parsed) => visitor.$visit(parsed),
                Err(_) => Err(DeError::invalid_value(
                    Unexpected::Str(self.value),
                    &visitor,
                )),
            }
        }
    )*};
}

impl<'de, 'a> Deserializer<'de> for ScalarDeserializer<'a> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_str(self.value)
    }

    fn deserialize_bool<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        match parse_bool(self.value, self.options) {
            Some(parsed) => visitor.visit_bool(parsed),
            None => Err(DeError::invalid_value(
                Unexpected::Str(self.value),
                &visitor,
            )),
        }
    }

    parse_scalar! {
        deserialize_i8   => visit_i8,   i8;
        deserialize_i16  => visit_i16,  i16;
        deserialize_i32  => visit_i32,  i32;
        deserialize_i64  => visit_i64,  i64;
        deserialize_i128 => visit_i128, i128;
        deserialize_u8   => visit_u8,   u8;
        deserialize_u16  => visit_u16,  u16;
        deserialize_u32  => visit_u32,  u32;
        deserialize_u64  => visit_u64,  u64;
        deserialize_u128 => visit_u128, u128;
        deserialize_f32  => visit_f32,  f32;
        deserialize_f64  => visit_f64,  f64;
        deserialize_char => visit_char, char;
    }

    fn deserialize_str<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_str(self.value)
    }

    fn deserialize_string<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_str(self.value)
    }

    fn deserialize_bytes<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_bytes(self.value.as_bytes())
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_byte_buf(self.value.as_bytes().to_vec())
    }

    fn deserialize_option<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        if self.value.is_empty() && self.options.empty_is_none {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_seq(ListAccess::single(self.value, self.options))
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        Err(DeError::invalid_type(Unexpected::Str(self.value), &visitor))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        Err(DeError::invalid_type(Unexpected::Str(self.value), &visitor))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_enum(self.value.into_deserializer())
    }

    fn deserialize_identifier<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_str(self.value)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_unit()
    }
}

/// The spellings a boolean accepts.
///
/// `?flag` arrives as an empty value and means `true`; a form's unticked
/// checkbox arrives not at all. Anything outside these lists is a 422 rather
/// than a silent `false`, because a typo that quietly disables a setting is
/// worse than an error.
fn parse_bool(value: &str, options: DeOptions) -> Option<bool> {
    let value = value.trim();
    if value.is_empty() {
        return Some(options.empty_is_true);
    }
    const TRUE: &[&str] = &["true", "1", "on", "yes", "y"];
    const FALSE: &[&str] = &["false", "0", "off", "no", "n"];
    if TRUE.iter().any(|c| c.eq_ignore_ascii_case(value)) {
        Some(true)
    } else if FALSE.iter().any(|c| c.eq_ignore_ascii_case(value)) {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn parse(query: &str) -> QueryMap {
        QueryMap::parse(query, 8).expect("the query parses")
    }

    fn decode<T: DeserializeOwned>(query: &str) -> core::result::Result<T, DeError> {
        let map = parse(query);
        let deserializer = MapDeserializer::new(map.entries(), DeOptions::QUERY);
        serde_path_to_error::deserialize(deserializer)
            .map_err(serde_path_to_error::Error::into_inner)
    }

    fn decode_error<T: DeserializeOwned>(query: &str) -> DeError {
        decode::<T>(query).err().expect("deserialisation fails")
    }

    fn pointer_of<T: DeserializeOwned + fmt::Debug>(query: &str) -> String {
        let map = parse(query);
        let deserializer = MapDeserializer::new(map.entries(), DeOptions::QUERY);
        let error = serde_path_to_error::deserialize::<_, T>(deserializer)
            .expect_err("deserialisation fails");
        error_pointer(QUERY_POINTER_ROOT, identity_name, &error)
    }

    #[test]
    fn query_derefs_to_its_payload() {
        let query = Query(7u8);
        assert_eq!(*query, 7);
    }

    #[test]
    fn an_empty_query_map_has_no_keys() {
        let map = QueryMap::default();
        assert!(map.is_empty());
        assert_eq!(map.keys().count(), 0);
    }

    // ── the behaviour table, one test per row ────────────────────────────

    #[derive(Debug, Deserialize, PartialEq)]
    struct Tags {
        tags: Vec<String>,
    }

    #[test]
    fn row_repeated_keys_become_a_vec() {
        assert_eq!(
            decode::<Tags>("tags=a&tags=b").unwrap(),
            Tags {
                tags: vec!["a".into(), "b".into()]
            }
        );
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct DelimitedTags {
        #[serde(deserialize_with = "comma_delimited")]
        tags: Vec<String>,
    }

    #[test]
    fn row_a_declared_delimiter_splits_one_value() {
        assert_eq!(
            decode::<DelimitedTags>("tags=a,b").unwrap(),
            DelimitedTags {
                tags: vec!["a".into(), "b".into()]
            }
        );
    }

    #[test]
    fn a_declared_delimiter_still_accepts_repeated_keys() {
        assert_eq!(
            decode::<DelimitedTags>("tags=a&tags=b").unwrap(),
            DelimitedTags {
                tags: vec!["a".into(), "b".into()]
            }
        );
    }

    #[test]
    fn without_a_declared_delimiter_a_comma_is_data() {
        assert_eq!(
            decode::<Tags>("tags=a,b").unwrap(),
            Tags {
                tags: vec!["a,b".into()]
            }
        );
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Filter {
        status: String,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Filtered {
        filter: Filter,
    }

    #[test]
    fn row_bracket_nesting_builds_a_struct() {
        assert_eq!(
            decode::<Filtered>("filter[status]=open").unwrap(),
            Filtered {
                filter: Filter {
                    status: "open".into()
                }
            }
        );
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Optional {
        search: Option<String>,
    }

    #[test]
    fn row_a_missing_option_is_none() {
        assert_eq!(decode::<Optional>("").unwrap(), Optional { search: None });
    }

    #[test]
    fn an_explicitly_empty_option_is_also_none() {
        assert_eq!(
            decode::<Optional>("search=").unwrap(),
            Optional { search: None }
        );
    }

    fn default_limit() -> u32 {
        20
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Defaulted {
        #[serde(default = "default_limit")]
        limit: u32,
    }

    #[test]
    fn row_a_missing_field_with_a_default_takes_it() {
        assert_eq!(decode::<Defaulted>("").unwrap(), Defaulted { limit: 20 });
        assert_eq!(
            decode::<Defaulted>("limit=5").unwrap(),
            Defaulted { limit: 5 }
        );
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Required {
        limit: u32,
    }

    #[test]
    fn row_a_missing_required_field_is_a_required_failure() {
        let error = decode_error::<Required>("");
        assert_eq!(error.kind(), DeErrorKind::Required);
        assert_eq!(error.field(), Some("limit"));
        assert_eq!(pointer_of::<Required>(""), "/query/limit");
    }

    #[test]
    fn row_a_bad_scalar_is_a_type_failure_not_a_parse_failure() {
        let error = decode_error::<Required>("limit=abc");
        assert_eq!(error.kind(), DeErrorKind::Type);
        assert_eq!(pointer_of::<Required>("limit=abc"), "/query/limit");
    }

    #[test]
    fn row_unknown_parameters_are_ignored() {
        assert_eq!(
            decode::<Required>("limit=5&utm_source=newsletter&fbclid=x").unwrap(),
            Required { limit: 5 }
        );
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Strict {
        limit: u32,
    }

    #[test]
    fn row_deny_unknown_rejects_an_extra_parameter() {
        let error = decode_error::<Strict>("limit=5&nope=1");
        assert_eq!(error.kind(), DeErrorKind::UnknownField);
        assert_eq!(error.field(), Some("nope"));
        assert_eq!(pointer_of::<Strict>("limit=5&nope=1"), "/query/nope");
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Flagged {
        flag: bool,
    }

    #[test]
    fn row_a_bare_flag_is_true() {
        assert_eq!(decode::<Flagged>("flag").unwrap(), Flagged { flag: true });
        assert_eq!(decode::<Flagged>("flag=").unwrap(), Flagged { flag: true });
        assert_eq!(
            decode::<Flagged>("flag=true").unwrap(),
            Flagged { flag: true }
        );
        assert_eq!(
            decode::<Flagged>("flag=false").unwrap(),
            Flagged { flag: false }
        );
        assert_eq!(
            decode::<Flagged>("flag=on").unwrap(),
            Flagged { flag: true }
        );
        assert_eq!(
            decode_error::<Flagged>("flag=maybe").kind(),
            DeErrorKind::Type
        );
    }

    // ── parsing ──────────────────────────────────────────────────────────

    #[test]
    fn percent_and_plus_encoding_are_decoded() {
        let map = parse("q=hello+world&r=a%20b%26c");
        assert_eq!(
            map.get("q"),
            Some(&QueryValue::Scalar("hello world".to_owned()))
        );
        assert_eq!(map.get("r"), Some(&QueryValue::Scalar("a b&c".to_owned())));
    }

    #[test]
    fn repeated_keys_fold_into_a_list_preserving_order() {
        let map = parse("a=1&a=2&a=3");
        assert_eq!(
            map.get("a"),
            Some(&QueryValue::List(vec![
                "1".to_owned(),
                "2".to_owned(),
                "3".to_owned()
            ]))
        );
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn empty_brackets_build_a_list() {
        let map = parse("a[]=1&a[]=2");
        assert_eq!(
            map.get("a"),
            Some(&QueryValue::List(vec!["1".to_owned(), "2".to_owned()]))
        );
    }

    #[test]
    fn nested_brackets_build_nested_maps() {
        let map = parse("a[b][c]=1");
        let expected = QueryValue::Map(vec![(
            "b".to_owned(),
            QueryValue::Map(vec![("c".to_owned(), QueryValue::Scalar("1".to_owned()))]),
        )]);
        assert_eq!(map.get("a"), Some(&expected));
    }

    #[test]
    fn nesting_deeper_than_the_limit_is_refused() {
        assert!(QueryMap::parse("a[b][c][d]=1", 2).is_err());
        assert!(QueryMap::parse("a[b][c][d]=1", 8).is_ok());
    }

    #[test]
    fn an_unbalanced_bracket_is_a_literal_key() {
        let map = parse("utm[source=x");
        assert_eq!(
            map.get("utm[source"),
            Some(&QueryValue::Scalar("x".to_owned()))
        );
    }

    #[test]
    fn a_key_used_as_both_a_value_and_an_object_is_refused() {
        assert!(QueryMap::parse("a=1&a[b]=2", 8).is_err());
    }

    #[test]
    fn indexed_maps_deserialise_as_sequences() {
        assert_eq!(
            decode::<Tags>("tags[1]=b&tags[0]=a").unwrap(),
            Tags {
                tags: vec!["a".into(), "b".into()]
            }
        );
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Nested {
        page: Page,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Page {
        size: u32,
    }

    #[test]
    fn a_nested_type_failure_carries_the_full_pointer() {
        assert_eq!(pointer_of::<Nested>("page[size]=huge"), "/query/page/size");
    }

    #[test]
    fn a_nested_missing_field_carries_the_full_pointer() {
        assert_eq!(pointer_of::<Nested>("page[other]=1"), "/query/page/size");
    }

    #[test]
    fn a_pointer_token_containing_a_slash_is_escaped() {
        let map =
            QueryMap::from_entries(vec![("a/b".to_owned(), QueryValue::Scalar("x".to_owned()))]);
        let deserializer = MapDeserializer::new(map.entries(), DeOptions::QUERY);
        let error = serde_path_to_error::deserialize::<_, Strict>(deserializer)
            .expect_err("the unknown field is rejected");
        assert_eq!(
            error_pointer(QUERY_POINTER_ROOT, identity_name, &error),
            "/query/a~1b"
        );
    }

    #[test]
    fn boolean_spellings_follow_the_documented_lists() {
        assert_eq!(parse_bool("", DeOptions::QUERY), Some(true));
        assert_eq!(parse_bool("", DeOptions::FORM), Some(false));
        assert_eq!(parse_bool("YES", DeOptions::QUERY), Some(true));
        assert_eq!(parse_bool("Off", DeOptions::QUERY), Some(false));
        assert_eq!(parse_bool("perhaps", DeOptions::QUERY), None);
    }

    #[test]
    fn key_splitting_recognises_every_shape() {
        assert_eq!(split_key("a"), ("a", vec![]));
        assert_eq!(
            split_key("a[b]"),
            ("a", vec![Segment::Named("b".to_owned())])
        );
        assert_eq!(split_key("a[]"), ("a", vec![Segment::Append]));
        assert_eq!(
            split_key("a[b][]"),
            ("a", vec![Segment::Named("b".to_owned()), Segment::Append])
        );
        assert_eq!(split_key("a[b"), ("a[b", vec![]));
        assert_eq!(split_key("a[b]c"), ("a[b]c", vec![]));
    }

    #[test]
    fn index_formatting_matches_the_decimal_rendering() {
        assert_eq!(itoa(0).as_str(), "0");
        assert_eq!(itoa(1234).as_str(), "1234");
        assert_eq!(itoa(usize::MAX).as_str(), usize::MAX.to_string());
    }
}
