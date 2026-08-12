//! Raw bodies: bytes, text, streams, and the size cap that guards all of them.
//!
//! # `read_limited` is the load-bearing function
//!
//! Every buffering body extractor goes through [`read_limited`], which stops at
//! the cap **while reading** rather than after. A 100 MiB body against a 2 MiB
//! limit costs two megabytes of memory and one 413 — not a hundred megabytes
//! and then a 413. `Content-Length` is consulted first as a fast rejection, but
//! it is not trusted: a chunked body has none, and a lying one is exactly what
//! an attacker sends.
//!
//! The policy decision and the HTTP mapping are deliberately separate:
//! `read_capped` answers "did this body fit", and [`read_body_limited`] turns
//! the answer into a 413. That split is what lets the cap be tested for what it
//! actually promises — that a hundred-megabyte body never reaches the heap —
//! without going through a request.

use http::header::CONTENT_LENGTH;
use http_body::Body as _;
use http_body_util::BodyExt;
use moso_openapi::{ContentType, OperationBuilder, ResponseSpec, SchemaNode, SchemaRef};
use moso_schema::json_schema::JsonType;

use crate::ctx::RequestCtx;
use crate::error::{Error, Result};
use crate::extract::ExtractBody;
use crate::response::{Describe, IntoResponse};
use crate::{Request, Response};

/// A request or response body as raw bytes.
///
/// A newtype over [`bytes::Bytes`] rather than a re-export, so that
/// `impl ExtractBody` and `impl IntoResponse` belong to this crate and can be
/// documented and diagnosed here.
///
/// The raw body, already buffered under the configured cap. Reach for it when the
/// payload is not JSON and not a form — a webhook whose signature covers the exact
/// bytes, an image upload, a protobuf message.
///
/// ```
/// use moso::prelude::*;
/// use moso::extract::Bytes;
/// use moso::response::NoContent;
///
/// /// Receive a signed webhook.
/// #[endpoint]
/// async fn webhook(body: Bytes) -> Result<NoContent> {
///     // The signature covers the bytes as sent, so they must not be re-encoded.
///     let _ = body.as_slice();
///     Ok(NoContent)
/// }
/// # fn main() {
/// let body = Bytes(bytes::Bytes::from_static(b"{}"));
/// assert_eq!(body.len(), 2);
/// assert!(!body.is_empty());
/// # }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(pub bytes::Bytes);

impl Bytes {
    /// The wrapped buffer.
    pub fn into_inner(self) -> bytes::Bytes {
        self.0
    }

    /// The bytes as a slice.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// How many bytes were read.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the body was empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<bytes::Bytes> for Bytes {
    fn from(bytes: bytes::Bytes) -> Self {
        Bytes(bytes)
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(bytes: Vec<u8>) -> Self {
        Bytes(bytes.into())
    }
}

/// `{"type": "string", "format": "binary"}` — how OpenAPI spells a byte stream.
pub(crate) fn binary_schema() -> SchemaNode {
    SchemaNode::of_type(JsonType::String).with_format("binary")
}

/// `{"type": "string"}`.
pub(crate) fn text_schema() -> SchemaNode {
    SchemaNode::of_type(JsonType::String)
}

impl ExtractBody for Bytes {
    fn describe(op: &mut OperationBuilder) {
        op.request_body(
            ContentType::OctetStream,
            SchemaRef::inline(binary_schema()),
            true,
        );
        op.response(
            413,
            ResponseSpec::problem("The body exceeded `http.body_max`"),
        );
    }

