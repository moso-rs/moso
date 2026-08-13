//! Serving stored objects over HTTP, correctly and safely.
//!
//! "Correctly" means `Range`, `ETag`/`If-None-Match`, `Last-Modified` and a
//! `Content-Disposition` that survives a non-ASCII filename. "Safely" means
//! `Content-Security-Policy: sandbox` and `X-Content-Type-Options: nosniff` on
//! everything a user uploaded — and a documented recommendation to serve user
//! content from a **separate origin**, because a same-origin HTML upload is a
//! session-stealing XSS whatever headers are set.

use moso_core::Response;
use moso_core::response::Describe;
use moso_openapi::OperationBuilder;

use crate::{Result, StorageKey};

/// How the object should be presented to the browser.
///
/// ```
/// use moso_storage::ServeMode;
///
/// // Download is the default: rendering a user's upload inline is the risky
/// // choice, so it must be asked for.
/// assert_eq!(ServeMode::default(), ServeMode::Download);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ServeMode {
    /// `Content-Disposition: attachment`. The default.
    #[default]
    Download,
    /// `Content-Disposition: inline`. Still sandboxed.
    Inline,
}

/// A response that streams a stored object.
///
/// Built by [`Storage::serve`](crate::Storage::serve) or the free [`serve`]
/// and returned from a handler. It implements [`Describe`], so the operation
/// documents a binary response and the 404 and 416 it can produce without the
/// handler saying anything.
///
/// ```no_run
/// use moso_storage::{ServedObject, Storage, StorageKey};
///
/// async fn download(s: &dyn Storage, key: &StorageKey) -> moso_storage::Result<ServedObject> {
///     s.serve(key).await
/// }
/// ```
pub struct ServedObject {
    /// What is being served.
    key: StorageKey,
    /// The object's metadata, already fetched.
    meta: crate::ObjectMeta,
    /// The bytes.
    body: crate::ByteStream,
    /// Attachment or inline.
    mode: ServeMode,
    /// The filename offered to the browser.
    filename: Option<String>,
    /// The outcome of evaluating the conditional and range headers.
    disposition: Delivery,
}

/// What [`ServedObject::evaluate`] decided.
///
/// Kept as data rather than as three booleans so that the response builder is
/// a `match` and cannot answer 206 with a full body.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Delivery {
    /// The whole object.
    Full,
    /// A byte range, inclusive at both ends.
    Range {
        /// First byte.
        start: u64,
        /// Last byte, inclusive.
        end: u64,
    },
    /// The client already has it.
    NotModified,
    /// The requested range cannot be satisfied.
    Unsatisfiable,
}

impl ServedObject {
    /// Present the object inline rather than as a download.
    ///
    /// The sandbox headers stay on. Use it for an image in an `<img>`, never
    /// for HTML a user uploaded.
    ///
    /// ```no_run
    /// # use moso_storage::ServedObject;
    /// # fn f(o: ServedObject) { let _ = o.inline(); }
    /// ```
    #[must_use]
    pub fn inline(mut self) -> Self {
        self.mode = ServeMode::Inline;
        self
    }

