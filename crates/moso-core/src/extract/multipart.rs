//! `Multipart` — streaming `multipart/form-data` with two caps.
//!
//! Behind the `multipart` cargo feature, which pulls Axum's multipart support
//! and the `multer` parser behind it. Off by default because most APIs never
//! accept a file and every off-by-default dependency is compile time an
//! application does not spend.
//!
//! # Two caps, not one
//!
//! ```text
//! http.multipart_file_max   16 MiB   any single field
//! http.multipart_max        32 MiB   the whole payload
//! ```
//!
//! One cap is not enough. A per-field cap alone lets a client send a thousand
//! fields of fifteen megabytes; a total cap alone lets one field consume the
//! whole budget and starve the rest of the form. Both are enforced *while
//! reading*, so neither is discovered after the bytes are already in memory.
//!
//! ```
//! use moso::prelude::*;
//! use moso::extract::Multipart;
//! use moso::response::NoContent;
//!
//! /// Accept an upload, one field at a time.
//! #[endpoint]
//! async fn upload(mut form: Multipart) -> Result<NoContent> {
//!     while let Some(field) = form.next_field().await? {
//!         let name = field.name().unwrap_or_default().to_owned();
//!         let bytes = field.bytes().await?;
//!         let _ = (name, bytes.len());
//!     }
//!     Ok(NoContent)
//! }
//! # fn main() { assert_eq!(Router::new().post("/upload", moso::ep!(upload)).len(), 1); }
//! ```
//!
//! # What it contributes to the document
//!
//! `requestBody: multipart/form-data` with a free-form object schema. OpenAPI
//! can describe a *typed* multipart body — one property per field — but this
//! extractor is untyped by construction, so claiming a shape would be a lie.
//! A typed upload documents itself properly; this is the escape hatch.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::FromRequest as _;
use moso_openapi::{ContentType, OperationBuilder, ResponseSpec, SchemaNode, SchemaRef};
use moso_schema::json_schema::{AdditionalProperties, JsonType};

use crate::Request;
use crate::ctx::RequestCtx;
use crate::error::{Error, Result};
use crate::extract::ExtractBody;
use crate::extract::body::Bytes;

/// A streaming `multipart/form-data` body.
///
/// Fields arrive in the order the client sent them and each is read on demand,
/// so a large upload never has to fit in memory at once.
///
/// ```
/// use moso::prelude::*;
/// use moso::extract::Multipart;
/// use moso::response::NoContent;
///
/// /// Accept an upload.
/// #[endpoint]
/// async fn upload(mut form: Multipart) -> Result<NoContent> {
///     while let Some(field) = form.next_field().await? {
///         let name = field.name().unwrap_or_default().to_owned();
///         let bytes = field.bytes().await?;
///         let _ = (name, bytes.len());
///     }
///     Ok(NoContent)
/// }
/// # fn main() { assert_eq!(Router::new().post("/upload", moso::ep!(upload)).len(), 1); }
/// ```
///
/// Requires the `multipart` cargo feature. Both caps are enforced as the body is
/// read, so an oversized upload is a `413` after a few kilobytes rather than after
/// the whole thing has been buffered.
#[derive(Debug)]
pub struct Multipart {
    inner: axum::extract::Multipart,
    limits: MultipartLimits,
    consumed: Arc<AtomicUsize>,
}

/// The two caps a multipart body is read under.
///
/// ```
/// use moso::http_config::HttpConfig;
///
/// let limits = HttpConfig::default().limits();
///
/// // A whole payload may be larger than any single file in it.
/// assert!(limits.multipart_max >= limits.multipart_file_max);
/// ```
///
/// Snapshotted from `http.multipart_max` and `http.multipart_file_max` at boot, and
/// carried in every [`Limits`](crate::ctx::Limits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultipartLimits {
    /// The whole payload, summed across every field. `http.multipart_max`.
    pub total: usize,
    /// Any single field. `http.multipart_file_max`.
    pub per_field: usize,
}

