//! `Cached<T>` — conditional requests, handled rather than documented away.
//!
//! ```
//! use moso::prelude::*;
//! use moso::deps::http::HeaderMap;
//! use moso::response::{Cached, ETag};
//! use std::time::Duration;
//! # /// A post.
//! # pub struct Post;
//! /// A post, as the API returns one.
//! #[derive(Schema)]
//! pub struct PostOut {
//!     /// URL-safe identifier.
//!     pub slug: Slug,
//!     /// Bumped on every edit.
//!     pub version: u32,
//! }
//! # fn find(_: Id<Post>) -> PostOut {
//! #     PostOut { slug: Slug::from_title("hello").unwrap(), version: 3 }
//! # }
//! /// Show a post, and let a repeat visitor skip the body.
//! #[endpoint]
//! async fn show(Path(id): Path<Id<Post>>, headers: HeaderMap) -> Result<Cached<Json<PostOut>>> {
//!     let post = find(id);
//!     let etag = ETag::strong(post.version);
//!     Ok(Cached::new(Json(post))
//!         .etag(etag)
//!         .max_age(Duration::from_secs(60))
//!         .evaluate(&headers))
//! }
//! # fn main() {
//! // A matching `If-None-Match` turns the response into a bodyless 304.
//! let mut headers = HeaderMap::new();
//! headers.insert("if-none-match", ETag::strong(3).to_header());
//!
//! let cached = Cached::new(Json(find(Id::new())))
//!     .etag(ETag::strong(3))
//!     .evaluate(&headers);
//! assert!(cached.is_not_modified());
//! # }
//! ```
//!
//! [`Cached::evaluate`] compares the `ETag` against `If-None-Match` and the
//! modification time against `If-Modified-Since`, and turns a match into a 304
//! with no body. Doing it here rather than in each handler means the rule that
//! a 304 must not carry a body — and must still carry the caching headers — is
//! implemented once.

use std::time::{Duration, SystemTime};

use http::StatusCode;
use moso_openapi::{Header, OperationBuilder, ResponseSpec};
use moso_schema::json_schema::StringBuilder;

use crate::Response;
use crate::response::{
    Describe, IntoResponse, empty_response, format_http_date, parse_http_date, set_header,
    unix_seconds,
};

/// An entity tag.
///
/// Strong tags mean byte-identical; weak tags mean semantically equivalent.
/// The distinction matters for `Range` requests, which a weak tag may not
/// validate — which is why this is a type rather than a `String`.
///
/// ```
/// use moso::response::ETag;
///
/// let strong = ETag::strong(42);
/// assert_eq!(strong.value(), "42");
/// assert!(!strong.is_weak());
/// assert_eq!(strong.to_header(), "\"42\"");
///
/// // A weak tag compares equal for caching but not for a conditional write.
/// let weak = ETag::weak(42);
/// assert!(weak.is_weak());
/// assert_eq!(weak.to_header(), "W/\"42\"");
///
/// // `*` matches anything the client has.
/// assert!(strong.matches_if_none_match("*"));
/// assert!(strong.matches_if_none_match("\"1\", \"42\""));
/// assert!(!strong.matches_if_none_match("\"1\""));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ETag {
    value: String,
    weak: bool,
}

impl ETag {
    /// A strong tag over an opaque value.
    pub fn strong(value: impl core::fmt::Display) -> Self {
        Self {
            value: value.to_string(),
            weak: false,
        }
    }

    /// A weak tag over an opaque value.
    pub fn weak(value: impl core::fmt::Display) -> Self {
        Self {
            value: value.to_string(),
            weak: true,
        }
    }

    /// A strong tag derived from a body's bytes.
    ///
    /// FNV-1a, rendered as sixteen hex digits. Not a cryptographic digest and
    /// not presented as one: an entity tag is a cache key, and a client that
    /// can choose the body can already choose the tag. What it must be is
    /// *stable* — the same bytes give the same tag on every machine and every
    /// release — and a named non-cryptographic hash is that, where a
    /// `DefaultHasher` is explicitly not.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;

