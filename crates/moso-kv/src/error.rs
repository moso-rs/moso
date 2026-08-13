//! What can go wrong, and what the HTTP layer does with it.
//!
//! A cache is not a database, and the errors say so. Almost everything here
//! renders as a 503 with a `Retry-After` rather than a 500, because a store
//! that is unreachable is a transient condition an operator already has an
//! alert for — and because [`FailureMode::Degrade`](crate::FailureMode::Degrade)
//! means most of these never reach a handler at all.
//!
//! The two exceptions are worth naming:
//!
//! * [`Error::Unsupported`] is a **programmer error**. It means code called an
//!   operation the configured backend does not have, and the fix is either to
//!   check [`Capabilities`](crate::Capabilities) first or to change the
//!   backend. It renders as a 500 because no retry will help.
//! * [`Error::Codec`] is a programmer error too: a namespace's `Value` changed
//!   shape without its `version` being bumped, and the bytes in the store no
//!   longer parse. The message says exactly that, and names the namespace.

use std::fmt;
use std::time::Duration;

use crate::key::KeyError;

/// The result type every fallible operation in this crate returns.
///
/// ```
/// use moso_kv::{Error, Result};
///
/// fn nothing() -> Result<u8> {
///     Ok(7)
/// }
///
/// assert_eq!(nothing().unwrap(), 7);
/// let _: fn() -> std::result::Result<u8, Error> = nothing;
/// ```
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A boxed source error, for the parts of a driver's failure Moso does not
/// model.
///
/// ```
/// use moso_kv::BoxError;
///
/// let source: BoxError = Box::new(std::io::Error::other("connection reset"));
/// assert_eq!(source.to_string(), "connection reset");
/// ```
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Everything `moso-kv` can fail with.
///
/// ```
/// use moso_kv::Error;
///
/// let error = Error::unsupported("memory", "subscribe", "pubsub_cross_process");
/// assert!(error.is_programmer_error());
/// assert_eq!(error.to_string(), "the memory backend does not support `subscribe`");
///
/// // A programmer error is never retried, and never told to the client.
/// assert!(!error.retryable());
/// ```
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The backend refused, timed out, or is unreachable.
    ///
    /// The operation name is a `&'static str` such as `"get"` so that the log
    /// line and the metric label agree without a format string.
    #[error("the {backend} backend failed during `{operation}`")]
    Backend {
        /// Which backend: `memory`, `redis` or `postgres`.
        backend: &'static str,
        /// The `KvStore` method that failed.
        operation: &'static str,
        /// The driver's own error.
        #[source]
        source: BoxError,
    },

    /// The backend does not implement this operation.
    ///
    /// Always a programmer error: the capability is knowable before the call,
    /// through [`KvStore::capabilities`](crate::KvStore::capabilities).
    #[error("the {backend} backend does not support `{operation}`")]
    Unsupported {
        /// Which backend.
        backend: &'static str,
        /// The `KvStore` method that is not available.
        operation: &'static str,
        /// The [`Capabilities`](crate::Capabilities) field that is `false`.
        capability: &'static str,
    },

    /// A stored value did not decode into the namespace's `Value` type.
    #[error("the value stored under namespace `{namespace}` did not decode")]
    Codec {
        /// The namespace whose `Value` the bytes failed to become.
        namespace: &'static str,
        /// The serialiser's own error.
        #[source]
        source: BoxError,
    },

    /// A key could not be built.
    #[error(transparent)]
    Key(#[from] KeyError),

    /// The circuit breaker is open: the backend failed repeatedly and is being
    /// given time to recover.
    #[error("the {backend} backend is in a failing state; not retrying for {}", humantime::format_duration(*retry_after))]
    CircuitOpen {
        /// Which backend.
        backend: &'static str,
        /// How long until the next probe.
        retry_after: Duration,
    },

    /// A lock is held by somebody else and the acquisition deadline passed.
    #[error("the lock `{name}` is held elsewhere")]
    LockHeld {
        /// The lock's name, as passed to [`Kv::lock`](crate::Kv::lock).
        name: String,
        /// How long to wait before trying again — the remaining lease.
        retry_after: Duration,
    },

    /// A lock guard could not renew its lease and has lost the lock.
    #[error("the lease on lock `{name}` expired before the work finished")]
    LockLost {
        /// The lock's name.
        name: String,
    },

    /// A pubsub channel name is not usable on this backend.
    #[error("`{channel}` is not a usable channel name on the {backend} backend: {reason}")]
    Channel {
        /// Which backend.
        backend: &'static str,
        /// The channel name that was rejected.
        channel: String,
        /// Why, in a sentence that names the limit.
        reason: &'static str,
    },

    /// The configuration could not be turned into a store.
    #[error("the kv configuration is not usable: {detail}")]
    Config {
        /// What is wrong, and what to change.
        detail: String,
    },

    /// An application error, on its way through the cache layer.
    ///
    /// The computation a
    /// [`get_or_insert_with`](crate::Kv::get_or_insert_with) runs is the
    /// application's, and it fails for the application's reasons — a 404, a
    /// 403, a validation problem. Boxing the original
    /// [`moso_core::Error`] here and unwrapping it on the way out means the
    /// status the handler meant is the status the client gets, rather than the
    /// 500 that a flattened error would have produced.
    ///
    /// Written by `?` inside a compute closure, through
    /// [`From<moso_core::Error>`], and read by
    /// [`From<Error> for moso_core::Error`].
    #[error(transparent)]
    Http(Box<moso_core::Error>),
}

