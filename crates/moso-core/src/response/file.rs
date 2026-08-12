//! `File` — streaming a file, with content type, disposition and `Range`.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use moso_openapi::{Header, OperationBuilder, ResponseSpec};
use moso_schema::json_schema::StringBuilder;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::Response;
use crate::error::{Error, Result};
use crate::response::cached::ETag;
use crate::response::{
    Describe, IntoResponse, empty_response, format_http_date, parse_http_date, set_header,
    unix_seconds,
};

/// How much is read from disk per chunk while streaming.
///
/// 64 KiB is the usual sweet spot: large enough that the syscall and the
/// channel hop are amortised, small enough that a thousand concurrent downloads
/// do not add up to a memory problem.
const CHUNK_SIZE: usize = 64 * 1024;

/// How many chunks may be in flight ahead of the socket.
///
/// Bounded on purpose: an unbounded channel would let a fast disk and a slow
/// client turn a download into a memory leak.
const CHUNK_QUEUE: usize = 2;

/// A file response, streamed rather than buffered.
///
/// ```
/// use moso::prelude::*;
/// use moso::deps::http::HeaderMap;
/// use moso::response::File;
/// # fn storage_path(id: u64) -> String { format!("/var/reports/{id}.pdf") }
/// /// Download a stored report.
/// #[endpoint]
/// async fn download(Path(id): Path<u64>, headers: HeaderMap) -> Result<File> {
///     Ok(File::open(storage_path(id)).await?
///         .attachment("report.pdf")
///         .evaluate(&headers))
/// }
/// # fn main() {}
/// ```
///
/// `File::open` fails with a `404` when the path does not exist, so a missing
/// file never becomes a 500.
///
/// Sets `Content-Type` from the extension, `Content-Length` from the metadata,
/// `Last-Modified`, `Accept-Ranges`, `Content-Disposition` and an `ETag`.
/// [`File::evaluate`] adds `Range`, `If-Range` and `If-None-Match` handling, so
/// resumable downloads and video seeking work.
///
/// # Why the conditional handling is a separate call
///
/// [`IntoResponse`] sees only the value, never the request, so a response type
/// cannot read a request header on its own. [`Cached`](crate::response::Cached)
/// has the same shape and the same answer: take the `HeaderMap` in the handler
/// and hand it over. A `File` that never calls [`File::evaluate`] is a plain
/// 200 — correct, just not resumable.
///
/// # The `ETag` is strong, and that is deliberate
///
/// It is derived from the file's length and modification time. RFC 9110 §13.1.5
/// forbids using a *weak* validator to satisfy an `If-Range`, so a weak tag here
/// would quietly disable byte-range resumption, which is the feature this type
/// exists to provide. Length-plus-mtime is the claim every production web
/// server makes, and a rewrite landing in the same nanosecond at the same length
/// is not a case worth breaking downloads over.
///
/// # Path safety
///
/// [`File::open`] does not sanitise its argument — it is a server-side path
/// chosen by the handler. Serving a *client-supplied* path is what
/// [`Router::static_files`](crate::Router::static_files) is for, and it refuses
/// traversal. Building a path by concatenating a request parameter is the bug
/// this note exists to name.
#[derive(Debug)]
pub struct File {
    path: PathBuf,
    content_type: Option<String>,
    disposition: Disposition,
    filename: Option<String>,
    len: u64,
    modified: Option<SystemTime>,
    etag: ETag,
    conditions: Conditions,
}

/// What the request's conditional headers asked for, resolved against this
/// file's validators.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Conditions {
    /// `If-None-Match`/`If-Modified-Since` matched: send a 304.
    not_modified: bool,
    /// What `Range` asked for, once `If-Range` has had its say.
    range: Option<RangeOutcome>,
}

/// The outcome of parsing a `Range` header against a known length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeOutcome {
    /// Serve bytes `start..=end` as a 206.
    Satisfiable {
        /// First byte, inclusive.
        start: u64,
        /// Last byte, inclusive.
        end: u64,
    },
    /// The range names nothing inside the file: a 416.
    Unsatisfiable,
}

