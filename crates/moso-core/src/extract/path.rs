//! `Path<T>` — typed path parameters.
//!
//! ```
//! use moso::prelude::*;
//! use moso::response::NoContent;
//! # /// A post.
//! # pub struct Post;
//! # /// A comment.
//! # pub struct Comment;
//! # /// A post, as the API returns one.
//! # #[derive(Schema)] pub struct PostOut { /// URL-safe identifier.
//! #     pub slug: Slug }
//! /// One segment: the type is the parameter.
//! #[endpoint]
//! async fn show(Path(slug): Path<Slug>) -> Result<Json<PostOut>> {
//!     Ok(Json(PostOut { slug }))
//! }
//!
//! /// Two segments, in declaration order.
//! #[endpoint]
//! async fn comment(Path(ids): Path<(Id<Post>, Id<Comment>)>) -> Result<NoContent> {
//!     let _ = ids;
//!     Ok(NoContent)
//! }
//!
//! /// Two segments, named — better when there are more than two.
//! #[derive(Schema)]
//! pub struct Target {
//!     /// Which post.
//!     pub post: Id<Post>,
//!     /// Which comment on it.
//!     pub comment: Id<Comment>,
//! }
//!
//! /// Edit one comment.
//! #[endpoint]
//! async fn edit(Path(target): Path<Target>) -> Result<NoContent> {
//!     let _ = target.post;
//!     Ok(NoContent)
//! }
//! # fn main() {
//! let router = moso::routes! {
//!     GET   "/posts/{slug}"                     => show,
//!     POST  "/posts/{post}/comments/{comment}"  => comment,
//!     PATCH "/posts/{post}/comments/{comment}"  => edit,
//! };
//! assert_eq!(router.len(), 3);
//! # }
//! ```
//!
//! # Naming
//!
//! For a struct `T`, the field names **must** match the route's parameter
//! names. A mismatch is a boot error naming both sides — Axum answers the same
//! mistake with a runtime 500 or a silently-absent value. [`path_shape`] is what
//! `App::build()` calls to perform that check: it reports the names a `T`
//! declares, or how many positional slots a scalar or tuple needs.
//!
//! For a scalar or a tuple, there are no field names to match, so `describe`
//! contributes parameters with **empty names** and the router fills them in
//! positionally from the path template at `App::build()`. That is why
//! [`Path::describe`] can be correct without knowing the path: the one piece of
//! information it lacks is supplied by the only component that has it.
//!
//! # Validation
//!
//! `T: Schema`, so `T::validate` runs after deserialisation exactly as it does
//! for a body. `Path<Slug>` cannot yield a string that is not a slug.
//!
//! # A mismatch at runtime is a 500, not a 422
//!
//! If a struct declares `post_id` and the route declares `{id}`, the client did
//! nothing wrong — the application is misconfigured. Extraction therefore
//! reports a 500 naming both sides rather than a 422 blaming the request. Boot
//! validation is meant to catch this first; the runtime error exists for the
//! route registered by a path that boot could not see.

use moso_openapi::{OperationBuilder, Param, SchemaGenerator, SchemaNode};
use moso_schema::Schema;
use serde::de::{DeserializeOwned, Deserializer, Visitor};

use crate::ctx::RequestCtx;
use crate::error::{Error, Result};
use crate::extract::Extract;
use crate::extract::query::{
    DeError, DeErrorKind, DeOptions, MapDeserializer, QueryValue, ValueDeserializer,
    deserialisation_error, identity_name, is_array, is_object, properties_of,
};

/// Path parameters, deserialised into `T` and validated.
///
/// `T` is deserialised from the captured segments: a single type for one capture, a
/// tuple for several in order, or a `#[derive(Schema)]` struct whose field names
/// match the template's braces.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::NoContent;
/// # /// A post.
/// # pub struct Post;
///
/// /// Show one post.
/// #[endpoint]
/// async fn show(Path(slug): Path<Slug>) -> Result<Json<String>> {
///     Ok(Json(slug.to_string()))
/// }
///
/// /// Which comment to act on.
/// #[derive(Schema)]
/// pub struct Target {
///     /// Which post.
///     pub post: Id<Post>,
///     /// Which comment on it.
///     pub comment: u32,
/// }
///
/// /// Delete one comment.
/// #[endpoint]
/// async fn destroy(Path(target): Path<Target>) -> Result<NoContent> {
///     let _ = (target.post, target.comment);
///     Ok(NoContent)
/// }
/// # fn main() {
/// let router = moso::routes! {
///     GET    "/posts/{slug}"                    => show,
///     DELETE "/posts/{post}/comments/{comment}" => destroy,
/// };
/// assert_eq!(router.len(), 2);
/// # }
/// ```
///
/// A name in `T` that the path template does not capture is a boot error naming
/// both sides, not a 500 on the first request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Path<T>(pub T);