impl Error {
    /// A [`Error::Backend`] from a driver error.
    ///
    /// ```
    /// use moso_kv::Error;
    ///
    /// let error = Error::backend(
    ///     "redis",
    ///     "get",
    ///     std::io::Error::other("connection reset"),
    /// );
    /// assert!(error.retryable());
    /// assert_eq!(error.backend_name(), Some("redis"));
    /// ```
    pub fn backend(
        backend: &'static str,
        operation: &'static str,
        source: impl Into<BoxError>,
    ) -> Self {
        Error::Backend {
            backend,
            operation,
            source: source.into(),
        }
    }

    /// An [`Error::Unsupported`] for an operation the backend does not have.
    ///
    /// ```
    /// use moso_kv::Error;
    ///
    /// let error = Error::unsupported("postgres", "zadd", "structures");
    /// assert!(error.is_programmer_error());
    /// ```
    pub fn unsupported(
        backend: &'static str,
        operation: &'static str,
        capability: &'static str,
    ) -> Self {
        Error::Unsupported {
            backend,
            operation,
            capability,
        }
    }

    /// An [`Error::Codec`] for a value that did not decode.
    ///
    /// ```
    /// use moso_kv::Error;
    ///
    /// let error = Error::codec("profile", std::io::Error::other("expected `,`"));
    /// assert!(error.is_programmer_error());
    /// assert!(error.to_string().contains("profile"));
    /// ```
    pub fn codec(namespace: &'static str, source: impl Into<BoxError>) -> Self {
        Error::Codec {
            namespace,
            source: source.into(),
        }
    }

