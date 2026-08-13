//! What an object *is* once it is stored: its metadata, its bytes, and the
//! options that decided how it got there.

use std::collections::BTreeMap;
use std::pin::Pin;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::Stream;
use serde::{Deserialize, Serialize};

use crate::{Result, StorageKey};

/// The bytes of an object, streamed.
///
/// A stream and not a `Vec<u8>`, because the acceptance criterion for this
/// crate is a 1 GiB upload under 20 MiB of peak RSS. Nothing in the put or get
/// path may collect one of these into memory.
///
/// ```no_run
/// use moso_storage::ByteStream;
///
/// fn consume(stream: ByteStream) {
///     let _ = stream;
/// }
/// ```
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

/// Build a [`ByteStream`] from bytes already in memory.
///
/// For a small object, a test, or the tail of a multipart upload. Anything
/// large should stream from its source instead.
///
/// ```
/// use futures_util::StreamExt as _;
/// use moso_storage::{stream_from_bytes, ByteStream};
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let mut stream: ByteStream = stream_from_bytes(bytes::Bytes::from_static(b"hello"));
/// assert_eq!(stream.next().await.transpose().expect("ok"), Some(bytes::Bytes::from_static(b"hello")));
/// assert!(stream.next().await.is_none());
/// # }
/// ```
#[must_use]
pub fn stream_from_bytes(bytes: Bytes) -> ByteStream {
    // Empty in, empty out: a zero-length object must produce a stream that
    // ends immediately rather than one empty chunk, because a backend that
    // counts chunks would see one where there is no data.
    if bytes.is_empty() {
        return Box::pin(futures_util::stream::empty());
    }
    Box::pin(futures_util::stream::once(async move { Ok(bytes) }))
}

/// Collect a [`ByteStream`], refusing to exceed `limit` bytes.
///
/// The check is per chunk, so a stream that claims to be small and is not
/// stops costing memory at the first byte past the limit rather than after the
/// whole thing has been buffered. This is the only place in the crate that
/// collects, and every caller has to name a bound.
///
/// # Errors
///
/// [`Error::TooLarge`](crate::Error::TooLarge) at the first byte past `limit`.
///
/// ```
/// use moso_storage::{collect_bounded, stream_from_bytes};
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() -> moso_storage::Result<()> {
/// let bytes = collect_bounded(stream_from_bytes("hello".into()), 1024, "Blob").await?;
/// assert_eq!(bytes.len(), 5);
///
/// let too_big = collect_bounded(stream_from_bytes("hello".into()), 2, "Blob").await;
/// assert!(too_big.is_err());
/// # Ok(()) }
/// ```
pub async fn collect_bounded(
    mut stream: ByteStream,
    limit: u64,
    kind: &'static str,
) -> Result<Bytes> {
    use futures_util::StreamExt as _;

    let mut buffer = bytes::BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if buffer.len() as u64 + chunk.len() as u64 > limit {
            return Err(crate::Error::too_large(kind, limit));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(buffer.freeze())
}

/// A content checksum, with the algorithm that produced it.
///
/// ```
/// use moso_storage::Checksum;
///
/// let c = Checksum::sha256("e3b0c442");
/// assert_eq!(c.algorithm(), "sha256");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checksum {
    /// The algorithm, lowercased: `"sha256"`, `"crc32c"`, `"md5"`.
    algorithm: String,
    /// The digest, hex-encoded.
    digest: String,
}

impl Checksum {
    /// A SHA-256 checksum. The default, and what `put` computes while streaming.
    ///
    /// ```
    /// use moso_storage::Checksum;
    ///
    /// let _ = Checksum::sha256("abc123");
    /// ```
    #[must_use]
    pub fn sha256(digest: impl Into<String>) -> Self {
        Self {
            algorithm: "sha256".to_owned(),
            digest: digest.into(),
        }
    }

    /// A checksum with an arbitrary algorithm name.
    ///
    /// ```
    /// use moso_storage::Checksum;
    ///
    /// let _ = Checksum::new("crc32c", "0f0f0f0f");
    /// ```
    #[must_use]
    pub fn new(algorithm: impl Into<String>, digest: impl Into<String>) -> Self {
        Self {
            algorithm: algorithm.into(),
            digest: digest.into(),
        }
    }

    /// The algorithm.
    ///
    /// ```
    /// use moso_storage::Checksum;
    ///
    /// assert_eq!(Checksum::sha256("x").algorithm(), "sha256");
    /// ```
    #[must_use]
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// The hex digest.
    ///
    /// ```
    /// use moso_storage::Checksum;
    ///
    /// assert_eq!(Checksum::sha256("x").digest(), "x");
    /// ```
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// Everything known about a stored object without reading it.
///
/// ```no_run
/// use moso_storage::ObjectMeta;
///
/// # fn f(m: &ObjectMeta) {
/// let _: u64 = m.size;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ObjectMeta {
    /// Where it is.
    pub key: StorageKey,
    /// How many bytes.
    pub size: u64,
    /// The media type, as decided by
    /// [`sniff`](crate::sniff) at write time — not as the client declared it.
    pub content_type: String,
    /// The entity tag, for conditional requests.
    pub etag: Option<String>,
    /// When it was last written.
    pub modified_at: Option<DateTime<Utc>>,
    /// The content checksum, when the backend computed or returned one.
    pub checksum: Option<Checksum>,
    /// Backend-side metadata, echoed back from [`PutOpts::metadata`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// The `Cache-Control` the object was stored with.
    pub cache_control: Option<String>,
    /// The `Content-Disposition` the object was stored with.
    pub content_disposition: Option<String>,
    /// Whether the backend reports the object as publicly readable.
    pub public: bool,
}

/// One page of a prefix listing.
///
/// ```no_run
/// use moso_storage::Listing;
///
/// # fn f(l: &Listing) {
/// let more = l.cursor.is_some();
/// let _ = more;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Listing {
    /// The objects on this page, in key order.
    pub objects: Vec<ObjectMeta>,
    /// Common prefixes, when the listing was delimited — the "directories".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefixes: Vec<String>,
    /// The cursor for the next page, or `None` at the end.
    pub cursor: Option<String>,
}