impl<T> Path<T> {
    /// The deserialised parameters.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> core::ops::Deref for Path<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

/// The JSON Pointer root a path-parameter failure is reported under.
pub const PATH_POINTER_ROOT: &str = "/path";

/// What a `Path<T>` needs from the route template.
///
/// Returned by [`path_shape`] so `App::build()` can compare a handler's
/// declaration against the path it was mounted at, and report a mismatch by
/// name instead of letting it become a runtime surprise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathShape {
    /// A struct: these field names, which must match the template's placeholders.
    Named(Vec<String>),
    /// A scalar or a tuple: this many placeholders, matched positionally.
    Positional(usize),
}

impl PathShape {
    /// How many placeholders the route must declare.
    pub fn len(&self) -> usize {
        match self {
            PathShape::Named(names) => names.len(),
            PathShape::Positional(count) => *count,
        }
    }

    /// Whether the type consumes no path parameters at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The declared names, for a struct.
    pub fn names(&self) -> Option<&[String]> {
        match self {
            PathShape::Named(names) => Some(names),
            PathShape::Positional(_) => None,
        }
    }

    /// The names a template declares that `self` does not, and vice versa.
    ///
    /// Empty for a positional shape, whose only requirement is a matching
    /// count. Returns `(missing_from_type, missing_from_template)`.
    pub fn mismatch(&self, template: &[&str]) -> (Vec<String>, Vec<String>) {
        let PathShape::Named(names) = self else {
            return (Vec::new(), Vec::new());
        };
        let missing_from_type = template
            .iter()
            .filter(|declared| !names.iter().any(|name| name == *declared))
            .map(|declared| (*declared).to_owned())
            .collect();
        let missing_from_template = names
            .iter()
            .filter(|name| !template.iter().any(|declared| declared == name))
            .cloned()
            .collect();
        (missing_from_type, missing_from_template)
    }
}

/// What `T` declares about the route it is mounted on.
///
/// An object schema yields [`PathShape::Named`]; a tuple yields
/// [`PathShape::Positional`] with one slot per element; anything else yields
/// one positional slot.
pub fn path_shape<T: Schema>(generator: &mut SchemaGenerator) -> PathShape {
    shape_of(&T::json_schema(generator))
}

fn shape_of(node: &SchemaNode) -> PathShape {
    if is_object(node) {
        return PathShape::Named(
            properties_of(node)
                .into_iter()
                .map(|property| property.name.to_owned())
                .collect(),
        );
    }
    if is_array(node) {
        return PathShape::Positional(node.prefix_items.len().max(1));
    }
    PathShape::Positional(1)
}

impl<T: Schema> Extract for Path<T> {
    fn describe(op: &mut OperationBuilder) {
        let node = T::json_schema(op.generator());
        match shape_of(&node) {
            PathShape::Named(_) => {
                for property in properties_of(&node) {
                    let mut param = Param::path(property.name).schema_node(property.schema.clone());
                    if let Some(description) = &property.schema.description {
                        param = param.description(description.to_string());
                    }
                    op.parameter(param);
                }
            }
            PathShape::Positional(count) => {
                // The names are the router's to supply: it is the only
                // component that has seen the path template. An empty name is
                // the agreed placeholder, filled in at `App::build()`.
                let schemas: Vec<SchemaNode> = if node.prefix_items.is_empty() {
                    vec![node.clone(); count]
                } else {
                    node.prefix_items.clone()
                };
                for schema in schemas {
                    op.parameter(Param::path("").schema_node(schema));
                }
            }
        }
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        let params = captured_parameters(parts)?;
        let value = deserialize_path::<T>(&params)?;
        let mut validation = ctx.validation(PATH_POINTER_ROOT);
        value.validate(&mut validation).map_err(Error::validation)?;
        Ok(Path(value))
    }
}