impl File {
    /// Open `path`, failing with a 404 if it does not exist and a 500 if it
    /// cannot be read.
    ///
    /// The metadata is read here, so the length, the modification time and the
    /// entity tag are known before the response is rendered — which is what
    /// lets [`File::evaluate`] answer a conditional request without the file's
    /// contents being touched at all.
    ///
    /// # Errors
    /// 404 when the path does not exist, is not readable, or is not a regular
    /// file; 500 for anything else. A missing file and an unreadable one are
    /// deliberately the same answer: distinguishing them tells a client which
    /// paths exist.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        // `std::fs` on the bounded blocking pool rather than `tokio::fs`, whose
        // Cargo feature the workspace does not enable. Same syscall underneath,
        // and this way a burst of stats queues rather than flooding the runtime.
        let probe = path.clone();
        let metadata = crate::task::blocking(move || std::fs::metadata(&probe))
            .await?
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
                    Error::not_found("file")
                }
                _ => Error::internal(error),
            })?;

        if !metadata.is_file() {
            // A directory is not a 500 and not a 403: from the client's point
            // of view there is simply no document at that URL.
            return Err(Error::not_found("file"));
        }

        let len = metadata.len();
        let modified = metadata.modified().ok();
        Ok(Self {
            content_type: None,
            disposition: Disposition::default(),
            filename: None,
            etag: file_etag(len, modified),
            len,
            modified,
            conditions: Conditions::default(),
            path,
        })
    }

    /// Override the content type guessed from the extension.
    pub fn content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// Serve as a download named `filename`.
    ///
    /// The filename is emitted both as a sanitised ASCII `filename=` and as an
    /// RFC 5987 `filename*=`, so non-ASCII names survive and a quote in the
    /// name cannot break out of the header.
    pub fn attachment(mut self, filename: impl Into<String>) -> Self {
        self.disposition = Disposition::Attachment;
        self.filename = Some(filename.into());
        self
    }

    /// Serve for display in the browser rather than as a download.
    pub fn inline(mut self) -> Self {
        self.disposition = Disposition::Inline;
        self
    }

    /// Resolve the request's `If-None-Match`, `If-Modified-Since`, `Range` and
    /// `If-Range` headers against this file.
    ///
    /// Without this call the response is an unconditional 200. With it:
    ///
    /// | Request | Response |
    /// | --- | --- |
    /// | `If-None-Match` matches | `304`, no body |
    /// | `If-Modified-Since` not older than the file | `304`, no body |
    /// | `Range: bytes=0-499` | `206` with `Content-Range` |
    /// | `Range` outside the file | `416` with `Content-Range: bytes */len` |
    /// | `If-Range` that does not match | the `Range` is ignored: a full `200` |
    /// | more than one range | the `Range` is ignored: a full `200` |
    ///
    /// Multipart ranges are deliberately not implemented. RFC 9110 §14.2 lets a
    /// server answer any range request with the whole representation, every
    /// client falls back cleanly, and `multipart/byteranges` exists mostly to be
    /// got wrong.
    pub fn evaluate(mut self, headers: &http::HeaderMap) -> Self {
        self.conditions.not_modified = self.is_not_modified(headers);
        if self.conditions.not_modified {
            return self;
        }
        if !self.if_range_allows(headers) {
            return self;
        }
        self.conditions.range =
            header_str(headers, http::header::RANGE).and_then(|range| parse_range(range, self.len));
        self
    }

    /// The path being served.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file's length in bytes, as of [`File::open`].
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The entity tag this response will send.
    pub fn etag(&self) -> &ETag {
        &self.etag
    }

    /// The `Content-Type` this response will send.
    pub fn media_type(&self) -> &str {
        match &self.content_type {
            Some(content_type) => content_type,
            None => content_type_for(&self.path),
        }
    }

    /// Whether the conditional headers said the client's copy is current.
    fn is_not_modified(&self, headers: &http::HeaderMap) -> bool {
        // As `Cached`: `If-None-Match` is decisive when present, even when it
        // fails to match, because a tag is a finer instrument than a timestamp.
        if let Some(if_none_match) = header_str(headers, http::header::IF_NONE_MATCH) {
            return self.etag.matches_if_none_match(if_none_match);
        }
        match (
            self.modified,
            header_str(headers, http::header::IF_MODIFIED_SINCE).and_then(parse_http_date),
        ) {
            (Some(modified), Some(since)) => unix_seconds(modified) <= unix_seconds(since),
            _ => false,
        }
    }

    /// Whether `If-Range` — if it was sent at all — permits a partial response.
    fn if_range_allows(&self, headers: &http::HeaderMap) -> bool {
        let Some(if_range) = header_str(headers, http::header::IF_RANGE) else {
            return true;
        };
        // A weak validator may never satisfy an `If-Range`: "semantically
        // equivalent" is not enough when the client is going to splice the
        // bytes together.
        if if_range.starts_with("W/") {
            return false;
        }
        if if_range.starts_with('"') {
            return if_range.trim_matches('"') == self.etag.value();
        }
        match (self.modified, parse_http_date(if_range)) {
            (Some(modified), Some(sent)) => unix_seconds(modified) == unix_seconds(sent),
            _ => false,
        }
    }
}

/// How a browser should treat the body.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Disposition {
    /// Render in place when the type allows it.
    #[default]
    Inline,
    /// Save to disk.
    Attachment,
}

impl Disposition {
    /// The token this disposition emits.
    pub const fn as_str(self) -> &'static str {
        match self {
            Disposition::Inline => "inline",
            Disposition::Attachment => "attachment",
        }
    }
}