    async fn extract_body(req: Request, ctx: &RequestCtx) -> Result<Self> {
        read_limited(req, ctx.limits().body_max).await
    }
}

impl IntoResponse for Bytes {
    fn into_response(self) -> Response {
        (
            [(
                http::header::CONTENT_TYPE,
                ContentType::OctetStream.as_str(),
            )],
            self.0,
        )
            .into_response()
    }
}

impl Describe for Bytes {
    fn describe(op: &mut OperationBuilder) {
        op.response(
            200,
            ResponseSpec::with_content(ContentType::OctetStream, binary_schema())
                .description("A binary payload"),
        );
    }
}

/// A request or response body as UTF-8 text.
///
/// As a response it is `text/plain; charset=utf-8`, which is why returning a
/// bare `String` from a handler is not supported: `Text(s)` says what you meant,
/// and the diagnostic on the return type points at it.
///
/// The body as UTF-8, buffered under the configured cap. Invalid UTF-8 is a `400`
/// rather than a lossy conversion.
///
/// ```
/// use moso::prelude::*;
/// use moso::extract::Text;
/// use moso::response::NoContent;
///
/// /// Accept a plain-text note.
/// #[endpoint]
/// async fn note(Text(body): Text) -> Result<NoContent> {
///     let _ = body.trim();
///     Ok(NoContent)
/// }
/// # fn main() {
/// let text = Text("hello".to_owned());
/// assert_eq!(text.as_str(), "hello");
/// # }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Text(pub String);

impl Text {
    /// The wrapped string.
    pub fn into_inner(self) -> String {
        self.0
    }

    /// The text as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Text {
    fn from(text: String) -> Self {
        Text(text)
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Text(text.to_owned())
    }
}

impl ExtractBody for Text {
    fn describe(op: &mut OperationBuilder) {
        op.request_body(ContentType::Text, SchemaRef::inline(text_schema()), true);
        op.response(400, ResponseSpec::problem("The body was not valid UTF-8"));
        op.response(
            413,
            ResponseSpec::problem("The body exceeded `http.body_max`"),
        );
    }

    async fn extract_body(req: Request, ctx: &RequestCtx) -> Result<Self> {
        let bytes = read_limited(req, ctx.limits().body_max).await?;
        let text = String::from_utf8(bytes.0.to_vec())
            .map_err(|error| Error::bad_request(format!("the body is not valid UTF-8: {error}")))?;
        Ok(Text(text))
    }
}

impl IntoResponse for Text {
    fn into_response(self) -> Response {
        (
            [(http::header::CONTENT_TYPE, ContentType::Text.as_str())],
            self.0,
        )
            .into_response()
    }
}

impl Describe for Text {
    fn describe(op: &mut OperationBuilder) {
        op.response(200, ResponseSpec::text("A plain-text payload"));
    }
}

/// The request body, unread and undocumented.
///
/// The escape hatch for a handler that wants to hand the body to something else
/// — a proxy, a signature verifier that needs the exact bytes, a protocol Moso
/// has never heard of. Documents itself as `requestBody: {}`, which is the true
/// statement rather than a flattering one.
///
/// The body with nothing done to it: not buffered, not capped, not decoded. Taking
/// one opts out of the body limit, so only use it where the handler enforces its
/// own bound — a proxy that streams straight through, an upload written to disk in
/// chunks.
///
/// ```
/// use moso::prelude::*;
/// use moso::extract::RawBody;
/// use moso::response::NoContent;
///
/// /// Stream an upload straight to storage.
/// #[endpoint]
/// async fn upload(RawBody(body): RawBody) -> Result<NoContent> {
///     // Nothing has been read yet; the handler decides how much it will accept.
///     let _ = body;
///     Ok(NoContent)
/// }
/// # fn main() { assert_eq!(Router::new().post("/upload", moso::ep!(upload)).len(), 1); }
/// ```
#[derive(Debug)]
pub struct RawBody(pub axum::body::Body);

impl RawBody {
    /// The wrapped body.
    pub fn into_inner(self) -> axum::body::Body {
        self.0
    }
}

impl ExtractBody for RawBody {
    fn describe(op: &mut OperationBuilder) {
        op.request_body(
            ContentType::OctetStream,
            SchemaRef::inline(SchemaNode::any()),
            false,
        );
        op.extension("x-moso-raw-body", serde_json::Value::Bool(true));
    }

