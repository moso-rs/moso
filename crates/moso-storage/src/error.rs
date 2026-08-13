//! What can go wrong talking to object storage, and what each failure becomes
//! over HTTP.
//!
//! The variants separate the cases that need different *actions*: fix the key,
//! send a different file, send a smaller one, retry later, fix the deployment.

use std::borrow::Cow;
use std::time::Duration;

/// The result of every fallible operation in this crate.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// A boxed error from a backend, kept as a source without naming its crate.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Something went wrong storing, reading or validating an object.
///
/// ```no_run
/// use moso_storage::Error;
///
/// let err = Error::not_found("uploads/2026/logo.png");
/// assert!(err.is_not_found());
/// ```
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No object at that key.
    #[error("no object at `{key}`")]
    NotFound {
        /// The key that was asked for.
        key: String,
    },

    /// The key is not a legal storage key.
    ///
    /// Rejected before it reaches a backend: a key containing `..`, a leading
    /// `/`, a NUL byte or a control character is a path-traversal attempt on
    /// the local backend and an unpredictable object name on every other.
    #[error("`{key}` is not a valid storage key: {detail}")]
    Key {
        /// The rejected key, verbatim.
        key: String,
        /// Why it was rejected.
        detail: Cow<'static, str>,
    },

    /// The uploaded bytes are not what the client said they were.
    ///
    /// Produced by magic-byte sniffing, not by trusting the declared type or
    /// the file extension.
    #[error("content is `{actual}`, which `{kind}` does not accept")]
    ContentType {
        /// The kind that was being uploaded.
        kind: &'static str,
        /// What the bytes actually are, per the magic-byte sniffer.
        actual: String,
        /// What the kind accepts.
        accepted: &'static [&'static str],
    },

    /// The upload exceeded its size limit.
    ///
    /// Raised at the first offending byte while streaming, never after
    /// buffering the whole body.
    #[error("upload exceeds the {limit}-byte limit for `{kind}`")]
    TooLarge {
        /// The kind that was being uploaded.
        kind: &'static str,
        /// The limit, in bytes.
        limit: u64,
    },

    /// The object's checksum did not match what was expected.
    #[error("checksum mismatch on `{key}`: expected {expected}, got {actual}")]
    Checksum {
        /// The key.
        key: String,
        /// What the caller asked for.
        expected: String,
        /// What the bytes hashed to.
        actual: String,
    },

    /// The backend was unreachable or transiently failed. Retrying may work.
    #[error("{backend} is unavailable: {detail}")]
    Unavailable {
        /// The backend's name, as [`Storage::name`](crate::Storage::name) reports it.
        backend: &'static str,
        /// What the transport reported.
        detail: String,
        /// The source, when the backend had one.
        #[source]
        source: Option<BoxError>,
    },

    /// The backend refused, permanently: no permission, bucket missing, quota.
    #[error("{backend} refused the operation: {detail}")]
    Refused {
        /// The backend's name.
        backend: &'static str,
        /// What the provider said, already redacted of credentials.
        detail: String,
    },

    /// The backend does not support what was asked of it.
    ///
    /// Checked against [`StorageCapabilities`](crate::StorageCapabilities)
    /// rather than discovered at the provider.
    #[error("{backend} does not support {operation}")]
    Unsupported {
        /// The backend's name.
        backend: &'static str,
        /// The operation, e.g. `"presigned_upload"`.
        operation: &'static str,
    },

    /// A call that answers once did not answer inside
    /// [`Deadlines::operation`](crate::Deadlines::operation).
    ///
    /// Only ever produced for a *unary* operation — `head`, `delete`, `list`,
    /// `copy`, `signed_url`, `presigned_upload`, `multipart_start`, `probe`.
    /// A transfer that is moving bytes is never cut off by this; it is bounded
    /// by [`Error::Stalled`] instead.
    #[error("{backend} did not answer {operation} within {after:?}")]
    Timeout {
        /// The backend's name.
        backend: &'static str,
        /// The operation that ran out of time, e.g. `"head"`.
        operation: &'static str,
        /// The deadline that expired.
        after: Duration,
    },

    /// A transfer stopped moving bytes for
    /// [`Deadlines::idle`](crate::Deadlines::idle).
    ///
    /// The deadline that bounds `put`, `get` and `get_range`. It restarts on
    /// every chunk, so a slow-but-progressing gibibyte finishes and a socket
    /// that went quiet does not hold a connection open until the process is
    /// restarted.
    #[error("{backend} stopped making progress on {operation}: no bytes moved for {after:?}")]
    Stalled {
        /// The backend's name.
        backend: &'static str,
        /// The operation that stopped, e.g. `"get"`.
        operation: &'static str,
        /// How long nothing moved before the transfer was abandoned.
        after: Duration,
    },

    /// Configuration is missing or contradictory.
    #[error("storage configuration is invalid: {0}")]
    Config(Cow<'static, str>),
}