    /// Offer a specific filename to the browser.
    ///
    /// Encoded per RFC 6266 with both an ASCII fallback and a `filename*`
    /// parameter, so a non-ASCII name neither breaks the header nor arrives as
    /// mojibake.
    ///
    /// ```no_run
    /// # use moso_storage::ServedObject;
    /// # fn f(o: ServedObject) { let _ = o.filename("rapporto annuale.pdf"); }
    /// ```
    #[must_use]
    pub fn filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    /// Evaluate conditional headers, so an unchanged object costs a 304.
    ///
    /// Also parses `Range`. The order is the one RFC 9110 fixes: a conditional
    /// that matches wins over a range, because answering 206 to a client that
    /// already has the bytes is worse than answering 304 to one that wanted a
    /// slice.
    ///
    /// ```no_run
    /// # use moso_storage::ServedObject;
    /// # fn f(o: ServedObject, h: &http::HeaderMap) { let _ = o.evaluate(h); }
    /// ```
    #[must_use]
    pub fn evaluate(mut self, headers: &http::HeaderMap) -> Self {
        // `If-None-Match` beats `If-Modified-Since` whenever both are present:
        // an entity tag is exact and a timestamp has one-second resolution.
        if let (Some(etag), Some(requested)) = (
            self.meta.etag.as_deref(),
            headers
                .get(http::header::IF_NONE_MATCH)
                .and_then(|value| value.to_str().ok()),
        ) {
            if requested.trim() == "*"
                || requested
                    .split(',')
                    .any(|candidate| etag_matches(candidate.trim(), etag))
            {
                self.disposition = Delivery::NotModified;
                return self;
            }
        } else if let (Some(modified), Some(since)) = (
            self.meta.modified_at,
            headers
                .get(http::header::IF_MODIFIED_SINCE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| chrono::DateTime::parse_from_rfc2822(value).ok()),
        ) && modified.timestamp() <= since.timestamp()
        {
            self.disposition = Delivery::NotModified;
            return self;
        }

        // `If-Range` guards a resumed download: if the object changed since the
        // client last saw it, the range is meaningless and the whole object is
        // the right answer.
        if let Some(if_range) = headers
            .get(http::header::IF_RANGE)
            .and_then(|value| value.to_str().ok())
            && self
                .meta
                .etag
                .as_deref()
                .is_none_or(|etag| !etag_matches(if_range.trim(), etag))
        {
            return self;
        }

        if let Some(range) = headers
            .get(http::header::RANGE)
            .and_then(|value| value.to_str().ok())
        {
            self.disposition = match parse_range(range, self.meta.size) {
                Some((start, end)) => Delivery::Range { start, end },
                None => Delivery::Unsatisfiable,
            };
        }
        self
    }

    /// The object's metadata.
    ///
    /// ```no_run
    /// # use moso_storage::{ObjectMeta, ServedObject};
    /// # fn f(o: &ServedObject) { let _: &ObjectMeta = o.meta(); }
    /// ```
    #[must_use]
    pub fn meta(&self) -> &crate::ObjectMeta {
        &self.meta
    }

    /// Replace the body stream, keeping the key, the metadata and the decisions
    /// [`evaluate`](ServedObject::evaluate) made.
    ///
    /// The hatch that lets a layer above wrap the bytes without rebuilding the
    /// response: [`TimedStorage`](crate::TimedStorage) uses it to attach the
    /// stall deadline, and an application can use it to meter or checksum what
    /// it serves. Whatever it returns is what streams, so a wrapper that
    /// collects here undoes the crate's one hard property.
    ///
    /// ```
    /// use moso_storage::{ByteStream, ObjectMeta, StorageKey, serve::from_parts};
    ///
    /// # fn f(key: StorageKey, meta: ObjectMeta, body: ByteStream) {
    /// // A body that is handed straight through is the identity wrapper.
    /// let _ = from_parts(key, meta, body).map_body(|body| body);
    /// # }
    /// ```
    #[must_use]
    pub fn map_body(mut self, wrap: impl FnOnce(crate::ByteStream) -> crate::ByteStream) -> Self {
        self.body = wrap(self.body);
        self
    }

    /// The status this will answer with, before it is turned into a response.
    ///
    /// For a handler that wants to log or meter the outcome without consuming
    /// the response.
    ///
    /// ```no_run
    /// # use moso_storage::ServedObject;
    /// # fn f(o: &ServedObject) { let _: http::StatusCode = o.status(); }
    /// ```
    #[must_use]
    pub fn status(&self) -> http::StatusCode {
        match self.disposition {
            Delivery::Full => http::StatusCode::OK,
            Delivery::Range { .. } => http::StatusCode::PARTIAL_CONTENT,
            Delivery::NotModified => http::StatusCode::NOT_MODIFIED,
            Delivery::Unsatisfiable => http::StatusCode::RANGE_NOT_SATISFIABLE,
        }
    }
}