impl IntoResponse for File {
    /// Streams through Tokio's blocking pool, so this must be called from
    /// inside a Tokio runtime — which every handler is.
    ///
    /// The read deliberately does *not* go through
    /// [`task::blocking`](crate::task::blocking): that pool is bounded, and
    /// holding one of its permits for the whole duration of a download would
    /// let a handful of slow clients starve every other blocking caller.
    fn into_response(self) -> Response {
        let media_type = self.media_type().to_owned();

        let mut response = match self.conditions {
            Conditions {
                not_modified: true, ..
            } => empty_response(http::StatusCode::NOT_MODIFIED),

            Conditions {
                range: Some(RangeOutcome::Unsatisfiable),
                ..
            } => {
                let mut response = empty_response(http::StatusCode::RANGE_NOT_SATISFIABLE);
                set_header(
                    &mut response,
                    http::header::CONTENT_RANGE,
                    &format!("bytes */{}", self.len),
                );
                response
            }

            Conditions {
                range: Some(RangeOutcome::Satisfiable { start, end }),
                ..
            } => {
                let length = end - start + 1;
                let mut response = Response::new(stream_file(self.path.clone(), start, length));
                *response.status_mut() = http::StatusCode::PARTIAL_CONTENT;
                set_header(
                    &mut response,
                    http::header::CONTENT_RANGE,
                    &format!("bytes {start}-{end}/{}", self.len),
                );
                set_header(
                    &mut response,
                    http::header::CONTENT_LENGTH,
                    &length.to_string(),
                );
                set_header(&mut response, http::header::CONTENT_TYPE, &media_type);
                response
            }

            _ => {
                let mut response = Response::new(stream_file(self.path.clone(), 0, self.len));
                set_header(
                    &mut response,
                    http::header::CONTENT_LENGTH,
                    &self.len.to_string(),
                );
                set_header(&mut response, http::header::CONTENT_TYPE, &media_type);
                response
            }
        };

        // Validators and `Accept-Ranges` go on every outcome, the 304 and the
        // 416 included: a client that gets one still needs to know what it is
        // holding and that it may ask for part of it.
        response
            .headers_mut()
            .insert(http::header::ETAG, self.etag.to_header());
        response.headers_mut().insert(
            http::header::ACCEPT_RANGES,
            http::HeaderValue::from_static("bytes"),
        );
        if let Some(modified) = self.modified {
            set_header(
                &mut response,
                http::header::LAST_MODIFIED,
                &format_http_date(modified),
            );
        }
        response.headers_mut().insert(
            http::header::CONTENT_DISPOSITION,
            content_disposition(self.disposition, self.filename.as_deref()),
        );
        response
    }
}

impl Describe for File {
    fn describe(op: &mut OperationBuilder) {
        op.response(
            200,
            ResponseSpec::binary("The file")
                .header_spec("accept-ranges", accept_ranges_header())
                .header_spec("etag", validator_header())
                .header_spec("content-disposition", content_disposition_header()),
        );
        op.response(
            206,
            ResponseSpec::binary("The requested byte range")
                .header_spec("content-range", content_range_header()),
        );
        op.response(
            304,
            ResponseSpec::empty("Not modified. The client's cached copy is current.")
                .header_spec("etag", validator_header()),
        );
        op.response(
            404,
            ResponseSpec::problem("No such file, or it is not readable"),
        );
        op.response(
            416,
            ResponseSpec::problem("The requested range lies outside the file")
                .header_spec("content-range", content_range_header()),
        );
    }
}

/// An in-memory attachment, for a file generated rather than read.
///
/// The CSV export case: the bytes are already in hand and writing them to a
/// temporary file only to stream it back would be theatre.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::Attachment;
///
/// /// Export the ledger.
/// #[endpoint]
/// async fn export() -> Result<Attachment> {
///     Ok(Attachment::csv("ledger.csv", "date,amount\n2026-01-01,10\n"))
/// }
/// # fn main() {
/// let response = Attachment::csv("ledger.csv", "a,b\n").into_response();
/// assert_eq!(response.status(), 200);
/// assert!(response.headers()["content-disposition"]
///     .to_str()
///     .unwrap()
///     .contains("ledger.csv"));
/// # }
/// ```
///
/// For bytes already in memory, unlike [`File`], which streams from disk.
#[derive(Debug, Clone)]
pub struct Attachment {
    /// The bytes to send.
    pub body: bytes::Bytes,
    /// The download filename.
    pub filename: String,
    /// The content type.
    pub content_type: String,
}

impl Attachment {
    /// An attachment with an explicit content type.
    pub fn new(
        filename: impl Into<String>,
        content_type: impl Into<String>,
        body: impl Into<bytes::Bytes>,
    ) -> Self {
        Self {
            body: body.into(),
            filename: filename.into(),
            content_type: content_type.into(),
        }
    }

    /// A `text/csv` attachment.
    pub fn csv(filename: impl Into<String>, body: impl Into<bytes::Bytes>) -> Self {
        Self::new(filename, "text/csv; charset=utf-8", body)
    }
}