impl Error {
    /// An [`Error::NotFound`] for `key`.
    ///
    /// ```
    /// use moso_storage::Error;
    ///
    /// assert!(Error::not_found("a/b").is_not_found());
    /// ```
    pub fn not_found(key: impl Into<String>) -> Self {
        Self::NotFound { key: key.into() }
    }

    /// An [`Error::Key`] naming the key and the rule it broke.
    ///
    /// ```
    /// use moso_storage::Error;
    ///
    /// let err = Error::key("../etc/passwd", "must not contain `..`");
    /// assert!(err.to_string().contains("must not contain"));
    /// ```
    pub fn key(key: impl Into<String>, detail: impl Into<Cow<'static, str>>) -> Self {
        Self::Key {
            key: key.into(),
            detail: detail.into(),
        }
    }

    /// An [`Error::Unavailable`] from a backend that could not reach its store.
    ///
    /// ```
    /// use moso_storage::Error;
    ///
    /// assert!(Error::unavailable("s3", "connection reset", None).retryable());
    /// ```
    pub fn unavailable(
        backend: &'static str,
        detail: impl Into<String>,
        source: Option<BoxError>,
    ) -> Self {
        Self::Unavailable {
            backend,
            detail: detail.into(),
            source,
        }
    }

    /// An [`Error::Refused`]: the backend will refuse this again.
    ///
    /// ```
    /// use moso_storage::Error;
    ///
    /// assert!(!Error::refused("s3", "AccessDenied").retryable());
    /// ```
    pub fn refused(backend: &'static str, detail: impl Into<String>) -> Self {
        Self::Refused {
            backend,
            detail: detail.into(),
        }
    }

    /// An [`Error::Unsupported`], for an operation a backend does not have.
    ///
    /// ```
    /// use moso_storage::Error;
    ///
    /// let err = Error::unsupported("memory", "presigned_upload");
    /// assert_eq!(err.backend(), Some("memory"));
    /// ```
    #[must_use]
    pub const fn unsupported(backend: &'static str, operation: &'static str) -> Self {
        Self::Unsupported { backend, operation }
    }

    /// An [`Error::TooLarge`] for an upload past its limit.
    ///
    /// ```
    /// use moso_storage::Error;
    ///
    /// let err = Error::too_large("Image", 10 * 1024 * 1024);
    /// assert!(err.to_string().contains("10485760"));
    /// ```
    #[must_use]
    pub const fn too_large(kind: &'static str, limit: u64) -> Self {
        Self::TooLarge { kind, limit }
    }