        let mut hash = OFFSET_BASIS;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        Self {
            value: format!("{hash:016x}"),
            weak: false,
        }
    }

    /// Whether this tag is weak.
    pub fn is_weak(&self) -> bool {
        self.weak
    }

    /// The opaque value, without quotes or the `W/` prefix.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Render as a header value: `"abc"` or `W/"abc"`.
    ///
    /// Characters that cannot appear inside a quoted entity tag — a quote, a
    /// backslash, anything outside printable ASCII — are replaced rather than
    /// escaped: an entity tag is opaque, so a substitution changes nothing a
    /// client can observe, and it makes the header unforgeable by construction.
    pub fn to_header(&self) -> http::HeaderValue {
        let mut rendered = String::with_capacity(self.value.len() + 4);
        if self.weak {
            rendered.push_str("W/");
        }
        rendered.push('"');
        for byte in self.value.bytes() {
            match byte {
                b'"' | b'\\' => rendered.push('_'),
                0x21..=0x7e => rendered.push(char::from(byte)),
                _ => rendered.push('_'),
            }
        }
        rendered.push('"');
        // Every byte above is printable ASCII, so this cannot fail; the
        // fallback is an empty tag rather than a panic on a response path.
        http::HeaderValue::from_str(&rendered)
            .unwrap_or_else(|_| http::HeaderValue::from_static("\"\""))
    }

    /// Whether this tag matches an `If-None-Match` header, per RFC 9110's weak
    /// comparison — which is the comparison a 304 decision uses.
    ///
    /// Weak comparison ignores the `W/` prefix on both sides: two
    /// representations that are semantically equivalent are interchangeable for
    /// a cache, which is the whole question a conditional `GET` asks.
    pub fn matches_if_none_match(&self, header: &str) -> bool {
        header.split(',').any(|candidate| {
            let candidate = candidate.trim();
            candidate == "*" || unquote(candidate) == self.value
        })
    }
}

/// Strip a `W/` prefix and the surrounding quotes from one `If-None-Match`
/// entry, leaving the opaque value.
fn unquote(candidate: &str) -> &str {
    let candidate = candidate.strip_prefix("W/").unwrap_or(candidate);
    candidate
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(candidate)
}

/// A response with caching metadata and conditional-request handling.
///
/// ```
/// use moso::prelude::*;
/// use moso::deps::http::HeaderMap;
/// use moso::response::{Cached, ETag};
/// use std::time::Duration;
///
/// /// A post, as the API returns one.
/// #[derive(Schema)]
/// pub struct PostOut {
///     /// Bumped on every edit.
///     pub version: u32,
/// }
///
/// /// Show a post, letting a repeat visitor skip the body.
/// #[endpoint]
/// async fn show(headers: HeaderMap) -> Result<Cached<Json<PostOut>>> {
///     Ok(Cached::new(Json(PostOut { version: 3 }))
///         .etag(ETag::strong(3))
///         .max_age(Duration::from_secs(60))
///         .evaluate(&headers))
/// }
/// # fn main() {
/// let mut headers = HeaderMap::new();
/// headers.insert("if-none-match", ETag::strong(3).to_header());
///
/// let cached = Cached::new(Json(PostOut { version: 3 }))
///     .etag(ETag::strong(3))
///     .evaluate(&headers);
///
/// assert!(cached.is_not_modified());
/// assert_eq!(cached.into_response().status(), 304);
/// # }
/// ```
pub struct Cached<T> {
    body: T,
    etag: Option<ETag>,
    last_modified: Option<SystemTime>,
    max_age: Option<Duration>,
    visibility: Visibility,
    not_modified: bool,
}

/// Whether a shared cache may store the response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Visibility {
    /// `Cache-Control: public` — any cache may store it.
    Public,
    /// `Cache-Control: private` — only the browser may.
    #[default]
    Private,
    /// `Cache-Control: no-store` — nobody may.
    NoStore,
}

impl<T> Cached<T> {
    /// Wrap a response with no caching metadata yet.
    pub fn new(body: T) -> Self {
        Self {
            body,
            etag: None,
            last_modified: None,
            max_age: None,
            visibility: Visibility::default(),
            not_modified: false,
        }
    }

    /// Attach an entity tag.
    pub fn etag(mut self, etag: ETag) -> Self {
        self.etag = Some(etag);
        self
    }