    async fn extract_body(req: Request, ctx: &RequestCtx) -> Result<Self> {
        let _ = ctx;
        Ok(RawBody(req.into_body()))
    }
}

/// The request body as a stream of chunks, for uploads too large to buffer.
///
/// The per-request cap does not apply — the point is to avoid buffering — so
/// the handler is responsible for its own limit. That responsibility is the
/// price of streaming and the documentation says so plainly.
///
/// The body as a stream of chunks, for a handler that processes as it arrives
/// rather than buffering. Like [`RawBody`], it is uncapped: the handler owns the
/// bound.
///
/// ```
/// use moso::prelude::*;
/// use moso::extract::BodyStream;
/// use moso::response::NoContent;
/// use futures_util::StreamExt;
///
/// /// Count the bytes without ever holding them all.
/// #[endpoint]
/// async fn measure(BodyStream(mut body): BodyStream) -> Result<NoContent> {
///     let mut total = 0_usize;
///     while let Some(chunk) = body.next().await {
///         total += chunk.map_err(|_| Error::bad_request("the body ended early"))?.len();
///     }
///     let _ = total;
///     Ok(NoContent)
/// }
/// # fn main() { assert_eq!(Router::new().post("/measure", moso::ep!(measure)).len(), 1); }
/// ```
#[derive(Debug)]
pub struct BodyStream(pub axum::body::BodyDataStream);

impl BodyStream {
    /// The wrapped stream.
    pub fn into_inner(self) -> axum::body::BodyDataStream {
        self.0
    }
}

impl ExtractBody for BodyStream {
    fn describe(op: &mut OperationBuilder) {
        op.request_body(
            ContentType::OctetStream,
            SchemaRef::inline(binary_schema()),
            true,
        );
        op.extension("x-moso-streaming-body", serde_json::Value::Bool(true));
    }

    async fn extract_body(req: Request, ctx: &RequestCtx) -> Result<Self> {
        let _ = ctx;
        Ok(BodyStream(req.into_body().into_data_stream()))
    }
}

// ---------------------------------------------------------------------------
// The cap
// ---------------------------------------------------------------------------

/// What reading a body under a cap produced.
///
/// Separating this from the 413 is what makes the cap testable for the property
/// it actually promises — see the module header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Capped {
    /// The whole body, which fitted.
    Complete(bytes::Bytes),
    /// The body exceeded the cap and was abandoned. No buffer is returned,
    /// because the point of the cap is that one was never accumulated.
    TooLarge,
}

/// Buffer `body`, abandoning it the moment it exceeds `limit` bytes.
///
/// Peak memory is `limit` plus one frame, never the size of the body: the check
/// runs *before* each frame is appended, so a 100 MiB body against a 1 MiB cap
/// allocates about a megabyte and then stops pulling frames.
///
/// # Errors
/// Returns the transport error if the connection fails mid-body.
pub(crate) async fn read_capped(
    body: axum::body::Body,
    limit: usize,
) -> core::result::Result<Capped, axum::Error> {
    if body.size_hint().lower() > limit as u64 {
        return Ok(Capped::TooLarge);
    }
    // Never pre-allocate the whole limit: a 2 MiB cap must not cost 2 MiB for
    // an empty body. Start small and let `BytesMut` grow into what arrives.
    let hint = usize::try_from(body.size_hint().lower()).unwrap_or(0);
    let mut buffer = bytes::BytesMut::with_capacity(hint.min(limit).min(8 * 1024));
    let mut body = body;
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if buffer.len().saturating_add(data.len()) > limit {
            return Ok(Capped::TooLarge);
        }
        buffer.extend_from_slice(&data);
    }
    Ok(Capped::Complete(buffer.freeze()))
}