    /// An [`Error::ContentType`] for bytes that are not what they claimed.
    ///
    /// ```
    /// use moso_storage::Error;
    ///
    /// let err = Error::content_type("Image", "application/x-mach-binary", &["image/*"]);
    /// assert!(err.to_string().contains("x-mach-binary"));
    /// ```
    pub fn content_type(
        kind: &'static str,
        actual: impl Into<String>,
        accepted: &'static [&'static str],
    ) -> Self {
        Self::ContentType {
            kind,
            actual: actual.into(),
            accepted,
        }
    }

    /// An [`Error::Config`] naming the field and the fix.
    ///
    /// ```
    /// use moso_storage::Error;
    ///
    /// assert!(!Error::config("`storage.bucket` is required").retryable());
    /// ```
    pub fn config(detail: impl Into<Cow<'static, str>>) -> Self {
        Self::Config(detail.into())
    }

    /// An [`Error::Timeout`]: a unary call ran past its deadline.
    ///
    /// ```
    /// use moso_storage::Error;
    /// use std::time::Duration;
    ///
    /// let err = Error::timed_out("s3", "head", Duration::from_secs(30));
    /// assert!(err.retryable());
    /// ```
    #[must_use]
    pub const fn timed_out(
        backend: &'static str,
        operation: &'static str,
        after: Duration,
    ) -> Self {
        Self::Timeout {
            backend,
            operation,
            after,
        }
    }

    /// An [`Error::Stalled`]: a transfer stopped moving bytes.
    ///
    /// ```
    /// use moso_storage::Error;
    /// use std::time::Duration;
    ///
    /// let err = Error::stalled("s3", "get", Duration::from_secs(30));
    /// assert!(err.retryable());
    /// ```
    #[must_use]
    pub const fn stalled(backend: &'static str, operation: &'static str, after: Duration) -> Self {
        Self::Stalled {
            backend,
            operation,
            after,
        }
    }

    /// Whether this is [`Error::NotFound`].
    ///
    /// ```
    /// use moso_storage::Error;
    ///
    /// assert!(Error::not_found("k").is_not_found());
    /// ```
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }

    /// Whether retrying the same operation could succeed.
    ///
    /// The three transient variants — [`Error::Unavailable`],
    /// [`Error::Timeout`] and [`Error::Stalled`] — are retryable. A refused
    /// operation, a bad key and a rejected content type are all permanent, and
    /// retrying one wastes a request per attempt.
    ///
    /// ```
    /// use moso_storage::Error;
    ///
    /// assert!(!Error::not_found("k").retryable());
    /// assert!(Error::unavailable("s3", "reset", None).retryable());
    /// ```
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Unavailable { .. } | Self::Timeout { .. } | Self::Stalled { .. }
        )
    }

    /// The backend that produced this, when one did.
    ///
    /// ```
    /// use moso_storage::Error;
    ///
    /// assert_eq!(Error::not_found("k").backend(), None);
    /// assert_eq!(Error::refused("s3", "no").backend(), Some("s3"));
    /// ```
    #[must_use]
    pub const fn backend(&self) -> Option<&'static str> {
        match self {
            Self::Unavailable { backend, .. }
            | Self::Refused { backend, .. }
            | Self::Unsupported { backend, .. }
            | Self::Timeout { backend, .. }
            | Self::Stalled { backend, .. } => Some(backend),
            Self::NotFound { .. }
            | Self::Key { .. }
            | Self::ContentType { .. }
            | Self::TooLarge { .. }
            | Self::Checksum { .. }
            | Self::Config(_) => None,
        }
    }
}

/// The JSON pointer an upload failure points a client at.
///
/// One constant rather than a literal in three arms: a client that keys off
/// the pointer keys off one string.
const UPLOAD_POINTER: &str = "/file";