    /// Attach a `Last-Modified` time.
    pub fn last_modified(mut self, at: SystemTime) -> Self {
        self.last_modified = Some(at);
        self
    }

    /// Set `max-age`.
    pub fn max_age(mut self, max_age: Duration) -> Self {
        self.max_age = Some(max_age);
        self
    }

    /// Set the cache visibility.
    pub fn visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// Compare against the request's conditional headers.
    ///
    /// A match arms the 304: the body is dropped at render time and only the
    /// validators and cache directives are sent.
    ///
    /// `If-None-Match` wins outright when it is present, as RFC 9110 §13.1.3
    /// requires — including when it *fails* to match, which is why this does
    /// not fall through to `If-Modified-Since`. A timestamp has one-second
    /// resolution and an entity tag does not; letting the coarser check
    /// override the finer one is how a client ends up caching a stale body it
    /// was explicitly told had changed.
    pub fn evaluate(mut self, headers: &http::HeaderMap) -> Self {
        if let Some(if_none_match) = header_str(headers, http::header::IF_NONE_MATCH) {
            self.not_modified = match &self.etag {
                Some(etag) => etag.matches_if_none_match(if_none_match),
                // `*` means "any current representation", which we have.
                None => if_none_match
                    .split(',')
                    .any(|candidate| candidate.trim() == "*"),
            };
            return self;
        }

        if let (Some(modified), Some(since)) = (
            self.last_modified,
            header_str(headers, http::header::IF_MODIFIED_SINCE).and_then(parse_http_date),
        ) {
            // `Last-Modified` goes out truncated to whole seconds, so compare
            // at that resolution or a response is never once considered fresh.
            self.not_modified = unix_seconds(modified) <= unix_seconds(since);
        }
        self
    }

    /// Whether [`Cached::evaluate`] decided this is a 304.
    pub fn is_not_modified(&self) -> bool {
        self.not_modified
    }

    /// The `Cache-Control` value these directives describe.
    fn cache_control(&self) -> String {
        let mut directives = String::from(match self.visibility {
            Visibility::Public => "public",
            Visibility::Private => "private",
            Visibility::NoStore => "no-store",
        });
        // `max-age` alongside `no-store` is a contradiction; the stricter half
        // wins, because a caching bug that leaks is worse than one that is slow.
        if let (Some(max_age), false) = (self.max_age, self.visibility == Visibility::NoStore) {
            directives.push_str(&format!(", max-age={}", max_age.as_secs()));
        }
        directives
    }
}

/// A header as a `&str`, or `None` when it is absent or not ASCII.
fn header_str(headers: &http::HeaderMap, name: http::HeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

impl<T> core::fmt::Debug for Cached<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cached")
            .field("etag", &self.etag)
            .field("max_age", &self.max_age)
            .field("not_modified", &self.not_modified)
            .finish_non_exhaustive()
    }
}

impl<T: IntoResponse> IntoResponse for Cached<T> {
    fn into_response(self) -> Response {
        // A 304 carries the validators and the cache directives but no body and
        // no `Content-Type` — a client that gets one is meant to re-use what it
        // already has, and a body would be a second copy of the wrong thing.
        // Rendering `T` at all is skipped, so a 304 costs no serialisation.
        let cache_control = self.cache_control();
        let Cached {
            body,
            etag,
            last_modified,
            not_modified,
            ..
        } = self;

        let mut response = if not_modified {
            empty_response(StatusCode::NOT_MODIFIED)
        } else {
            body.into_response()
        };

        if let Some(etag) = &etag {
            response
                .headers_mut()
                .insert(http::header::ETAG, etag.to_header());
        }
        if let Some(modified) = last_modified {
            set_header(
                &mut response,
                http::header::LAST_MODIFIED,
                &format_http_date(modified),
            );
        }
        set_header(&mut response, http::header::CACHE_CONTROL, &cache_control);
        response
    }
}

impl<T: Describe> Describe for Cached<T> {
    fn describe(op: &mut OperationBuilder) {
        <T as Describe>::describe(op);
        op.response(
            200,
            ResponseSpec::empty("The current representation")
                .header_spec("etag", etag_header())
                .header_spec("cache-control", cache_control_header()),
        );
        op.response(
            304,
            ResponseSpec::empty(
                "Not modified. The client's cached copy is current; no body is sent.",
            )
            .header_spec("etag", etag_header()),
        );
    }
}