    /// Which backend produced this, when the error names one.
    ///
    /// Named `backend_name` and not `backend` because the constructor
    /// [`Error::backend`] already has that name, and one identifier meaning
    /// both "make one of these" and "read a field of one" is a coin flip at
    /// every call site.
    ///
    /// ```
    /// use moso_kv::Error;
    ///
    /// assert_eq!(
    ///     Error::unsupported("memory", "eval", "scripting").backend_name(),
    ///     Some("memory"),
    /// );
    /// assert_eq!(Error::codec("session", std::io::Error::other("x")).backend_name(), None);
    /// ```
    pub fn backend_name(&self) -> Option<&'static str> {
        match self {
            Error::Backend { backend, .. }
            | Error::Unsupported { backend, .. }
            | Error::CircuitOpen { backend, .. }
            | Error::Channel { backend, .. } => Some(backend),
            Error::Codec { .. }
            | Error::Key(_)
            | Error::LockHeld { .. }
            | Error::LockLost { .. }
            | Error::Config { .. }
            | Error::Http(_) => None,
        }
    }

    /// Carry an application error through the cache layer unchanged.
    ///
    /// The same thing `?` does inside a compute closure, spelled out for the
    /// places where a `?` will not fit.
    ///
    /// ```
    /// use moso_kv::Error;
    ///
    /// let error = Error::http(moso_core::Error::not_found("user"));
    /// let back: moso_core::Error = error.into();
    /// assert_eq!(back.status(), moso_core::deps::http::StatusCode::NOT_FOUND);
    /// ```
    #[must_use]
    pub fn http(error: moso_core::Error) -> Self {
        Error::Http(Box::new(error))
    }

    /// Whether the same call, made again later, could succeed.
    ///
    /// This is what [`FailureMode`](crate::FailureMode) consults: a transient
    /// error degrades, a programmer error propagates whatever the mode says,
    /// because swallowing a bug makes it permanent.
    ///
    /// ```
    /// use moso_kv::Error;
    /// use std::time::Duration;
    ///
    /// assert!(Error::backend("redis", "set", std::io::Error::other("reset")).retryable());
    /// assert!(!Error::unsupported("memory", "eval", "scripting").retryable());
    /// ```
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Error::Backend { .. } | Error::CircuitOpen { .. } | Error::LockHeld { .. }
        )
    }

    /// Whether this is a mistake in the program rather than a condition in the
    /// world.
    ///
    /// A programmer error is never degraded away: `Degrade` turns an
    /// unreachable Redis into a cache miss, and turning a decode failure into a
    /// cache miss would hide a schema change behind a permanent 100% miss rate.
    ///
    /// ```
    /// use moso_kv::Error;
    ///
    /// assert!(Error::codec("profile", std::io::Error::other("x")).is_programmer_error());
    /// assert!(!Error::backend("redis", "get", std::io::Error::other("x")).is_programmer_error());
    /// ```
    pub fn is_programmer_error(&self) -> bool {
        matches!(
            self,
            Error::Unsupported { .. } | Error::Codec { .. } | Error::Key(_) | Error::Config { .. }
        )
    }

    /// How long to wait before retrying, when the error knows.
    ///
    /// ```
    /// use moso_kv::Error;
    /// use std::time::Duration;
    ///
    /// let error = Error::CircuitOpen { backend: "redis", retry_after: Duration::from_secs(5) };
    /// assert_eq!(error.retry_after(), Some(Duration::from_secs(5)));
    /// ```
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::CircuitOpen { retry_after, .. } | Error::LockHeld { retry_after, .. } => {
                Some(*retry_after)
            }
            _ => None,
        }
    }

    /// A one-line rendering of the whole `source` chain, for a log field.
    ///
    /// ```
    /// use moso_kv::Error;
    ///
    /// let error = Error::backend("redis", "get", std::io::Error::other("connection reset"));
    /// assert_eq!(
    ///     error.chain(),
    ///     "the redis backend failed during `get`: connection reset",
    /// );
    /// ```
    pub fn chain(&self) -> String {
        let mut out = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(next) = source {
            out.push_str(": ");
            out.push_str(&next.to_string());
            source = next.source();
        }
        out
    }
}

/// The bridge into the HTTP layer.
///
/// A KV failure that reaches a handler is a 503 with a `Retry-After`, not a
/// 500: the request can be retried and the client should be told so. A
/// programmer error is a 500 with the detail suppressed, exactly like every
/// other 5xx in Moso.
///
/// ```
/// use moso_kv::Error;
///
/// let http: moso_core::Error = Error::backend(
///     "redis",
///     "get",
///     std::io::Error::other("connection reset"),
/// )
/// .into();
///
/// assert_eq!(http.status(), moso_core::deps::http::StatusCode::SERVICE_UNAVAILABLE);
/// ```
impl From<Error> for moso_core::Error {
    fn from(error: Error) -> Self {
        // An application error passes through with its status intact. This is
        // the half of the round trip that makes `?` inside a compute closure
        // safe: a 404 goes in and a 404 comes out.
        if let Error::Http(inner) = error {
            return *inner;
        }

        let retry_after = error.retry_after();
        let mapped = match &error {
            Error::LockHeld { name, .. } => moso_core::Error::conflict(format!(
                "another operation holds the `{name}` lock; try again shortly"
            )),
            Error::LockLost { .. } => {
                moso_core::Error::unavailable("the lock lease expired before the work finished")
            }
            _ if error.is_programmer_error() => moso_core::Error::internal_msg(
                "the cache layer was used in a way the configured backend cannot serve",
            ),
            _ => moso_core::Error::unavailable("the cache or session store is unavailable"),
        };

        let mapped = match retry_after {
            Some(after) if after > Duration::ZERO => {
                let seconds = after.as_secs().max(1).to_string();
                match http::HeaderValue::try_from(seconds) {
                    Ok(value) => mapped.with_header(http::header::RETRY_AFTER, value),
                    Err(_) => mapped,
                }
            }
            _ => mapped,
        };

        mapped.with_source(error)
    }
}