/// Read a request body, refusing to buffer more than `limit` bytes.
///
/// Rejects on `Content-Length` first when the header is present and already too
/// large, then enforces the cap chunk by chunk while reading, because a
/// `Content-Length` is a claim and not a fact.
///
/// Returns [`Error::payload_too_large`](crate::Error::payload_too_large) — a
/// 413 problem naming the limit, since a client cannot otherwise discover it.
///
/// # Errors
/// 413 when the body exceeds `limit`; 400 when the connection fails mid-body,
/// which is the client hanging up or a malformed chunked encoding.
pub async fn read_limited(req: Request, limit: usize) -> Result<Bytes> {
    // The `body_limit` layer may have installed a tighter cap than this
    // operation's own. Whichever is smaller is the one that will actually stop
    // the read, so it is the one the client is told about.
    let limit = req
        .extensions()
        .get::<crate::middleware::body_limit::BodyCap>()
        .map_or(limit, |cap| limit.min(cap.0));
    if let Some(declared) = declared_length(req.headers())
        && declared > limit
    {
        return Err(Error::payload_too_large(limit));
    }
    read_body_limited(req.into_body(), limit).await
}

/// Read request *parts* plus a body that has already been split off.
///
/// Same cap and the same error; used by extractors that peeked at the head
/// before deciding how to read.
///
/// # Errors
/// As [`read_limited`].
pub async fn read_body_limited(body: axum::body::Body, limit: usize) -> Result<Bytes> {
    match read_capped(body, limit).await {
        Ok(Capped::Complete(bytes)) => Ok(Bytes(bytes)),
        Ok(Capped::TooLarge) => Err(Error::payload_too_large(limit)),
        // An outer cap that fired first is still "the body was too big", not
        // "the connection broke". The `body_limit` middleware wraps the body in
        // a `Limited`, so an application whose stack cap is tighter than the
        // extractor's would otherwise answer 400 for an oversize body — the one
        // status a client must not have to guess at.
        Err(error) if is_length_limit(&error) => Err(Error::payload_too_large(limit)),
        Err(error) => Err(Error::from(error)),
    }
}