/// The `ETag` response header, as OpenAPI documents it.
fn etag_header() -> Header {
    Header::new(StringBuilder::new().example("\"a1b2c3d4e5f60718\"").build())
        .with_description("The validator to send back as `If-None-Match`.")
}

/// The `Cache-Control` response header, as OpenAPI documents it.
fn cache_control_header() -> Header {
    Header::new(StringBuilder::new().example("private, max-age=60").build())
        .with_description("How long, and by whom, this representation may be stored.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::Json;
    use crate::response::tests::described;

    fn headers(pairs: &[(http::HeaderName, &str)]) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(name.clone(), http::HeaderValue::from_str(value).unwrap());
        }
        map
    }

    fn header_of(response: &Response, name: http::HeaderName) -> Option<String> {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    #[test]
    fn tags_remember_their_strength() {
        assert!(ETag::weak("v1").is_weak());
        assert!(!ETag::strong("v1").is_weak());
        assert_eq!(ETag::strong("v1").value(), "v1");
    }

    #[test]
    fn visibility_defaults_to_private() {
        assert_eq!(Visibility::default(), Visibility::Private);
    }

    #[test]
    fn tags_render_with_and_without_the_weak_marker() {
        assert_eq!(ETag::strong("v1").to_header(), "\"v1\"");
        assert_eq!(ETag::weak("v1").to_header(), "W/\"v1\"");
    }

    #[test]
    fn a_tag_cannot_break_out_of_its_quotes() {
        assert_eq!(ETag::strong("a\"b\\c").to_header(), "\"a_b_c\"");
        assert_eq!(ETag::strong("a\r\nb").to_header(), "\"a__b\"");
        assert_eq!(ETag::strong("é").to_header(), "\"__\"");
    }

    #[test]
    fn a_byte_derived_tag_is_stable_and_content_addressed() {
        let a = ETag::from_bytes(b"hello");
        assert_eq!(a, ETag::from_bytes(b"hello"));
        assert_ne!(a, ETag::from_bytes(b"hellp"));
        assert!(!a.is_weak());
        assert_eq!(a.value().len(), 16);
        // FNV-1a/64 of "hello", so a change of algorithm is a visible change.
        assert_eq!(a.value(), "a430d84680aabd0b");
    }

    #[test]
    fn if_none_match_uses_weak_comparison() {
        let tag = ETag::strong("v1");
        assert!(tag.matches_if_none_match("\"v1\""));
        assert!(tag.matches_if_none_match("W/\"v1\""));
        assert!(tag.matches_if_none_match("\"v0\", \"v1\""));
        assert!(tag.matches_if_none_match(" \"v0\" , W/\"v1\" "));
        assert!(tag.matches_if_none_match("*"));
        assert!(!tag.matches_if_none_match("\"v2\""));
        assert!(!tag.matches_if_none_match(""));
        // The weak tag with the same value is the same representation.
        assert!(ETag::weak("v1").matches_if_none_match("\"v1\""));
    }

    #[test]
    fn a_matching_if_none_match_arms_a_304() {
        let cached = Cached::new(Json(1u32))
            .etag(ETag::strong("v1"))
            .evaluate(&headers(&[(http::header::IF_NONE_MATCH, "\"v1\"")]));
        assert!(cached.is_not_modified());

        let response = cached.into_response();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            header_of(&response, http::header::ETAG).as_deref(),
            Some("\"v1\"")
        );
        assert!(response.headers().get(http::header::CONTENT_TYPE).is_none());
    }

    #[test]
    fn a_mismatched_if_none_match_sends_the_body() {
        let response = Cached::new(Json(1u32))
            .etag(ETag::strong("v2"))
            .evaluate(&headers(&[(http::header::IF_NONE_MATCH, "\"v1\"")]))
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_of(&response, http::header::ETAG).as_deref(),
            Some("\"v2\"")
        );
    }

    #[test]
    fn if_none_match_wins_over_if_modified_since() {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let request = headers(&[
            (http::header::IF_NONE_MATCH, "\"stale\""),
            (
                http::header::IF_MODIFIED_SINCE,
                &format_http_date(modified + Duration::from_secs(60)),
            ),
        ]);

        // The date alone would say "not modified"; the failed tag match wins.
        let cached = Cached::new(Json(1u32))
            .etag(ETag::strong("fresh"))
            .last_modified(modified)
            .evaluate(&request);
        assert!(!cached.is_not_modified());
    }

    #[test]
    fn if_modified_since_decides_when_no_tag_was_sent() {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);

        let unchanged = Cached::new(Json(1u32))
            .last_modified(modified)
            .evaluate(&headers(&[(
                http::header::IF_MODIFIED_SINCE,
                &format_http_date(modified),
            )]));
        assert!(unchanged.is_not_modified(), "equal times are not modified");

        let changed = Cached::new(Json(1u32))
            .last_modified(modified + Duration::from_secs(1))
            .evaluate(&headers(&[(
                http::header::IF_MODIFIED_SINCE,
                &format_http_date(modified),
            )]));
        assert!(!changed.is_not_modified());

        // An unparseable date proves nothing, so the body is sent.
        let garbage = Cached::new(Json(1u32))
            .last_modified(modified)
            .evaluate(&headers(&[(http::header::IF_MODIFIED_SINCE, "yesterday")]));
        assert!(!garbage.is_not_modified());
    }

    #[test]
    fn a_star_matches_any_representation_even_without_a_tag() {
        let cached =
            Cached::new(Json(1u32)).evaluate(&headers(&[(http::header::IF_NONE_MATCH, "*")]));
        assert!(cached.is_not_modified());

        // But a concrete tag cannot match a response that has none.
        let cached =
            Cached::new(Json(1u32)).evaluate(&headers(&[(http::header::IF_NONE_MATCH, "\"v1\"")]));
        assert!(!cached.is_not_modified());
    }

    #[test]
    fn no_conditional_headers_means_a_plain_200() {
        let response = Cached::new(Json(1u32))
            .etag(ETag::weak("v1"))
            .last_modified(SystemTime::UNIX_EPOCH)
            .max_age(Duration::from_secs(60))
            .evaluate(&http::HeaderMap::new())
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_of(&response, http::header::LAST_MODIFIED).as_deref(),
            Some("Thu, 01 Jan 1970 00:00:00 GMT")
        );
        assert_eq!(
            header_of(&response, http::header::CACHE_CONTROL).as_deref(),
            Some("private, max-age=60")
        );
    }

    #[test]
    fn cache_control_reflects_the_visibility() {
        let render = |cached: Cached<Json<u32>>| {
            header_of(&cached.into_response(), http::header::CACHE_CONTROL).unwrap()
        };
        assert_eq!(render(Cached::new(Json(1u32))), "private");
        assert_eq!(
            render(Cached::new(Json(1u32)).visibility(Visibility::Public)),
            "public"
        );
        assert_eq!(
            render(
                Cached::new(Json(1u32))
                    .visibility(Visibility::Public)
                    .max_age(Duration::from_secs(3600))
            ),
            "public, max-age=3600"
        );
        // `no-store` is never softened by a `max-age`.
        assert_eq!(
            render(
                Cached::new(Json(1u32))
                    .visibility(Visibility::NoStore)
                    .max_age(Duration::from_secs(3600))
            ),
            "no-store"
        );
    }

    #[test]
    fn cached_documents_the_body_at_200_and_an_empty_304() {
        let op = described::<Cached<Json<u32>>>();

        let ok = op.response(200).expect("200 documented");
        assert!(ok.content.contains_key("application/json"));
        assert!(ok.headers.contains_key("etag"));
        assert!(ok.headers.contains_key("cache-control"));

        let not_modified = op.response(304).expect("304 documented");
        assert!(not_modified.content.is_empty(), "a 304 carries no body");
        assert!(not_modified.headers.contains_key("etag"));
    }

    #[test]
    fn debug_never_prints_the_body() {
        let rendered = format!("{:?}", Cached::new("secret").etag(ETag::strong("v1")));
        assert!(rendered.contains("Cached"));
        assert!(!rendered.contains("secret"), "{rendered}");
    }
}