impl IntoResponse for Attachment {
    fn into_response(self) -> Response {
        let mut response = Response::new(axum::body::Body::from(self.body));
        set_header(
            &mut response,
            http::header::CONTENT_TYPE,
            &self.content_type,
        );
        response.headers_mut().insert(
            http::header::CONTENT_DISPOSITION,
            content_disposition(Disposition::Attachment, Some(&self.filename)),
        );
        response
    }
}

impl Describe for Attachment {
    fn describe(op: &mut OperationBuilder) {
        op.response(
            200,
            ResponseSpec::binary("A generated file, served as a download")
                .header_spec("content-disposition", content_disposition_header()),
        );
    }
}

/// A strong entity tag over a file's length and modification time.
fn file_etag(len: u64, modified: Option<SystemTime>) -> ETag {
    let stamp = modified
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|delta| delta.as_nanos())
        .unwrap_or(0);
    ETag::strong(format!("{len:x}-{stamp:x}"))
}

/// A header as a `&str`, or `None` when it is absent or not ASCII.
fn header_str(headers: &http::HeaderMap, name: http::HeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

/// Parse a single-range `Range: bytes=…` header against a known length.
///
/// `None` means the header names no range this implementation honours — a unit
/// other than `bytes`, more than one range, or syntactic nonsense — in which
/// case the whole representation is sent, as RFC 9110 §14.2 permits.
fn parse_range(header: &str, len: u64) -> Option<RangeOutcome> {
    let spec = header.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let (first, last) = (first.trim(), last.trim());

    let outcome = match (first.is_empty(), last.is_empty()) {
        // `bytes=-N`: the final N bytes.
        (true, false) => {
            let suffix: u64 = last.parse().ok()?;
            if suffix == 0 || len == 0 {
                RangeOutcome::Unsatisfiable
            } else {
                RangeOutcome::Satisfiable {
                    start: len.saturating_sub(suffix),
                    end: len - 1,
                }
            }
        }
        // `bytes=N-`: from N to the end.
        (false, true) => {
            let start: u64 = first.parse().ok()?;
            if start >= len {
                RangeOutcome::Unsatisfiable
            } else {
                RangeOutcome::Satisfiable {
                    start,
                    end: len - 1,
                }
            }
        }
        // `bytes=N-M`, with M clamped to the last byte.
        (false, false) => {
            let start: u64 = first.parse().ok()?;
            let end: u64 = last.parse().ok()?;
            if start > end || start >= len {
                RangeOutcome::Unsatisfiable
            } else {
                RangeOutcome::Satisfiable {
                    start,
                    end: end.min(len - 1),
                }
            }
        }
        // `bytes=-` names nothing at all.
        (true, true) => return None,
    };
    Some(outcome)
}

/// Stream `length` bytes of `path`, starting at `start`.
///
/// Reading happens on the blocking pool and arrives through a bounded channel,
/// so a large file costs [`CHUNK_SIZE`] × [`CHUNK_QUEUE`] of memory however big
/// it is, and a slow client applies back-pressure to the disk rather than to
/// the heap.
fn stream_file(path: PathBuf, start: u64, length: u64) -> axum::body::Body {
    let (sender, receiver) = tokio::sync::mpsc::channel::<
        core::result::Result<bytes::Bytes, std::io::Error>,
    >(CHUNK_QUEUE);

    tokio::task::spawn_blocking(move || {
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                let _ = sender.blocking_send(Err(error));
                return;
            }
        };
        if start > 0
            && let Err(error) = file.seek(SeekFrom::Start(start))
        {
            let _ = sender.blocking_send(Err(error));
            return;
        }

        let mut remaining = length;
        let mut buffer = vec![0u8; CHUNK_SIZE];
        while remaining > 0 {
            let want = remaining.min(CHUNK_SIZE as u64) as usize;
            match file.read(&mut buffer[..want]) {
                // Short of what the metadata promised: the file was truncated
                // under us. Stopping is the only option left — the framing has
                // already gone out in `Content-Length`.
                Ok(0) => return,
                Ok(read) => {
                    remaining -= read as u64;
                    if sender
                        .blocking_send(Ok(bytes::Bytes::copy_from_slice(&buffer[..read])))
                        .is_err()
                    {
                        // The client hung up; stop reading rather than finish a
                        // download nobody is receiving.
                        return;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = sender.blocking_send(Err(error));
                    return;
                }
            }
        }
    });

    axum::body::Body::from_stream(futures_util::stream::unfold(
        receiver,
        |mut receiver| async move { receiver.recv().await.map(|chunk| (chunk, receiver)) },
    ))
}