/// Whether a body-stream failure is an outer length cap rather than transport.
///
/// The error arrives wrapped — `axum::Error` over `hyper` over
/// `http_body_util::LengthLimitError` — so the whole source chain is walked
/// rather than just the head.
fn is_length_limit(error: &axum::Error) -> bool {
    let mut source: Option<&(dyn core::error::Error + 'static)> = Some(error);
    while let Some(error) = source {
        if error.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        source = error.source();
    }
    false
}

/// The `Content-Length` a request claims, when it claims one that parses.
fn declared_length(headers: &http::HeaderMap) -> Option<usize> {
    headers
        .get(CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn bytes_wraps_and_unwraps() {
        let value = Bytes::from(vec![1u8, 2, 3]);
        assert_eq!(value.len(), 3);
        assert_eq!(value.as_slice(), &[1, 2, 3]);
        assert!(!value.is_empty());
    }

    #[test]
    fn text_wraps_and_unwraps() {
        let value = Text::from("hello");
        assert_eq!(value.as_str(), "hello");
        assert_eq!(value.into_inner(), "hello");
    }

    #[test]
    fn a_declared_content_length_is_read() {
        let mut headers = http::HeaderMap::new();
        assert_eq!(declared_length(&headers), None);
        headers.insert(CONTENT_LENGTH, http::HeaderValue::from_static("42"));
        assert_eq!(declared_length(&headers), Some(42));
        headers.insert(CONTENT_LENGTH, http::HeaderValue::from_static("huge"));
        assert_eq!(declared_length(&headers), None);
    }

    /// A body that yields `chunks` frames of `chunk` bytes each, counting how
    /// many were actually pulled.
    fn counted_body(chunks: usize, chunk: usize, pulled: Arc<AtomicUsize>) -> axum::body::Body {
        let stream = futures_util::stream::iter((0..chunks).map(move |_| {
            pulled.fetch_add(1, Ordering::Relaxed);
            Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'x'; chunk]))
        }));
        axum::body::Body::from_stream(stream)
    }

    #[tokio::test]
    async fn a_body_within_the_cap_is_returned_whole() {
        let body = axum::body::Body::from("hello");
        assert_eq!(
            read_capped(body, 1024).await.unwrap(),
            Capped::Complete(bytes::Bytes::from_static(b"hello"))
        );
    }

    #[tokio::test]
    async fn a_hundred_megabyte_body_against_a_one_megabyte_cap_is_abandoned_early() {
        const MIB: usize = 1024 * 1024;
        const CHUNK: usize = 64 * 1024;
        const CHUNKS: usize = 100 * MIB / CHUNK;

        let pulled = Arc::new(AtomicUsize::new(0));
        let body = counted_body(CHUNKS, CHUNK, Arc::clone(&pulled));
        assert_eq!(read_capped(body, MIB).await.unwrap(), Capped::TooLarge);

        // The cap is enforced while reading, so at most `limit / chunk` frames
        // plus the one that overflowed are ever pulled — 17 of 1600 here. A cap
        // applied after buffering would have pulled all 1600 and held 100 MiB.
        let pulled = pulled.load(Ordering::Relaxed);
        assert!(
            pulled <= MIB / CHUNK + 1,
            "pulled {pulled} frames, which means the body was buffered before the check"
        );
    }

    #[tokio::test]
    async fn a_declared_content_length_over_the_cap_short_circuits() {
        let body = axum::body::Body::from(vec![b'x'; 4096]);
        assert_eq!(read_capped(body, 1024).await.unwrap(), Capped::TooLarge);
    }

    #[tokio::test]
    async fn a_body_exactly_at_the_cap_fits() {
        let body = axum::body::Body::from(vec![b'x'; 1024]);
        match read_capped(body, 1024).await.unwrap() {
            Capped::Complete(bytes) => assert_eq!(bytes.len(), 1024),
            Capped::TooLarge => panic!("a body exactly at the cap must fit"),
        }
    }

    #[tokio::test]
    async fn the_tighter_of_the_two_caps_is_the_one_reported() {
        // The `body_limit` layer's cap is below this operation's own, so it is
        // the one that will stop the read — and therefore the one the 413 has
        // to name. Reporting the looser number would send the client back to
        // retry at a size that is still refused.
        let mut req = Request::new(axum::body::Body::from(vec![b'x'; 4096]));
        req.headers_mut()
            .insert(CONTENT_LENGTH, http::HeaderValue::from_static("4096"));
        req.extensions_mut()
            .insert(crate::middleware::body_limit::BodyCap(64));

        let error = read_limited(req, 1024 * 1024)
            .await
            .expect_err("4096 bytes against a 64 byte cap");
        assert_eq!(error.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            error.to_string().contains("64"),
            "the 413 must name the cap that fired: {error}"
        );
    }

    #[tokio::test]
    async fn no_outer_cap_leaves_the_operations_own_limit_alone() {
        let req = Request::new(axum::body::Body::from("hello"));
        let bytes = read_limited(req, 1024).await.expect("within the limit");
        assert_eq!(bytes.as_slice(), b"hello");
    }

    #[tokio::test]
    async fn an_outer_length_cap_that_fires_is_a_413_not_a_400() {
        // Exactly the shape the `body_limit` layer produces: an
        // `http_body_util::Limited` re-erased into an `axum::body::Body`. The
        // read fails with a body error, and a body error is a 400 — except this
        // one, which means "too large" and has to say so.
        use http_body_util::Limited;

        let inner = axum::body::Body::from(vec![b'x'; 4096]);
        let body = axum::body::Body::new(Limited::new(inner, 64));

        let error = read_body_limited(body, 1024 * 1024)
            .await
            .expect_err("the outer cap fires first");
        assert_eq!(error.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn a_genuine_transport_failure_is_still_a_400() {
        let stream = futures_util::stream::iter([Err::<bytes::Bytes, std::io::Error>(
            std::io::Error::other("the client hung up"),
        )]);
        let body = axum::body::Body::from_stream(stream);

        let error = read_body_limited(body, 1024)
            .await
            .expect_err("the stream failed");
        assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    }
}