/// How the object should be readable once written.
///
/// ```
/// use moso_storage::Visibility;
///
/// // Private is the default, and the docs explain why user content should be
/// // served from a separate origin even when it is public.
/// assert_eq!(Visibility::default(), Visibility::Private);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Reachable only through a signed URL. The default.
    #[default]
    Private,
    /// Reachable by anyone with the URL.
    Public,
}

/// Options for a write.
///
/// ```no_run
/// use moso_storage::{PutOpts, Visibility};
///
/// let opts = PutOpts::new("image/png")
///     .cache_control("public, max-age=31536000, immutable")
///     .visibility(Visibility::Public);
/// assert_eq!(opts.content_type(), "image/png");
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PutOpts {
    /// The media type to store. Overwritten by the sniffer when
    /// [`PutOpts::sniff`] is on, which is the default.
    content_type: String,
    /// Backend-side metadata.
    metadata: BTreeMap<String, String>,
    /// The `Cache-Control` to store.
    cache_control: Option<String>,
    /// The `Content-Disposition` to store.
    content_disposition: Option<String>,
    /// Whether the object is public.
    visibility: Visibility,
    /// The checksum the caller expects, verified while streaming.
    expected_checksum: Option<Checksum>,
    /// Whether to re-derive the content type from the bytes.
    sniff: bool,
    /// Refuse the write when an object already exists at the key.
    if_absent: bool,
}

impl PutOpts {
    /// Options with the declared content type and every default.
    ///
    /// ```
    /// use moso_storage::PutOpts;
    ///
    /// let opts = PutOpts::new("application/pdf");
    /// assert_eq!(opts.content_type(), "application/pdf");
    /// assert!(opts.sniffs(), "sniffing is on unless it is turned off");
    /// ```
    #[must_use]
    pub fn new(content_type: impl Into<String>) -> Self {
        Self {
            content_type: content_type.into(),
            metadata: BTreeMap::new(),
            cache_control: None,
            content_disposition: None,
            visibility: Visibility::Private,
            expected_checksum: None,
            sniff: true,
            if_absent: false,
        }
    }