/// Guess a content type from a file extension.
///
/// Falls back to `application/octet-stream`, which is the safe answer: a
/// mis-guessed `text/html` is a stored-XSS vector.
///
/// The table is deliberately short. It covers what an API actually serves —
/// documents, images, fonts, the handful of text formats — and does not try to
/// be `/etc/mime.types`. Anything else takes [`File::content_type`], which is
/// one call and cannot be wrong.
pub fn content_type_for(path: &Path) -> &'static str {
    /// `(extension, media type)`, lowercase.
    const TABLE: &[(&str, &str)] = &[
        // text
        ("txt", "text/plain; charset=utf-8"),
        ("md", "text/markdown; charset=utf-8"),
        ("csv", "text/csv; charset=utf-8"),
        ("html", "text/html; charset=utf-8"),
        ("htm", "text/html; charset=utf-8"),
        ("css", "text/css; charset=utf-8"),
        ("js", "text/javascript; charset=utf-8"),
        ("mjs", "text/javascript; charset=utf-8"),
        // structured
        ("json", "application/json"),
        ("map", "application/json"),
        ("yaml", "application/yaml"),
        ("yml", "application/yaml"),
        ("xml", "application/xml"),
        ("wasm", "application/wasm"),
        // documents
        ("pdf", "application/pdf"),
        // images
        ("png", "image/png"),
        ("jpg", "image/jpeg"),
        ("jpeg", "image/jpeg"),
        ("gif", "image/gif"),
        ("webp", "image/webp"),
        ("avif", "image/avif"),
        ("svg", "image/svg+xml"),
        ("ico", "image/x-icon"),
        // fonts
        ("woff", "font/woff"),
        ("woff2", "font/woff2"),
        ("ttf", "font/ttf"),
        ("otf", "font/otf"),
        // audio and video
        ("mp3", "audio/mpeg"),
        ("ogg", "audio/ogg"),
        ("wav", "audio/wav"),
        ("mp4", "video/mp4"),
        ("webm", "video/webm"),
        // archives
        ("zip", "application/zip"),
        ("gz", "application/gzip"),
        ("tar", "application/x-tar"),
    ];

    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return "application/octet-stream";
    };
    let extension = extension.to_ascii_lowercase();
    TABLE
        .iter()
        .find(|(candidate, _)| *candidate == extension)
        .map(|(_, media_type)| *media_type)
        .unwrap_or("application/octet-stream")
}

/// RFC 5987 `attr-char`: everything else is percent-encoded in `filename*=`.
const ATTR_CHAR: &AsciiSet = &NON_ALPHANUMERIC
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

/// Render a `Content-Disposition` header value.
///
/// Emits both `filename=` (ASCII, quoted, sanitised) and `filename*=`
/// (RFC 5987, UTF-8), which is what every browser released this decade wants.
///
/// The ASCII form replaces anything that is not printable ASCII — the quote, the
/// backslash, `/` and `;` included — with `_`. That is not politeness: an
/// unescaped quote ends the parameter and lets a filename inject header
/// parameters, and a path separator lets it choose where it lands on the user's
/// disk.
pub fn content_disposition(disposition: Disposition, filename: Option<&str>) -> http::HeaderValue {
    let Some(filename) = filename else {
        return http::HeaderValue::from_static(match disposition {
            Disposition::Inline => "inline",
            Disposition::Attachment => "attachment",
        });
    };

    let ascii: String = filename
        .chars()
        .map(|c| match c {
            '"' | '\\' | '/' | ';' => '_',
            c if c.is_ascii_graphic() || c == ' ' => c,
            _ => '_',
        })
        .collect();
    let ascii = if ascii.trim().is_empty() {
        String::from("download")
    } else {
        ascii
    };
    let encoded = utf8_percent_encode(filename, ATTR_CHAR).to_string();

    let rendered = format!(
        "{}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}",
        disposition.as_str()
    );
    // Every byte is printable ASCII by construction, so this cannot fail; the
    // fallback is the bare disposition rather than a panic on a response path.
    http::HeaderValue::from_str(&rendered)
        .unwrap_or_else(|_| http::HeaderValue::from_static("attachment"))
}

/// The `Accept-Ranges` response header, as OpenAPI documents it.
fn accept_ranges_header() -> Header {
    Header::new(StringBuilder::new().constant("bytes").build())
        .with_description("Byte ranges are supported; send `Range: bytes=…` to resume.")
}

/// The `Content-Range` response header, as OpenAPI documents it.
fn content_range_header() -> Header {
    Header::new(StringBuilder::new().example("bytes 0-1023/146515").build())
        .with_description("Which bytes this response carries, and the total length.")
        .required()
}

/// The `ETag` response header, as OpenAPI documents it.
fn validator_header() -> Header {
    Header::new(
        StringBuilder::new()
            .example("\"1e240-17c9a4f2b00\"")
            .build(),
    )
    .with_description("The validator to send back as `If-None-Match` or `If-Range`.")
}