/// The `(name, value)` captures for the matched route, percent-decoded.
///
/// Read from the request parts rather than from
/// [`RequestCtx`](crate::RequestCtx) because the parts are where the router put
/// them: the context snapshots the request *head*, and a capture belongs to the
/// match rather than to the head.
///
/// # Why this is not `extensions.get::<RawPathParams>()`
///
/// Axum does not insert a `RawPathParams` — it inserts its own private
/// `UrlParams`, and `RawPathParams` is the *extractor* that reads it. Asking the
/// extensions for `RawPathParams` therefore always answers `None`, silently, and
/// every `Path<T>` on a parameterised route turns into a 500 about arity. The
/// only public way in is the `FromRequestParts` impl, which does not await, so
/// `now_or_never` resolves it in place. This mirrors `ctx::take_path_params`.
fn captured_parameters(parts: &http::request::Parts) -> Result<Vec<(String, QueryValue)>> {
    use axum::extract::{FromRequestParts, RawPathParams};
    use futures_util::future::FutureExt as _;

    // `from_request_parts` needs `&mut Parts`; the captures live in the
    // extensions, so a scratch head carrying a clone of them is enough and
    // leaves the caller's parts untouched.
    let (mut scratch, ()) = http::Request::new(()).into_parts();
    scratch.extensions = parts.extensions.clone();

    let raw = match RawPathParams::from_request_parts(&mut scratch, &()).now_or_never() {
        Some(Ok(raw)) => raw,
        // Either the route captured nothing, or a capture was not valid UTF-8.
        // Both mean "no usable parameters"; a target that needs one then fails
        // in `deserialize_path`, which has the names to say so.
        _ => return Ok(Vec::new()),
    };

    Ok(raw
        .iter()
        .map(|(name, value)| (name.to_owned(), QueryValue::Scalar(value.to_owned())))
        .collect())
}

/// Deserialise captures into `T`, keeping the structured failure.
fn raw_deserialize_path<T: DeserializeOwned>(
    params: &[(String, QueryValue)],
) -> core::result::Result<T, serde_path_to_error::Error<DeError>> {
    serde_path_to_error::deserialize(PathDeserializer {
        params,
        options: DeOptions::PATH,
    })
}

/// Deserialise captures into `T`, whatever shape `T` has.
fn deserialize_path<T: DeserializeOwned>(params: &[(String, QueryValue)]) -> Result<T> {
    match raw_deserialize_path(params) {
        Ok(value) => Ok(value),
        Err(error) if error.inner().kind() == DeErrorKind::Required => {
            let declared: Vec<&str> = params.iter().map(|(name, _)| name.as_str()).collect();
            let detail = match error.inner().field() {
                Some(field) => format!(
                    "the route does not capture a path parameter named `{field}`; it declares \
                     {declared:?}"
                ),
                None => error.inner().message().to_owned(),
            };
            Err(Error::internal_msg(format!(
                "{detail}. The route template and the handler's `Path<…>` type must agree."
            )))
        }
        Err(error) => Err(deserialisation_error(
            PATH_POINTER_ROOT,
            identity_name,
            error,
        )),
    }
}

/// Deserialises path captures as a struct, a tuple, or a bare scalar.
///
/// The three forms cannot be told apart from the captures alone — one capture
/// could be `Path<Uuid>` or `Path<(Uuid,)>` or a one-field struct — so the
/// decision is made by which `deserialize_*` method serde calls, which is
/// exactly the information the target type has.
struct PathDeserializer<'a> {
    params: &'a [(String, QueryValue)],
    options: DeOptions,
}

impl<'a> PathDeserializer<'a> {
    /// The single capture a scalar target reads.
    ///
    /// A count other than one means the route template and the handler's
    /// `Path<…>` disagree, which is an application error rather than a client
    /// one — hence [`DeError::arity`], which
    /// [`deserialize_path`] renders as a 500.
    fn only(&self) -> core::result::Result<&'a QueryValue, DeError> {
        match self.params {
            [(_, value)] => Ok(value),
            other => Err(DeError::arity(format!(
                "the route captures {} path parameters, but the handler reads a single value",
                other.len()
            ))),
        }
    }
}

macro_rules! forward_only {
    ($($method:ident),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> core::result::Result<V::Value, DeError> {
            let value = self.only()?;
            ValueDeserializer::new(value, self.options).$method(visitor)
        }
    )*};
}