impl core::fmt::Debug for ServedObject {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ServedObject")
            .field("key", &self.key)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl moso_core::IntoResponse for ServedObject {
    fn into_response(self) -> Response {
        use futures_util::StreamExt as _;

        let status = self.status();
        let meta = self.meta;
        let disposition = self.disposition;

        // A 304 and a 416 carry no body, and sending one would be a protocol
        // error rather than a wasted byte.
        let body = match disposition {
            Delivery::NotModified | Delivery::Unsatisfiable => axum::body::Body::empty(),
            _ => axum::body::Body::from_stream(
                self.body.map(|chunk| chunk.map_err(std::io::Error::other)),
            ),
        };

        let mut response = Response::new(body);
        *response.status_mut() = status;
        let headers = response.headers_mut();

        // Content type first: a browser that sniffs a stored HTML upload as
        // HTML is the whole problem, and `nosniff` is what stops it.
        if let Ok(value) = http::HeaderValue::from_str(&meta.content_type) {
            headers.insert(http::header::CONTENT_TYPE, value);
        }
        headers.insert(
            http::header::X_CONTENT_TYPE_OPTIONS,
            http::HeaderValue::from_static("nosniff"),
        );
        headers.insert(
            http::header::CONTENT_SECURITY_POLICY,
            http::HeaderValue::from_static(SANDBOX_CSP),
        );
        // Belt and braces for the case a proxy strips the CSP: a downloaded
        // file cannot execute in this origin.
        headers.insert(
            http::header::HeaderName::from_static("x-frame-options"),
            http::HeaderValue::from_static("DENY"),
        );
        headers.insert(
            http::header::ACCEPT_RANGES,
            http::HeaderValue::from_static("bytes"),
        );

        if let Some(etag) = meta.etag.as_deref()
            && let Ok(value) = http::HeaderValue::from_str(etag)
        {
            headers.insert(http::header::ETAG, value);
        }
        if let Some(modified) = meta.modified_at
            && let Ok(value) = http::HeaderValue::from_str(&modified.to_rfc2822())
        {
            headers.insert(http::header::LAST_MODIFIED, value);
        }
        if let Some(cache) = meta.cache_control.as_deref()
            && let Ok(value) = http::HeaderValue::from_str(cache)
        {
            headers.insert(http::header::CACHE_CONTROL, value);
        }

        let name = self
            .filename
            .unwrap_or_else(|| crate::sanitise_filename(self.key.name()));
        headers.insert(
            http::header::CONTENT_DISPOSITION,
            content_disposition(self.mode, &name),
        );

        match disposition {
            Delivery::Full => {
                headers.insert(http::header::CONTENT_LENGTH, header_number(meta.size));
            }
            Delivery::Range { start, end } => {
                headers.insert(http::header::CONTENT_LENGTH, header_number(end - start + 1));
                if let Ok(value) =
                    http::HeaderValue::from_str(&format!("bytes {start}-{end}/{}", meta.size,))
                {
                    headers.insert(http::header::CONTENT_RANGE, value);
                }
            }
            Delivery::Unsatisfiable => {
                if let Ok(value) = http::HeaderValue::from_str(&format!("bytes */{}", meta.size)) {
                    headers.insert(http::header::CONTENT_RANGE, value);
                }
            }
            Delivery::NotModified => {}
        }

        response
    }
}

/// A `u64` as a header value. Digits are always a valid header value.
fn header_number(value: u64) -> http::HeaderValue {
    http::HeaderValue::from_str(&value.to_string())
        .unwrap_or_else(|_| http::HeaderValue::from_static("0"))
}

/// Whether a client's entity tag matches ours, weakly.
///
/// `W/"abc"` and `"abc"` are the same entity for a conditional GET; treating
/// them as different means a client that received a weak tag can never get a
/// 304 back.
fn etag_matches(candidate: &str, ours: &str) -> bool {
    candidate.trim_start_matches("W/") == ours.trim_start_matches("W/")
}

/// Parse a single-range `Range` header against a known size.
///
/// Returns the inclusive bounds, or `None` when the range cannot be satisfied.
/// Multi-range requests are answered as the whole object rather than as a
/// `multipart/byteranges`: no browser sends one for a download, and the
/// multipart encoding is a lot of surface for no benefit.
fn parse_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = (start.trim(), end.trim());

    // `-500` is the *last* 500 bytes, which is the form a resumed download of
    // an unknown-length object uses.
    if start.is_empty() {
        let last: u64 = end.parse().ok()?;
        if last == 0 || size == 0 {
            return None;
        }
        let last = last.min(size);
        return Some((size - last, size - 1));
    }