/// The `Content-Disposition` response header, as OpenAPI documents it.
fn content_disposition_header() -> Header {
    Header::new(
        StringBuilder::new()
            .example("attachment; filename=\"report.pdf\"")
            .build(),
    )
    .with_description("Whether to display the body or save it, and under what name.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::tests::described;
    use http_body_util::BodyExt;
    use std::io::Write;

    /// A file with `contents`, in a directory removed along with it.
    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(name: &str, contents: &[u8]) -> Self {
            // A counter rather than a clock: tests run in parallel and two of
            // them landing in the same nanosecond would share a directory, at
            // which point one `Drop` deletes the other's file.
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let mut path = std::env::temp_dir();
            path.push(format!("moso-file-test-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir");
            path.push(name);
            let mut file = std::fs::File::create(&path).expect("create");
            file.write_all(contents).expect("write");
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }

    fn headers(pairs: &[(http::HeaderName, String)]) -> http::HeaderMap {
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

    async fn body_of(response: Response) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec()
    }

    #[test]
    fn disposition_defaults_to_inline() {
        assert_eq!(Disposition::default(), Disposition::Inline);
        assert_eq!(Disposition::Attachment.as_str(), "attachment");
    }

    #[test]
    fn csv_attachments_carry_the_charset() {
        let attachment = Attachment::csv("a.csv", "id\n1\n");
        assert!(attachment.content_type.contains("charset=utf-8"));
    }

    #[test]
    fn content_types_come_from_the_extension_and_default_to_octet_stream() {
        assert_eq!(content_type_for(Path::new("a.pdf")), "application/pdf");
        assert_eq!(content_type_for(Path::new("a.PDF")), "application/pdf");
        assert_eq!(content_type_for(Path::new("a.png")), "image/png");
        assert_eq!(
            content_type_for(Path::new("a.txt")),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            content_type_for(Path::new("a.unknown")),
            "application/octet-stream"
        );
        assert_eq!(
            content_type_for(Path::new("no-extension")),
            "application/octet-stream"
        );
    }

    #[test]
    fn a_filename_cannot_inject_header_parameters() {
        let value = content_disposition(Disposition::Attachment, Some(r#"a";x="b.pdf"#));
        let rendered = value.to_str().unwrap();
        assert_eq!(
            rendered,
            "attachment; filename=\"a__x=_b.pdf\"; filename*=UTF-8''a%22%3Bx%3D%22b.pdf"
        );
        assert_eq!(rendered.matches('"').count(), 2, "exactly one quoted value");
    }

    #[test]
    fn a_non_ascii_filename_survives_in_the_extended_form() {
        let value = content_disposition(Disposition::Attachment, Some("rapport été.pdf"));
        let rendered = value.to_str().unwrap();
        assert!(
            rendered.contains("filename=\"rapport _t_.pdf\""),
            "{rendered}"
        );
        assert!(
            rendered.contains("filename*=UTF-8''rapport%20%C3%A9t%C3%A9.pdf"),
            "{rendered}"
        );
    }

    #[test]
    fn a_disposition_without_a_filename_is_the_bare_token() {
        assert_eq!(content_disposition(Disposition::Inline, None), "inline");
        assert_eq!(
            content_disposition(Disposition::Attachment, None),
            "attachment"
        );
        // A name that sanitises away to nothing still leaves something usable.
        assert!(
            content_disposition(Disposition::Attachment, Some("   "))
                .to_str()
                .unwrap()
                .contains("filename=\"download\"")
        );
        // Control characters become placeholders rather than disappearing.
        assert!(
            content_disposition(Disposition::Attachment, Some("\u{1}\u{2}"))
                .to_str()
                .unwrap()
                .contains("filename=\"__\"")
        );
    }

    #[test]
    fn ranges_parse_in_every_form_the_specification_allows() {
        use RangeOutcome::Satisfiable;

        assert_eq!(
            parse_range("bytes=0-499", 1000),
            Some(Satisfiable { start: 0, end: 499 })
        );
        assert_eq!(
            parse_range("bytes=500-", 1000),
            Some(Satisfiable {
                start: 500,
                end: 999
            })
        );
        assert_eq!(
            parse_range("bytes=-500", 1000),
            Some(Satisfiable {
                start: 500,
                end: 999
            })
        );
        // A suffix longer than the file is the whole file, not an error.
        assert_eq!(
            parse_range("bytes=-5000", 1000),
            Some(Satisfiable { start: 0, end: 999 })
        );
        // An end past the last byte is clamped, as RFC 9110 requires.
        assert_eq!(
            parse_range("bytes=900-5000", 1000),
            Some(Satisfiable {
                start: 900,
                end: 999
            })
        );
        assert_eq!(
            parse_range("bytes= 0 - 9 ", 1000),
            Some(Satisfiable { start: 0, end: 9 })
        );
    }

    #[test]
    fn a_range_outside_the_file_is_unsatisfiable_rather_than_ignored() {
        use RangeOutcome::Unsatisfiable;
        assert_eq!(parse_range("bytes=1000-", 1000), Some(Unsatisfiable));
        assert_eq!(parse_range("bytes=1000-2000", 1000), Some(Unsatisfiable));
        assert_eq!(parse_range("bytes=5-1", 1000), Some(Unsatisfiable));
        assert_eq!(parse_range("bytes=-0", 1000), Some(Unsatisfiable));
        assert_eq!(parse_range("bytes=0-0", 0), Some(Unsatisfiable));
    }

    #[test]
    fn an_unhonoured_range_header_falls_back_to_the_whole_file() {
        // Multiple ranges, a unit we do not speak, and syntactic nonsense all
        // mean "send everything", which RFC 9110 §14.2 explicitly permits.
        assert_eq!(parse_range("bytes=0-1,4-5", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
        assert_eq!(parse_range("bytes=abc", 1000), None);
        assert_eq!(parse_range("bytes=-", 1000), None);
        assert_eq!(parse_range("", 1000), None);
        assert_eq!(parse_range("bytes = 0-9", 1000), None);
    }

    #[tokio::test]
    async fn a_missing_file_is_a_404_not_a_500() {
        let error = File::open("/no/such/path/at/all.txt")
            .await
            .expect_err("must not open");
        assert_eq!(error.status(), http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_directory_is_a_404() {
        let error = File::open(std::env::temp_dir())
            .await
            .expect_err("a directory is not a document");
        assert_eq!(error.status(), http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_whole_file_is_a_200_with_its_length_and_type() {
        let temp = TempFile::new("report.pdf", b"0123456789");
        let file = File::open(&temp.path).await.expect("opens");
        assert_eq!(file.len(), 10);
        assert_eq!(file.media_type(), "application/pdf");
        assert_eq!(file.path(), temp.path);
        assert!(!file.is_empty());

        let response = file.into_response();
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(
            header_of(&response, http::header::CONTENT_LENGTH).as_deref(),
            Some("10")
        );
        assert_eq!(
            header_of(&response, http::header::CONTENT_TYPE).as_deref(),
            Some("application/pdf")
        );
        assert_eq!(
            header_of(&response, http::header::ACCEPT_RANGES).as_deref(),
            Some("bytes")
        );
        assert!(header_of(&response, http::header::ETAG).is_some());
        assert!(header_of(&response, http::header::LAST_MODIFIED).is_some());
        assert_eq!(body_of(response).await, b"0123456789");
    }

    #[tokio::test]
    async fn a_range_request_streams_exactly_those_bytes() {
        let temp = TempFile::new("data.bin", b"0123456789");
        let response = File::open(&temp.path)
            .await
            .expect("opens")
            .evaluate(&headers(&[(http::header::RANGE, "bytes=2-5".into())]))
            .into_response();

        assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            header_of(&response, http::header::CONTENT_RANGE).as_deref(),
            Some("bytes 2-5/10")
        );
        assert_eq!(
            header_of(&response, http::header::CONTENT_LENGTH).as_deref(),
            Some("4")
        );
        assert_eq!(body_of(response).await, b"2345");
    }

    #[tokio::test]
    async fn a_suffix_range_streams_the_tail() {
        let temp = TempFile::new("data.bin", b"0123456789");
        let response = File::open(&temp.path)
            .await
            .expect("opens")
            .evaluate(&headers(&[(http::header::RANGE, "bytes=-3".into())]))
            .into_response();

        assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            header_of(&response, http::header::CONTENT_RANGE).as_deref(),
            Some("bytes 7-9/10")
        );
        assert_eq!(body_of(response).await, b"789");
    }

    #[tokio::test]
    async fn an_unsatisfiable_range_is_a_416_that_says_how_long_the_file_is() {
        let temp = TempFile::new("data.bin", b"0123456789");
        let response = File::open(&temp.path)
            .await
            .expect("opens")
            .evaluate(&headers(&[(http::header::RANGE, "bytes=50-60".into())]))
            .into_response();

        assert_eq!(response.status(), http::StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            header_of(&response, http::header::CONTENT_RANGE).as_deref(),
            Some("bytes */10")
        );
        assert!(body_of(response).await.is_empty());
    }

    #[tokio::test]
    async fn a_matching_if_none_match_is_a_304_with_no_body() {
        let temp = TempFile::new("data.bin", b"0123456789");
        let file = File::open(&temp.path).await.expect("opens");
        let tag = file.etag().to_header().to_str().unwrap().to_owned();

        let response = file
            .evaluate(&headers(&[(http::header::IF_NONE_MATCH, tag.clone())]))
            .into_response();
        assert_eq!(response.status(), http::StatusCode::NOT_MODIFIED);
        assert_eq!(
            header_of(&response, http::header::ETAG).as_deref(),
            Some(tag.as_str())
        );
        assert!(response.headers().get(http::header::CONTENT_TYPE).is_none());
        assert!(body_of(response).await.is_empty());
    }

    #[tokio::test]
    async fn a_stale_if_none_match_sends_the_whole_file() {
        let temp = TempFile::new("data.bin", b"0123456789");
        let response = File::open(&temp.path)
            .await
            .expect("opens")
            .evaluate(&headers(&[(
                http::header::IF_NONE_MATCH,
                "\"something-else\"".into(),
            )]))
            .into_response();
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(body_of(response).await, b"0123456789");
    }

    #[tokio::test]
    async fn a_star_if_none_match_matches_because_the_file_exists() {
        let temp = TempFile::new("data.bin", b"01");
        let response = File::open(&temp.path)
            .await
            .expect("opens")
            .evaluate(&headers(&[(http::header::IF_NONE_MATCH, "*".into())]))
            .into_response();
        assert_eq!(response.status(), http::StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn if_range_gates_the_partial_response() {
        let temp = TempFile::new("data.bin", b"0123456789");

        // A matching tag lets the range through.
        let file = File::open(&temp.path).await.expect("opens");
        let tag = file.etag().to_header().to_str().unwrap().to_owned();
        let response = file
            .evaluate(&headers(&[
                (http::header::RANGE, "bytes=0-1".into()),
                (http::header::IF_RANGE, tag),
            ]))
            .into_response();
        assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);

        // A stale one makes it a full 200 instead, which is what lets a client
        // resume safely against a file that changed underneath it.
        let response = File::open(&temp.path)
            .await
            .expect("opens")
            .evaluate(&headers(&[
                (http::header::RANGE, "bytes=0-1".into()),
                (http::header::IF_RANGE, "\"stale\"".into()),
            ]))
            .into_response();
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(body_of(response).await, b"0123456789");

        // And a weak validator may never satisfy an `If-Range`.
        let response = File::open(&temp.path)
            .await
            .expect("opens")
            .evaluate(&headers(&[
                (http::header::RANGE, "bytes=0-1".into()),
                (http::header::IF_RANGE, "W/\"whatever\"".into()),
            ]))
            .into_response();
        assert_eq!(response.status(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn a_file_larger_than_one_chunk_streams_intact() {
        let contents: Vec<u8> = (0..CHUNK_SIZE * 2 + 7).map(|i| (i % 251) as u8).collect();
        let temp = TempFile::new("big.bin", &contents);
        let response = File::open(&temp.path).await.expect("opens").into_response();
        assert_eq!(body_of(response).await, contents);
    }

    #[tokio::test]
    async fn an_attachment_is_served_as_a_download() {
        let temp = TempFile::new("data.bin", b"x");
        let response = File::open(&temp.path)
            .await
            .expect("opens")
            .content_type("application/x-custom")
            .attachment("report.pdf")
            .into_response();

        assert_eq!(
            header_of(&response, http::header::CONTENT_TYPE).as_deref(),
            Some("application/x-custom")
        );
        assert!(
            header_of(&response, http::header::CONTENT_DISPOSITION)
                .is_some_and(|v| v.starts_with("attachment; filename=\"report.pdf\""))
        );
    }

    #[tokio::test]
    async fn inline_is_the_other_disposition() {
        let temp = TempFile::new("data.bin", b"x");
        let response = File::open(&temp.path)
            .await
            .expect("opens")
            .attachment("a.bin")
            .inline()
            .into_response();
        assert!(
            header_of(&response, http::header::CONTENT_DISPOSITION)
                .is_some_and(|v| v.starts_with("inline;"))
        );
    }

    #[tokio::test]
    async fn an_in_memory_attachment_needs_no_disk() {
        let response = Attachment::csv("export.csv", "id\n1\n").into_response();
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(
            header_of(&response, http::header::CONTENT_TYPE).as_deref(),
            Some("text/csv; charset=utf-8")
        );
        assert!(
            header_of(&response, http::header::CONTENT_DISPOSITION)
                .is_some_and(|v| v.contains("filename=\"export.csv\""))
        );
        assert_eq!(body_of(response).await, b"id\n1\n");
    }

    #[test]
    fn file_documents_every_outcome_it_can_produce() {
        let op = described::<File>();
        for status in [200u16, 206, 304, 404, 416] {
            assert!(op.response(status).is_some(), "{status} must be documented");
        }
        assert!(
            op.response(200)
                .is_some_and(|r| r.content.contains_key("application/octet-stream"))
        );
        assert!(
            op.response(206)
                .is_some_and(|r| r.headers.contains_key("content-range"))
        );
        assert!(op.response(304).is_some_and(|r| r.content.is_empty()));
    }

    #[test]
    fn an_attachment_documents_a_binary_download() {
        let op = described::<Attachment>();
        let response = op.response(200).expect("200 documented");
        assert!(response.content.contains_key("application/octet-stream"));
        assert!(response.headers.contains_key("content-disposition"));
    }
}