impl From<Error> for moso_core::Error {
    /// A storage failure becomes the HTTP problem it means.
    ///
    /// [`Error::NotFound`] is a 404, [`Error::ContentType`] and
    /// [`Error::TooLarge`] are 422 and 413 with a field pointer at the upload
    /// field, [`Error::Unavailable`] is a 503 marked retryable,
    /// [`Error::Timeout`] and [`Error::Stalled`] are a 504 — the backend is an
    /// upstream and it did not answer — and everything else is a 500 whose
    /// detail is suppressed outside development.
    fn from(error: Error) -> Self {
        use moso_core::ErrorKind;

        let message = error.to_string();
        match error {
            Error::NotFound { key } => moso_core::Error::not_found(key),
            Error::ContentType {
                kind,
                ref actual,
                accepted,
            } => moso_core::Error::new(ErrorKind::Validation)
                .with_detail(message.clone())
                .with_field(
                    UPLOAD_POINTER,
                    "content_type",
                    &format!(
                        "the bytes are `{actual}`; `{kind}` accepts {}",
                        accepted.join(", "),
                    ),
                ),
            Error::TooLarge { kind, limit } => moso_core::Error::new(ErrorKind::PayloadTooLarge)
                .with_detail(message.clone())
                .with_field(
                    UPLOAD_POINTER,
                    "too_large",
                    &format!("`{kind}` accepts at most {limit} bytes"),
                ),
            // A key a client chose is the client's mistake to fix; a key the
            // application generated is a bug, and both read the same way to
            // whoever is holding the error.
            Error::Key { ref detail, .. } => moso_core::Error::new(ErrorKind::Validation)
                .with_detail(message.clone())
                .with_field(UPLOAD_POINTER, "key", detail),
            Error::Unavailable { .. } => moso_core::Error::unavailable(message),
            // 504 rather than 503: the deadline says nothing about whether this
            // instance is healthy, only that the store behind it did not answer,
            // and a load balancer must not take the instance out of rotation for
            // one slow bucket.
            Error::Timeout { .. } | Error::Stalled { .. } => {
                moso_core::Error::new(ErrorKind::GatewayTimeout).with_detail(message)
            }
            Error::Checksum { .. }
            | Error::Refused { .. }
            | Error::Unsupported { .. }
            | Error::Config(_) => moso_core::Error::internal_msg(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Retrying a permanent failure wastes a request per attempt and, for a
    /// 403, fills a log with the same line.
    #[test]
    fn only_an_unavailable_backend_is_worth_retrying() {
        assert!(Error::unavailable("s3", "reset", None).retryable());
        assert!(Error::timed_out("s3", "head", Duration::from_secs(1)).retryable());
        assert!(Error::stalled("s3", "get", Duration::from_secs(1)).retryable());
        assert!(!Error::not_found("k").retryable());
        assert!(!Error::refused("s3", "denied").retryable());
        assert!(!Error::key("../x", "no").retryable());
        assert!(!Error::too_large("Image", 1).retryable());
        assert!(!Error::content_type("Image", "text/html", &["image/*"]).retryable());
        assert!(!Error::unsupported("memory", "presign").retryable());
        assert!(!Error::config("x").retryable());
    }

    /// A missing object is a 404 and not a 500: the caller asked for something
    /// that is not there, which is an answer and not a fault.
    #[test]
    fn a_missing_object_is_a_404() {
        let http: moso_core::Error = Error::not_found("uploads/logo.png").into();
        assert_eq!(http.status(), http::StatusCode::NOT_FOUND);
    }

    /// A rejected upload has to say which field and why, or the client cannot
    /// tell the user anything useful.
    #[test]
    fn a_rejected_content_type_is_a_422_pointing_at_the_upload() {
        let http: moso_core::Error =
            Error::content_type("Image", "application/x-mach-binary", &["image/png"]).into();
        assert_eq!(http.status(), http::StatusCode::UNPROCESSABLE_ENTITY);
        let fields = http.fields().expect("a field pointer");
        assert_eq!(fields.as_slice()[0].pointer, UPLOAD_POINTER);
        assert!(fields.as_slice()[0].message.contains("image/png"));
    }

    /// An oversized upload is a 413, which is the status a client's own
    /// retry-and-resize logic keys off.
    #[test]
    fn an_oversized_upload_is_a_413() {
        let http: moso_core::Error = Error::too_large("Image", 1024).into();
        assert_eq!(http.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// A backend outage is retryable at the HTTP layer too.
    #[test]
    fn an_unreachable_backend_is_a_retryable_503() {
        let http: moso_core::Error = Error::unavailable("s3", "timed out", None).into();
        assert_eq!(http.status(), http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(http.retryable());
    }

    /// A deadline that fired is the *store* failing to answer, not this
    /// instance failing, so it is a 504 and not a 503 — a 503 would take the
    /// instance out of rotation over one slow bucket.
    #[test]
    fn a_deadline_that_fired_is_a_retryable_504() {
        for error in [
            Error::timed_out("s3", "head", Duration::from_secs(30)),
            Error::stalled("s3", "get", Duration::from_secs(30)),
        ] {
            let text = error.to_string();
            let http: moso_core::Error = error.into();
            assert_eq!(http.status(), http::StatusCode::GATEWAY_TIMEOUT, "{text}");
            assert!(http.retryable(), "{text}");
        }
    }

    /// The message has to name which operation ran out of time, or a log line
    /// says only that "storage was slow".
    #[test]
    fn a_deadline_names_the_backend_and_the_operation() {
        let text = Error::timed_out("azure", "list", Duration::from_secs(5)).to_string();
        assert!(text.contains("azure"), "{text}");
        assert!(text.contains("list"), "{text}");

        let text = Error::stalled("gcs", "put", Duration::from_secs(5)).to_string();
        assert!(text.contains("gcs"), "{text}");
        assert!(text.contains("put"), "{text}");
    }

    /// Only the variants that came from a backend name one.
    #[test]
    fn the_backend_is_reported_only_where_there_was_one() {
        assert_eq!(Error::refused("gcs", "x").backend(), Some("gcs"));
        assert_eq!(
            Error::unavailable("azure", "x", None).backend(),
            Some("azure")
        );
        assert_eq!(
            Error::unsupported("local", "presign").backend(),
            Some("local")
        );
        assert_eq!(Error::key("k", "x").backend(), None);
        assert_eq!(Error::too_large("K", 1).backend(), None);
    }
}