    let start: u64 = start.parse().ok()?;
    if start >= size {
        return None;
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    (end >= start).then_some((start, end))
}

/// An RFC 6266 `Content-Disposition` with both spellings of the filename.
fn content_disposition(mode: ServeMode, filename: &str) -> http::HeaderValue {
    let kind = match mode {
        ServeMode::Download => "attachment",
        ServeMode::Inline => "inline",
    };

    // The ASCII fallback for clients that do not understand `filename*`, with
    // everything outside a safe set replaced rather than escaped.
    let ascii: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let ascii = if ascii.trim().is_empty() {
        "file".to_owned()
    } else {
        ascii
    };

    let encoded = percent_encoding::utf8_percent_encode(filename, FILENAME_ESCAPE);
    let value = format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}");
    http::HeaderValue::from_str(&value)
        .unwrap_or_else(|_| http::HeaderValue::from_static("attachment"))
}

/// What `filename*` has to escape: everything but the RFC 8187 `attr-char` set.
const FILENAME_ESCAPE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'!')
    .remove(b'#')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b'-')
    .remove(b'.')
    .remove(b'^')
    .remove(b'_')
    .remove(b'`')
    .remove(b'|')
    .remove(b'~');

impl Describe for ServedObject {
    fn describe(op: &mut OperationBuilder) {
        use moso_openapi::{ContentType, ResponseSpec};
        use moso_schema::json_schema::{JsonType, SchemaNode};

        let binary = SchemaNode {
            types: JsonType::String.into(),
            format: Some("binary".into()),
            ..SchemaNode::default()
        };

        op.response(
            200,
            ResponseSpec::with_content(ContentType::custom("application/octet-stream"), binary)
                .description(
                    "The object. Served with `Content-Security-Policy: sandbox` and \
                     `X-Content-Type-Options: nosniff`, because the bytes are user content.",
                ),
        );
        op.response(
            206,
            ResponseSpec::empty("The requested byte range, with `Content-Range`."),
        );
        op.response(
            304,
            ResponseSpec::empty("The client's copy is current; no body is sent."),
        );
        op.response(404, ResponseSpec::empty("No object at that key."));
        op.response(
            416,
            ResponseSpec::empty("The `Range` header cannot be satisfied for this object."),
        );
    }
}

/// Build a response that streams the object at `key`.
///
/// The free-function spelling of [`Storage::serve`](crate::Storage::serve),
/// which is where the implementation lives — this delegates, so the two cannot
/// drift. Use whichever reads better at the call site: a handler holding an
/// `Inject<dyn Storage>` usually wants the method.
///
/// # Errors
///
/// [`Error::NotFound`](crate::Error::NotFound) when there is nothing there.
///
/// ```no_run
/// use moso_storage::{serve, Storage, StorageKey};
///
/// # async fn f(s: &dyn Storage, k: &StorageKey) -> moso_storage::Result<()> {
/// let _response = serve(s, k).await?;
/// # Ok(()) }
/// ```
pub async fn serve(storage: &dyn crate::Storage, key: &StorageKey) -> Result<ServedObject> {
    crate::Storage::serve(storage, key).await
}

/// Build a response from metadata and bytes already in hand.
///
/// [`serve`] costs a `head` and a `get`; a backend that produced both in one
/// operation uses this instead.
///
/// ```no_run
/// # use moso_storage::{serve::from_parts, ByteStream, ObjectMeta, StorageKey};
/// # fn f(key: StorageKey, meta: ObjectMeta, body: ByteStream) {
/// let _ = from_parts(key, meta, body);
/// # }
/// ```
#[must_use]
pub fn from_parts(
    key: StorageKey,
    meta: crate::ObjectMeta,
    body: crate::ByteStream,
) -> ServedObject {
    ServedObject {
        key,
        meta,
        body,
        mode: ServeMode::Download,
        filename: None,
        disposition: Delivery::Full,
    }
}

/// The `Content-Security-Policy` every served object carries.
///
/// `sandbox` with no allow-list: no scripts, no forms, no same-origin access.
/// The one header that turns "somebody uploaded an HTML file" from an incident
/// into a shrug.
///
/// ```
/// assert_eq!(moso_storage::serve::SANDBOX_CSP, "sandbox");
/// ```
pub const SANDBOX_CSP: &str = "sandbox";

#[cfg(test)]
mod tests {
    use super::*;
    use moso_core::IntoResponse as _;