impl Multipart {
    /// The next field, or `None` at the end of the body.
    ///
    /// # Errors
    /// 400 for a malformed body; 413 once either cap is exceeded.
    pub async fn next_field(&mut self) -> Result<Option<Field<'_>>> {
        // Cloned before the parser is borrowed, so the two field borrows stay
        // disjoint: `multer` allows only one live field at a time and Axum
        // enforces that with the `&mut Multipart` inside `Field`.
        let consumed = Arc::clone(&self.consumed);
        let limits = self.limits;
        let field = self
            .inner
            .next_field()
            .await
            .map_err(|error| multipart_error(&error))?;
        Ok(field.map(|inner| Field {
            inner,
            limits,
            consumed,
        }))
    }

    /// How many bytes of field content have been read so far.
    pub fn consumed(&self) -> usize {
        self.consumed.load(Ordering::Relaxed)
    }

    /// The caps in force.
    pub fn limits(&self) -> MultipartLimits {
        self.limits
    }
}

/// One field of a [`Multipart`] body.
///
/// Borrows the [`Multipart`] it came from: `multer` supports only one live
/// field at a time, and the borrow is what makes that a compile error rather
/// than a runtime one.
#[derive(Debug)]
pub struct Field<'a> {
    inner: axum::extract::multipart::Field<'a>,
    limits: MultipartLimits,
    consumed: Arc<AtomicUsize>,
}

impl Field<'_> {
    /// The field's form name, if it declared one.
    pub fn name(&self) -> Option<&str> {
        self.inner.name()
    }

    /// The uploaded file's name, if this field is a file.
    ///
    /// Client-supplied and therefore **not** safe to use as a path. Treat it as
    /// a label; generate the storage name yourself.
    pub fn file_name(&self) -> Option<&str> {
        self.inner.file_name()
    }

    /// The field's declared content type.
    pub fn content_type(&self) -> Option<&str> {
        self.inner.content_type()
    }

    /// The field's headers.
    pub fn headers(&self) -> &http::HeaderMap {
        self.inner.headers()
    }

    /// Read the whole field, refusing to buffer past either cap.
    ///
    /// The check runs per chunk, so a field that claims to be 16 MiB and is
    /// actually 16 GiB stops costing memory the moment it passes the cap.
    ///
    /// # Errors
    /// 413 when the field or the payload exceeds its cap; 400 when the body is
    /// malformed or the connection fails.
    pub async fn bytes(mut self) -> Result<Bytes> {
        let mut buffer = bytes::BytesMut::new();
        loop {
            let chunk = self
                .inner
                .chunk()
                .await
                .map_err(|error| multipart_error(&error))?;
            let Some(chunk) = chunk else {
                break;
            };
            if buffer.len().saturating_add(chunk.len()) > self.limits.per_field {
                return Err(Error::payload_too_large(self.limits.per_field));
            }
            let total = self
                .consumed
                .fetch_add(chunk.len(), Ordering::Relaxed)
                .saturating_add(chunk.len());
            if total > self.limits.total {
                return Err(Error::payload_too_large(self.limits.total));
            }
            buffer.extend_from_slice(&chunk);
        }
        Ok(Bytes(buffer.freeze()))
    }

    /// Read the whole field as UTF-8 text, under the same caps.
    ///
    /// # Errors
    /// As [`Field::bytes`], plus a 400 when the content is not UTF-8.
    pub async fn text(self) -> Result<String> {
        let bytes = self.bytes().await?;
        String::from_utf8(bytes.0.to_vec())
            .map_err(|error| Error::bad_request(format!("the field is not valid UTF-8: {error}")))
    }

    /// The next chunk of this field, for a handler streaming to storage.
    ///
    /// Does **not** enforce either cap — a handler that streams has taken
    /// responsibility for its own accounting, which is the whole point of not
    /// buffering. Use [`Field::bytes`] to get the caps back.
    ///
    /// # Errors
    /// 400 when the body is malformed or the connection fails.
    pub async fn chunk(&mut self) -> Result<Option<bytes::Bytes>> {
        self.inner
            .chunk()
            .await
            .map_err(|error| multipart_error(&error))
    }
}

/// Map a multipart parse failure onto the taxonomy.
///
/// Axum's rejection already carries the right status — 400 for a malformed
/// body, 413 for one of `multer`'s own limits — so the status is what decides
/// the kind, exactly as it does for any other Axum rejection.
fn multipart_error(error: &axum::extract::multipart::MultipartError) -> Error {
    let kind = crate::extract::kind_for_status(error.status());
    Error::new(kind).with_detail(error.body_text())
}