impl<'de, 'a> Deserializer<'de> for PathDeserializer<'a> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        if self.params.len() == 1 {
            let value = self.only()?;
            return ValueDeserializer::new(value, self.options).deserialize_any(visitor);
        }
        self.deserialize_map(visitor)
    }

    forward_only!(
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
        deserialize_option,
    );

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
        visitor.visit_seq(CaptureSeq {
            iter: self.params.iter(),
            options: self.options,
        })
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
        MapDeserializer::new(self.params, self.options).deserialize_map(visitor)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        MapDeserializer::new(self.params, self.options).deserialize_struct(name, fields, visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        let value = self.only()?;
        ValueDeserializer::new(value, self.options).deserialize_enum(name, variants, visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> core::result::Result<V::Value, DeError> {
        visitor.visit_unit()
    }
}

struct CaptureSeq<'a> {
    iter: core::slice::Iter<'a, (String, QueryValue)>,
    options: DeOptions,
}

impl<'de, 'a> serde::de::SeqAccess<'de> for CaptureSeq<'a> {
    type Error = DeError;

    fn next_element_seed<T: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> core::result::Result<Option<T::Value>, DeError> {
        match self.iter.next() {
            Some((_, value)) => seed
                .deserialize(ValueDeserializer::new(value, self.options))
                .map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

/// Extract the parameter names a path template declares, in order.
///
/// `/users/{id}/posts/{slug}` yields `["id", "slug"]`; `/files/{*rest}` yields
/// `["rest"]`. Used by the router to name [`Path`]'s positional placeholders
/// and to detect a mismatch against a struct's fields.
pub fn template_parameters(path: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('}') else {
            break;
        };
        let name = rest[..close].trim_start_matches('*');
        if !name.is_empty() {
            names.push(name);
        }
        rest = &rest[close + 1..];
    }
    names
}

/// Whether a path uses the pre-0.8 Axum / Actix parameter syntax.
///
/// `:id` and a bare `*rest` are rejected: Moso uses OpenAPI-style `{id}` and
/// `{*rest}` everywhere, so a path in a route table, in the document and in a
/// generated client are the same string.
///
/// Implemented rather than deferred because it must run in a `const` context,
/// where `todo!()` is not callable — and because the byte scan is shorter than
/// the comment explaining why it was deferred.
pub const fn has_legacy_syntax(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let starts_segment = index == 0 || bytes[index - 1] == b'/';
        if starts_segment && (bytes[index] == b':' || bytes[index] == b'*') {
            return true;
        }
        index += 1;
    }
    false
}

/// Compile-time rejection of a legacy path.
///
/// `Router::get` takes `&'static str` precisely so this can run in a `const`
/// context, turning a routing mistake into a compile error rather than a
/// 404 nobody notices until staging.
pub const fn assert_modern_path(path: &'static str) -> &'static str {
    if has_legacy_syntax(path) {
        panic!("legacy path parameter syntax: Moso uses OpenAPI-style braces, not a leading colon");
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn captures(pairs: &[(&str, &str)]) -> Vec<(String, QueryValue)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), QueryValue::Scalar((*value).to_owned())))
            .collect()
    }

    #[test]
    fn path_derefs_to_its_payload() {
        let path = Path(42u32);
        assert_eq!(*path, 42);
        assert_eq!(path.into_inner(), 42);
    }

    #[test]
    fn legacy_syntax_is_recognised() {
        assert!(has_legacy_syntax("/users/:id"));
        assert!(has_legacy_syntax(":id"));
        assert!(has_legacy_syntax("/files/*rest"));
    }

    #[test]
    fn openapi_syntax_is_accepted() {
        assert!(!has_legacy_syntax("/users/{id}"));
        assert!(!has_legacy_syntax("/files/{*rest}"));
        assert!(!has_legacy_syntax("/"));
        assert!(!has_legacy_syntax(""));
        assert!(!has_legacy_syntax("/a:b"));
    }

    #[test]
    fn the_const_assertion_runs_at_compile_time() {
        const CHECKED: &str = assert_modern_path("/users/{id}");
        assert_eq!(CHECKED, "/users/{id}");
    }

    #[test]
    fn template_parameters_are_read_in_order() {
        assert_eq!(
            template_parameters("/users/{id}/posts/{slug}"),
            ["id", "slug"]
        );
        assert_eq!(template_parameters("/files/{*rest}"), ["rest"]);
        assert_eq!(template_parameters("/health"), Vec::<&str>::new());
        assert_eq!(template_parameters("/{a}"), ["a"]);
        assert_eq!(template_parameters("/{}"), Vec::<&str>::new());
        assert_eq!(template_parameters("/{unterminated"), Vec::<&str>::new());
    }

    #[test]
    fn a_scalar_reads_the_single_capture() {
        let value: String = deserialize_path(&captures(&[("slug", "hello-world")])).unwrap();
        assert_eq!(value, "hello-world");
        let value: u32 = deserialize_path(&captures(&[("id", "42")])).unwrap();
        assert_eq!(value, 42);
    }

    #[test]
    fn a_tuple_reads_the_captures_positionally() {
        let value: (u32, String) =
            deserialize_path(&captures(&[("post", "7"), ("comment", "abc")])).unwrap();
        assert_eq!(value, (7, "abc".to_owned()));
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Target {
        post: u32,
        comment: String,
    }

    #[test]
    fn a_struct_reads_the_captures_by_name() {
        let value: Target =
            deserialize_path(&captures(&[("post", "7"), ("comment", "abc")])).unwrap();
        assert_eq!(
            value,
            Target {
                post: 7,
                comment: "abc".into()
            }
        );
    }

    #[test]
    fn captures_out_of_declaration_order_still_match_by_name() {
        let value: Target =
            deserialize_path(&captures(&[("comment", "abc"), ("post", "7")])).unwrap();
        assert_eq!(value.post, 7);
    }

    #[test]
    fn a_bad_scalar_is_a_client_error() {
        // A `type` failure becomes a 422: the request named a resource that
        // cannot exist, which is the client's mistake to fix.
        let error = raw_deserialize_path::<u32>(&captures(&[("id", "abc")]))
            .expect_err("`abc` is not a u32");
        assert_eq!(error.inner().kind(), DeErrorKind::Type);
    }

    #[test]
    fn a_name_mismatch_is_an_application_error_naming_the_field() {
        // A `required` failure becomes a 500: no request could have satisfied
        // a handler whose field the route never captures.
        let error = raw_deserialize_path::<Target>(&captures(&[("id", "7"), ("comment", "abc")]))
            .expect_err("`post` is not captured");
        assert_eq!(error.inner().kind(), DeErrorKind::Required);
        assert_eq!(error.inner().field(), Some("post"));
    }

    #[test]
    fn a_capture_count_mismatch_is_an_application_error() {
        let error = raw_deserialize_path::<u32>(&captures(&[("a", "1"), ("b", "2")]))
            .expect_err("two captures cannot become one scalar");
        assert_eq!(error.inner().kind(), DeErrorKind::Required);
    }

    #[test]
    fn path_shapes_compare_against_a_template() {
        let shape = PathShape::Named(vec!["post".into(), "comment".into()]);
        assert_eq!(shape.len(), 2);
        assert!(!shape.is_empty());
        assert_eq!(shape.mismatch(&["post", "comment"]), (vec![], vec![]));
        assert_eq!(
            shape.mismatch(&["post", "id"]),
            (vec!["id".to_owned()], vec!["comment".to_owned()])
        );
        assert_eq!(
            PathShape::Positional(2).mismatch(&["a", "b"]),
            (vec![], vec![])
        );
        assert_eq!(PathShape::Positional(2).names(), None);
    }

    #[test]
    fn shapes_are_read_from_a_schema_node() {
        use moso_schema::json_schema::{JsonType, ObjectBuilder};

        let object = ObjectBuilder::named("Target")
            .property("post", SchemaNode::of_type(JsonType::Integer), true)
            .property("comment", SchemaNode::of_type(JsonType::String), true)
            .build();
        assert_eq!(
            shape_of(&object),
            PathShape::Named(vec!["post".into(), "comment".into()])
        );
        assert_eq!(
            shape_of(&SchemaNode::of_type(JsonType::String)),
            PathShape::Positional(1)
        );
        let mut tuple = SchemaNode::of_type(JsonType::Array);
        tuple.prefix_items = vec![
            SchemaNode::of_type(JsonType::Integer),
            SchemaNode::of_type(JsonType::String),
        ];
        assert_eq!(shape_of(&tuple), PathShape::Positional(2));
    }
}