/// The other half of the round trip.
///
/// What makes `?` work inside a compute closure: an application function that
/// returns `moso_core::Result` can be called with `?` in a closure that returns
/// [`crate::Result`], and the error keeps its identity all the way back out.
///
/// ```
/// use moso_kv::{Error, Result};
///
/// fn load() -> moso_core::Result<u8> {
///     Err(moso_core::Error::not_found("user"))
/// }
///
/// fn cached() -> Result<u8> {
///     Ok(load()?)
/// }
///
/// let http: moso_core::Error = cached().expect_err("not found").into();
/// assert_eq!(http.status(), moso_core::deps::http::StatusCode::NOT_FOUND);
/// ```
impl From<moso_core::Error> for Error {
    fn from(error: moso_core::Error) -> Self {
        Error::Http(Box::new(error))
    }
}

/// A `Display` wrapper that renders the whole source chain.
///
/// Used by the degrade path's `warn!`, where the driver's message is the only
/// part that says anything and the outer error is boilerplate.
pub(crate) struct Chain<'a>(pub(crate) &'a Error);

impl fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.chain())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backend_failure_is_retryable_and_names_its_backend() {
        let error = Error::backend("redis", "get", std::io::Error::other("reset"));
        assert!(error.retryable());
        assert!(!error.is_programmer_error());
        assert_eq!(error.backend_name(), Some("redis"));
    }

    #[test]
    fn a_programmer_error_is_never_retryable() {
        for error in [
            Error::unsupported("memory", "zadd", "structures"),
            Error::codec("profile", std::io::Error::other("bad json")),
            Error::Config {
                detail: "no url".to_owned(),
            },
        ] {
            assert!(error.is_programmer_error(), "{error}");
            assert!(!error.retryable(), "{error}");
        }
    }

    #[test]
    fn the_chain_includes_the_driver_message() {
        let error = Error::backend("postgres", "set", std::io::Error::other("closed"));
        assert_eq!(
            error.chain(),
            "the postgres backend failed during `set`: closed"
        );
        assert_eq!(Chain(&error).to_string(), error.chain());
    }

    #[test]
    fn a_transient_failure_is_a_503() {
        let http: moso_core::Error =
            Error::backend("redis", "get", std::io::Error::other("reset")).into();
        assert_eq!(http.status(), http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(http.is_server_error());
    }

    #[test]
    fn a_held_lock_is_a_409_with_a_retry_after() {
        let http: moso_core::Error = Error::LockHeld {
            name: "import:acme".to_owned(),
            retry_after: Duration::from_secs(12),
        }
        .into();
        assert_eq!(http.status(), http::StatusCode::CONFLICT);
        let headers = http.headers().expect("the retry hint is a header");
        assert_eq!(headers[http::header::RETRY_AFTER], "12");
    }

    #[test]
    fn an_open_circuit_carries_a_retry_after() {
        let error = Error::CircuitOpen {
            backend: "redis",
            retry_after: Duration::from_millis(400),
        };
        assert_eq!(error.retry_after(), Some(Duration::from_millis(400)));

        // Sub-second waits round up: `Retry-After: 0` is an invitation to spin.
        let http: moso_core::Error = error.into();
        assert_eq!(
            http.headers().expect("headers")[http::header::RETRY_AFTER],
            "1"
        );
    }

    #[test]
    fn a_programmer_error_does_not_leak_its_detail_to_the_client() {
        let http: moso_core::Error = Error::codec("profile", std::io::Error::other("x")).into();
        assert_eq!(http.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
        let detail = http.detail().unwrap_or_default().to_owned();
        assert!(!detail.contains("profile"), "{detail}");
    }
}