impl ExtractBody for Multipart {
    fn describe(op: &mut OperationBuilder) {
        let schema = SchemaNode {
            types: JsonType::Object.into(),
            additional_properties: Some(AdditionalProperties::Any(true)),
            ..SchemaNode::default()
        };
        op.request_body(ContentType::Multipart, SchemaRef::inline(schema), true);
        op.response(
            400,
            ResponseSpec::problem("Malformed multipart/form-data body"),
        );
        op.response(
            413,
            ResponseSpec::problem(
                "A field exceeded `http.multipart_file_max`, or the payload exceeded \
                 `http.multipart_max`",
            ),
        );
    }

    async fn extract_body(req: Request, ctx: &RequestCtx) -> Result<Self> {
        let limits = MultipartLimits {
            total: ctx.limits().multipart_max,
            per_field: ctx.limits().multipart_file_max,
        };
        let inner = axum::extract::Multipart::from_request(req, &())
            .await
            .map_err(crate::extract::axum_rejection)?;
        Ok(Multipart {
            inner,
            limits,
            consumed: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(boundary: &str, parts: &[(&str, &str)]) -> String {
        let mut out = String::new();
        for (name, value) in parts {
            out.push_str(&format!("--{boundary}\r\n"));
            out.push_str(&format!(
                "content-disposition: form-data; name=\"{name}\"\r\n\r\n"
            ));
            out.push_str(value);
            out.push_str("\r\n");
        }
        out.push_str(&format!("--{boundary}--\r\n"));
        out
    }

    async fn multipart(payload: String, boundary: &str, limits: MultipartLimits) -> Multipart {
        let request = http::Request::builder()
            .method(http::Method::POST)
            .header(
                http::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(axum::body::Body::from(payload))
            .expect("a well-formed request");
        // The caps come from the request context in production and from the
        // caller here, so the two limits can be exercised independently.
        let inner = axum::extract::Multipart::from_request(request, &())
            .await
            .expect("the content type declares a boundary");
        Multipart {
            inner,
            limits,
            consumed: Arc::new(AtomicUsize::new(0)),
        }
    }

    const GENEROUS: MultipartLimits = MultipartLimits {
        total: 1024 * 1024,
        per_field: 1024 * 1024,
    };

    #[tokio::test]
    async fn fields_arrive_in_order_with_their_names() {
        let mut form = multipart(
            body("X", &[("title", "Hello"), ("body", "World")]),
            "X",
            GENEROUS,
        )
        .await;
        let first = form.next_field().await.unwrap().expect("a first field");
        assert_eq!(first.name(), Some("title"));
        assert_eq!(first.text().await.unwrap(), "Hello");
        let second = form.next_field().await.unwrap().expect("a second field");
        assert_eq!(second.name(), Some("body"));
        assert_eq!(second.text().await.unwrap(), "World");
        assert!(form.next_field().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_field_over_the_per_field_cap_is_refused() {
        let payload = body("X", &[("big", &"x".repeat(2048))]);
        let mut form = multipart(
            payload,
            "X",
            MultipartLimits {
                total: 1024 * 1024,
                per_field: 1024,
            },
        )
        .await;
        let field = form.next_field().await.unwrap().expect("a field");
        assert!(field.bytes().await.is_err());
    }

    #[tokio::test]
    async fn the_total_cap_is_enforced_across_fields() {
        let payload = body("X", &[("a", &"x".repeat(700)), ("b", &"y".repeat(700))]);
        let mut form = multipart(
            payload,
            "X",
            MultipartLimits {
                total: 1024,
                per_field: 1024,
            },
        )
        .await;
        let first = form.next_field().await.unwrap().expect("a first field");
        assert_eq!(first.bytes().await.unwrap().len(), 700);
        let second = form.next_field().await.unwrap().expect("a second field");
        assert!(
            second.bytes().await.is_err(),
            "the second field pushes the payload past the total cap"
        );
    }

    #[tokio::test]
    async fn consumed_tracks_what_has_been_read() {
        let mut form = multipart(body("X", &[("a", "12345")]), "X", GENEROUS).await;
        assert_eq!(form.consumed(), 0);
        let field = form.next_field().await.unwrap().expect("a field");
        field.bytes().await.unwrap();
        assert_eq!(form.consumed(), 5);
        assert_eq!(form.limits(), GENEROUS);
    }
}