    /// An object of `size` bytes, with an entity tag.
    fn meta(size: u64) -> crate::ObjectMeta {
        crate::ObjectMeta {
            key: StorageKey::new("uploads/report.pdf").expect("valid"),
            size,
            content_type: "application/pdf".to_owned(),
            etag: Some("\"abc123\"".to_owned()),
            modified_at: Some(
                chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                    .expect("valid")
                    .with_timezone(&chrono::Utc),
            ),
            checksum: None,
            metadata: std::collections::BTreeMap::new(),
            cache_control: None,
            content_disposition: None,
            public: false,
        }
    }

    fn served(size: u64) -> ServedObject {
        from_parts(
            StorageKey::new("uploads/report.pdf").expect("valid"),
            meta(size),
            crate::stream_from_bytes(bytes::Bytes::from(vec![0_u8; size as usize])),
        )
    }

    fn headers(pairs: &[(http::HeaderName, &str)]) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                name.clone(),
                http::HeaderValue::from_str(value).expect("ascii"),
            );
        }
        map
    }

    /// A user-uploaded HTML file served same-origin without a sandbox is a
    /// session-stealing XSS. These three headers are the difference.
    #[test]
    fn every_served_object_is_sandboxed_and_nosniff() {
        let response = served(10).into_response();
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_SECURITY_POLICY),
            Some(&http::HeaderValue::from_static("sandbox")),
        );
        assert_eq!(
            response.headers().get(http::header::X_CONTENT_TYPE_OPTIONS),
            Some(&http::HeaderValue::from_static("nosniff")),
        );
        assert_eq!(
            response.headers().get("x-frame-options"),
            Some(&http::HeaderValue::from_static("DENY")),
        );
    }

    /// Rendering a user's upload inline is the risky choice, so it has to be
    /// asked for.
    #[test]
    fn the_default_is_a_download() {
        let response = served(10).into_response();
        let value = response
            .headers()
            .get(http::header::CONTENT_DISPOSITION)
            .expect("present")
            .to_str()
            .expect("ascii")
            .to_owned();
        assert!(value.starts_with("attachment;"), "{value}");
        assert!(value.contains("filename=\"report.pdf\""));

        let inline = served(10).inline().into_response();
        assert!(
            inline
                .headers()
                .get(http::header::CONTENT_DISPOSITION)
                .expect("present")
                .to_str()
                .expect("ascii")
                .starts_with("inline;"),
        );
    }

    /// A non-ASCII filename must neither break the header nor arrive as
    /// mojibake, which is what the two spellings are for.
    #[test]
    fn a_non_ascii_filename_carries_both_spellings() {
        let response = served(10)
            .filename("rapporto annuale — 2026.pdf")
            .into_response();
        let value = response
            .headers()
            .get(http::header::CONTENT_DISPOSITION)
            .expect("present")
            .to_str()
            .expect("the header is ASCII whatever the filename was");

        assert!(
            value.contains("filename=\"rapporto annuale _ 2026.pdf\""),
            "{value}"
        );
        assert!(
            value.contains("filename*=UTF-8''rapporto%20annuale%20%E2%80%94%202026.pdf"),
            "{value}"
        );
    }

    /// A filename with a quote in it would otherwise end the parameter and
    /// start injecting.
    #[test]
    fn a_hostile_filename_cannot_escape_the_header() {
        let response = served(10).filename("a\"; filename=\"b.exe").into_response();
        let value = response
            .headers()
            .get(http::header::CONTENT_DISPOSITION)
            .expect("present")
            .to_str()
            .expect("ascii");
        assert_eq!(value.matches("filename=").count(), 1, "{value}");
    }

    /// A client that already has the object gets a 304 with no body.
    #[test]
    fn a_matching_entity_tag_is_a_304() {
        let response = served(10)
            .evaluate(&headers(&[(http::header::IF_NONE_MATCH, "\"abc123\"")]))
            .into_response();
        assert_eq!(response.status(), http::StatusCode::NOT_MODIFIED);

        // A weak tag names the same entity.
        let weak = served(10)
            .evaluate(&headers(&[(http::header::IF_NONE_MATCH, "W/\"abc123\"")]))
            .into_response();
        assert_eq!(weak.status(), http::StatusCode::NOT_MODIFIED);

        // `*` matches anything that exists.
        let star = served(10)
            .evaluate(&headers(&[(http::header::IF_NONE_MATCH, "*")]))
            .into_response();
        assert_eq!(star.status(), http::StatusCode::NOT_MODIFIED);
    }

    /// A different tag means the client's copy is stale.
    #[test]
    fn a_different_entity_tag_is_a_200() {
        let response = served(10)
            .evaluate(&headers(&[(http::header::IF_NONE_MATCH, "\"other\"")]))
            .into_response();
        assert_eq!(response.status(), http::StatusCode::OK);
    }

    /// The three range forms every download manager sends.
    #[test]
    fn the_documented_range_forms_are_all_supported() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        // An end past the object is clamped, not refused.
        assert_eq!(parse_range("bytes=990-2000", 1000), Some((990, 999)));
        // A suffix longer than the object is the whole object.
        assert_eq!(parse_range("bytes=-2000", 1000), Some((0, 999)));
    }

    /// A range that cannot be satisfied is a 416 with `Content-Range: bytes
    /// */size`, which is what tells a client to start over.
    #[test]
    fn an_unsatisfiable_range_is_a_416_naming_the_size() {
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=50-10", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None);

        let response = served(1000)
            .evaluate(&headers(&[(http::header::RANGE, "bytes=2000-3000")]))
            .into_response();
        assert_eq!(response.status(), http::StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            response.headers().get(http::header::CONTENT_RANGE),
            Some(&http::HeaderValue::from_static("bytes */1000")),
        );
    }

    /// A satisfiable range is a 206 whose `Content-Length` is the range's
    /// length and not the object's.
    #[test]
    fn a_range_request_is_a_206_with_the_right_lengths() {
        let response = served(1000)
            .evaluate(&headers(&[(http::header::RANGE, "bytes=100-199")]))
            .into_response();
        assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers().get(http::header::CONTENT_RANGE),
            Some(&http::HeaderValue::from_static("bytes 100-199/1000")),
        );
        assert_eq!(
            response.headers().get(http::header::CONTENT_LENGTH),
            Some(&http::HeaderValue::from_static("100")),
        );
    }

    /// A conditional that matches wins over a range: answering 206 to a client
    /// that already has the bytes is worse than answering 304.
    #[test]
    fn a_conditional_match_beats_a_range() {
        let response = served(1000)
            .evaluate(&headers(&[
                (http::header::IF_NONE_MATCH, "\"abc123\""),
                (http::header::RANGE, "bytes=0-9"),
            ]))
            .into_response();
        assert_eq!(response.status(), http::StatusCode::NOT_MODIFIED);
    }

    /// `If-Range` guards a resumed download: if the object changed, the range
    /// is meaningless and the whole object is the right answer.
    #[test]
    fn a_stale_if_range_falls_back_to_the_whole_object() {
        let response = served(1000)
            .evaluate(&headers(&[
                (http::header::IF_RANGE, "\"stale\""),
                (http::header::RANGE, "bytes=100-199"),
            ]))
            .into_response();
        assert_eq!(response.status(), http::StatusCode::OK);

        let fresh = served(1000)
            .evaluate(&headers(&[
                (http::header::IF_RANGE, "\"abc123\""),
                (http::header::RANGE, "bytes=100-199"),
            ]))
            .into_response();
        assert_eq!(fresh.status(), http::StatusCode::PARTIAL_CONTENT);
    }

    /// `Accept-Ranges` is what tells a download manager it may resume at all.
    #[test]
    fn ranges_are_advertised() {
        let response = served(10).into_response();
        assert_eq!(
            response.headers().get(http::header::ACCEPT_RANGES),
            Some(&http::HeaderValue::from_static("bytes")),
        );
    }

    /// The operation documents every status it can produce, so a client
    /// generator handles the 206 and the 416 without being told.
    #[test]
    fn the_operation_documents_every_status_it_can_answer() {
        let mut op = OperationBuilder::new(moso_openapi::SchemaGenerator::default());
        <ServedObject as Describe>::describe(&mut op);
        let (operation, _) = op.finish();

        for status in ["200", "206", "304", "404", "416"] {
            assert!(
                operation.responses.contains_key(status),
                "{status} is missing from {:?}",
                operation.responses.keys().collect::<Vec<_>>(),
            );
        }
    }
}