    /// Set the `Cache-Control` stored with the object.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// let opts = PutOpts::new("text/plain").cache_control("no-store");
    /// assert_eq!(opts.cache_control_value(), Some("no-store"));
    /// ```
    #[must_use]
    pub fn cache_control(mut self, value: impl Into<String>) -> Self {
        self.cache_control = Some(value.into());
        self
    }

    /// Set the `Content-Disposition` stored with the object.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// let _ = PutOpts::new("application/pdf").content_disposition("attachment");
    /// ```
    #[must_use]
    pub fn content_disposition(mut self, value: impl Into<String>) -> Self {
        self.content_disposition = Some(value.into());
        self
    }

    /// Add one backend-side metadata pair.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// let opts = PutOpts::new("text/plain").metadata("uploaded-by", "usr_123");
    /// assert_eq!(opts.metadata_pairs().get("uploaded-by").map(String::as_str), Some("usr_123"));
    /// ```
    #[must_use]
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Make the object public or private.
    ///
    /// ```
    /// # use moso_storage::{PutOpts, Visibility};
    /// let opts = PutOpts::new("image/png").visibility(Visibility::Public);
    /// assert_eq!(opts.visibility_value(), Visibility::Public);
    /// ```
    #[must_use]
    pub fn visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// Verify the content against this checksum while streaming.
    ///
    /// # Errors at write time
    ///
    /// [`Error::Checksum`](crate::Error::Checksum) when the bytes hash to
    /// something else. The partial object is deleted before the error returns.
    ///
    /// ```
    /// # use moso_storage::{Checksum, PutOpts};
    /// let opts = PutOpts::new("text/plain").expect_checksum(Checksum::sha256("ab"));
    /// assert!(opts.expected_checksum().is_some());
    /// ```
    #[must_use]
    pub fn expect_checksum(mut self, checksum: Checksum) -> Self {
        self.expected_checksum = Some(checksum);
        self
    }

    /// Turn magic-byte sniffing off, keeping the declared content type.
    ///
    /// Only for content the application generated itself. Never for an upload:
    /// trusting a client's `Content-Type` is how a `.png` that is really an
    /// HTML document becomes stored XSS.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// assert!(!PutOpts::new("text/csv").trust_content_type().sniffs());
    /// ```
    #[must_use]
    pub fn trust_content_type(mut self) -> Self {
        self.sniff = false;
        self
    }

    /// Refuse the write if something is already at the key.
    ///
    /// Implemented with the backend's conditional write where there is one and
    /// a `head` plus a documented race where there is not —
    /// [`StorageCapabilities::conditional_writes`](crate::StorageCapabilities::conditional_writes)
    /// says which.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// assert!(PutOpts::new("text/plain").if_absent().refuses_overwrite());
    /// ```
    #[must_use]
    pub fn if_absent(mut self) -> Self {
        self.if_absent = true;
        self
    }

    /// The content type as it currently stands.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// assert_eq!(PutOpts::new("image/png").content_type(), "image/png");
    /// ```
    #[must_use]
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Replace the content type, as a backend does after sniffing.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// let mut opts = PutOpts::new("image/png");
    /// opts.set_content_type("text/html");
    /// assert_eq!(opts.content_type(), "text/html");
    /// ```
    pub fn set_content_type(&mut self, content_type: impl Into<String>) {
        self.content_type = content_type.into();
    }

    /// Whether the content type will be re-derived from the bytes.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// assert!(PutOpts::new("image/png").sniffs());
    /// ```
    #[must_use]
    pub fn sniffs(&self) -> bool {
        self.sniff
    }

    /// Whether the write must not overwrite an existing object.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// assert!(!PutOpts::new("image/png").refuses_overwrite());
    /// ```
    #[must_use]
    pub fn refuses_overwrite(&self) -> bool {
        self.if_absent
    }

    /// The `Cache-Control` to store, when one was set.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// assert_eq!(PutOpts::new("image/png").cache_control_value(), None);
    /// ```
    #[must_use]
    pub fn cache_control_value(&self) -> Option<&str> {
        self.cache_control.as_deref()
    }

    /// The `Content-Disposition` to store, when one was set.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// assert_eq!(PutOpts::new("image/png").content_disposition_value(), None);
    /// ```
    #[must_use]
    pub fn content_disposition_value(&self) -> Option<&str> {
        self.content_disposition.as_deref()
    }

    /// The visibility the object gets.
    ///
    /// ```
    /// # use moso_storage::{PutOpts, Visibility};
    /// assert_eq!(PutOpts::new("image/png").visibility_value(), Visibility::Private);
    /// ```
    #[must_use]
    pub fn visibility_value(&self) -> Visibility {
        self.visibility
    }

    /// The checksum the caller expects, when one was set.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// assert!(PutOpts::new("image/png").expected_checksum().is_none());
    /// ```
    #[must_use]
    pub fn expected_checksum(&self) -> Option<&Checksum> {
        self.expected_checksum.as_ref()
    }

    /// The backend-side metadata pairs.
    ///
    /// ```
    /// # use moso_storage::PutOpts;
    /// assert!(PutOpts::new("image/png").metadata_pairs().is_empty());
    /// ```
    #[must_use]
    pub fn metadata_pairs(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }
}

impl Default for PutOpts {
    fn default() -> Self {
        Self::new("application/octet-stream")
    }
}

/// Build the metadata a backend reports back after a write.
///
/// Every backend produces the same shape from the same inputs, so it lives
/// here rather than five times.
pub(crate) fn meta_from(
    key: &StorageKey,
    size: u64,
    opts: &PutOpts,
    checksum: Option<Checksum>,
    etag: Option<String>,
) -> ObjectMeta {
    ObjectMeta {
        key: key.clone(),
        size,
        content_type: opts.content_type().to_owned(),
        etag,
        modified_at: Some(Utc::now()),
        checksum,
        metadata: opts.metadata_pairs().clone(),
        cache_control: opts.cache_control_value().map(str::to_owned),
        content_disposition: opts.content_disposition_value().map(str::to_owned),
        public: opts.visibility_value() == Visibility::Public,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sniffing on by default is the security-relevant default in this crate;
    /// a `PutOpts` that quietly trusted the caller would undo the whole of
    /// `upload`.
    #[test]
    fn the_defaults_are_the_safe_ones() {
        let opts = PutOpts::default();
        assert!(opts.sniffs());
        assert_eq!(opts.visibility_value(), Visibility::Private);
        assert!(!opts.refuses_overwrite());
        assert!(opts.metadata_pairs().is_empty());
        assert_eq!(opts.content_type(), "application/octet-stream");
    }

    /// Every builder sets what it says and nothing else.
    #[test]
    fn the_builders_are_independent() {
        let opts = PutOpts::new("image/png")
            .cache_control("public, max-age=31536000, immutable")
            .content_disposition("inline")
            .metadata("a", "1")
            .metadata("b", "2")
            .visibility(Visibility::Public)
            .expect_checksum(Checksum::sha256("ab"))
            .if_absent();

        assert_eq!(opts.content_type(), "image/png");
        assert_eq!(
            opts.cache_control_value(),
            Some("public, max-age=31536000, immutable")
        );
        assert_eq!(opts.content_disposition_value(), Some("inline"));
        assert_eq!(opts.metadata_pairs().len(), 2);
        assert_eq!(opts.visibility_value(), Visibility::Public);
        assert_eq!(opts.expected_checksum().map(Checksum::digest), Some("ab"));
        assert!(opts.refuses_overwrite());
        assert!(
            opts.sniffs(),
            "the other builders must not turn sniffing off"
        );
    }

    /// A zero-length object streams as nothing, not as one empty chunk.
    #[tokio::test]
    async fn an_empty_object_streams_as_an_empty_stream() {
        use futures_util::StreamExt as _;

        let mut stream = stream_from_bytes(Bytes::new());
        assert!(stream.next().await.is_none());
    }

    /// The one place in the crate that collects has to be bounded, or the
    /// 20 MiB peak-RSS criterion is unenforceable.
    #[tokio::test]
    async fn collecting_stops_at_the_first_byte_past_the_limit() {
        let stream = stream_from_bytes(Bytes::from(vec![0_u8; 4096]));
        let error = collect_bounded(stream, 1024, "Blob")
            .await
            .expect_err("too large");
        assert!(matches!(error, crate::Error::TooLarge { limit: 1024, .. }));
    }
}
