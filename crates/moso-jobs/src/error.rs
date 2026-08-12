//! Job failures, and the one distinction that matters: retry or do not.
//!
//! A background job that retries a permanent failure twenty-five times is not
//! resilient, it is a denial-of-service against its own queue. A job that gives
//! up on a transient failure loses work. [`Error::retryable`] is the whole of
//! that decision, and it is a property of the error rather than a policy on the
//! queue, because only the code that failed knows which kind it was.

use std::borrow::Cow;

/// The result of a job, and of every fallible operation in this crate.
pub type Result<T = (), E = Error> = core::result::Result<T, E>;

/// A boxed error from a job body or a backend.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Something went wrong running, enqueuing or scheduling a job.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The job failed, and trying again might work.
    ///
    /// A timeout talking to a third party, a deadlock, a rate limit. The
    /// default for anything converted from another error type, because
    /// assuming "retryable" loses less than assuming "permanent".
    #[error("{detail}")]
    Retryable {
        /// What happened.
        detail: String,
        /// The source, when there was one.
        #[source]
        source: Option<BoxError>,
    },

    /// The job failed and will fail again. Goes straight to the dead-letter
    /// queue without burning the retry budget.
    ///
    /// A malformed payload, a row that no longer exists, a business rule that
    /// says no.
    #[error("{detail}")]
    Permanent {
        /// What happened.
        detail: String,
        /// The source, when there was one.
        #[source]
        source: Option<BoxError>,
    },

    /// The job exceeded its timeout and was cancelled.
    ///
    /// Retryable, but it counts against the retry budget: a job that always
    /// takes too long should end up in the dead-letter queue rather than
    /// running forever.
    #[error("job `{job}` exceeded its {timeout:?} timeout")]
    Timeout {
        /// The job's wire name.
        job: &'static str,
        /// The limit it passed.
        timeout: std::time::Duration,
    },

    /// The payload could not be deserialised.
    ///
    /// The poison-payload guard: goes **straight** to the dead-letter queue,
    /// because a deploy that changed a payload's shape must not turn 40,000
    /// queued jobs into 1,000,000 failed attempts.
    #[error("payload for `{job}` did not deserialise: {detail}")]
    Payload {
        /// The job's wire name.
        job: String,
        /// What serde said.
        detail: String,
    },

    /// A job was enqueued that no registry knows how to run.
    ///
    /// A boot error when it is caught at boot, which is where
    /// [`JobRegistry::validate`](crate::JobRegistry::validate) catches it, and
    /// a dead-letter entry when a rolling deploy produces one at runtime.
    #[error("{}", unregistered_message(name, *suggestion, site.as_deref()))]
    Unregistered {
        /// The wire name that arrived.
        name: String,
        /// The closest registered name, when one is close enough.
        suggestion: Option<&'static str>,
        /// Where the enqueue was written, `src/services/search.rs:42`.
        ///
        /// Captured with `#[track_caller]` on the enqueue path, so the message
        /// points at the line to change rather than at the framework's own
        /// stack. `None` when the job arrived off a queue rather than from an
        /// enqueue in this process — a rolling deploy, typically.
        site: Option<String>,
    },

    /// The queue backend was unreachable or transiently failed.
    #[error("{backend} queue is unavailable: {detail}")]
    Unavailable {
        /// The backend's name.
        backend: &'static str,
        /// What the transport reported.
        detail: String,
        /// The source, when there was one.
        #[source]
        source: Option<BoxError>,
    },

    /// The backend does not support what was asked of it.
    #[error("{backend} queue does not support {operation}")]
    Unsupported {
        /// The backend's name.
        backend: &'static str,
        /// The operation, e.g. `"push_tx"`.
        operation: &'static str,
    },

    /// Configuration is missing or contradictory.
    #[error("jobs configuration is invalid: {0}")]
    Config(Cow<'static, str>),
}

/// Renders [`Error::Unregistered`] the way `docs/03-batteries/32-jobs.md`
/// prints it: the headline, then the enqueue site, then the paste-able fix.
fn unregistered_message(
    name: &str,
    suggestion: Option<&'static str>,
    site: Option<&str>,
) -> String {
    let mut rendered = format!("job `{name}` is enqueued but not registered");
    if let Some(site) = site {
        rendered.push_str(&format!("\n    enqueued at  {site}"));
    }
    if let Some(suggestion) = suggestion {
        rendered.push_str(&format!("\n    did you mean `{suggestion}`?"));
    }
    rendered.push_str(&format!(
        "\n    fix          add `.register::<{name}>()` to the `JobRegistry`"
    ));
    rendered
}

impl Error {
    /// A failure worth retrying.
    ///
    /// Named `retry` and not `retryable`, because `retryable` is the
    /// *predicate* — the same spelling `moso_core::Error` uses — and a
    /// constructor and a predicate with one name is a bad afternoon.
    ///
    /// ```
    /// use moso_jobs::Error;
    ///
    /// let err = Error::retry("upstream returned 502");
    /// assert!(err.retryable());
    /// ```
    pub fn retry(detail: impl Into<String>) -> Self {
        Self::Retryable {
            detail: detail.into(),
            source: None,
        }
    }

    /// A failure that will not get better.
    ///
    /// ```
    /// use moso_jobs::Error;
    ///
    /// let err = Error::permanent("user 42 no longer exists");
    /// assert!(!err.retryable());
    /// ```
    pub fn permanent(detail: impl Into<String>) -> Self {
        Self::Permanent {
            detail: detail.into(),
            source: None,
        }
    }

    /// A queue that could not be reached.
    ///
    /// ```
    /// use moso_jobs::Error;
    ///
    /// let err = Error::unavailable("postgres", "connection refused");
    /// assert!(err.retryable());
    /// ```
    pub fn unavailable(backend: &'static str, detail: impl Into<String>) -> Self {
        Self::Unavailable {
            backend,
            detail: detail.into(),
            source: None,
        }
    }

    /// A configuration problem, named with its fix.
    ///
    /// ```
    /// use moso_jobs::Error;
    ///
    /// let err = Error::config("`jobs.concurrency` must be at least 1");
    /// assert!(!err.retryable());
    /// ```
    pub fn config(detail: impl Into<Cow<'static, str>>) -> Self {
        Self::Config(detail.into())
    }

    /// Attach the underlying error.
    ///
    /// ```
    /// # use moso_jobs::Error;
    /// let boxed: Box<dyn std::error::Error + Send + Sync> = "io failed".into();
    /// let error = Error::retry("send failed").with_source(boxed);
    /// assert!(std::error::Error::source(&error).is_some());
    /// ```
    #[must_use]
    pub fn with_source(self, source: BoxError) -> Self {
        match self {
            Self::Retryable { detail, .. } => Self::Retryable {
                detail,
                source: Some(source),
            },
            Self::Permanent { detail, .. } => Self::Permanent {
                detail,
                source: Some(source),
            },
            Self::Unavailable {
                backend, detail, ..
            } => Self::Unavailable {
                backend,
                detail,
                source: Some(source),
            },
            // The remaining variants carry their whole story in their fields;
            // attaching a source to one would produce a message that says the
            // same thing twice.
            other => other,
        }
    }

    /// Whether the worker should try again.
    ///
    /// [`Error::Retryable`], [`Error::Timeout`] and
    /// [`Error::Unavailable`] are; everything else goes to the dead-letter
    /// queue on the first attempt.
    ///
    /// ```
    /// use moso_jobs::Error;
    ///
    /// assert!(!Error::permanent("gone").retryable());
    /// assert!(Error::retry("later").retryable());
    /// ```
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Retryable { .. } | Self::Timeout { .. } | Self::Unavailable { .. }
        )
    }

    /// Whether this error skips the retry budget entirely.
    ///
    /// True for [`Error::Payload`] and [`Error::Unregistered`]: both mean the
    /// job cannot run *at all*, so attempting it again is pure cost.
    ///
    /// ```
    /// use moso_jobs::Error;
    ///
    /// assert!(!Error::retry("later").skips_retries());
    /// ```
    #[must_use]
    pub fn skips_retries(&self) -> bool {
        matches!(self, Self::Payload { .. } | Self::Unregistered { .. })
    }

    /// The error chain as one line, for the dead-letter record.
    ///
    /// ```
    /// # use moso_jobs::Error;
    /// let boxed: Box<dyn std::error::Error + Send + Sync> = "connection refused".into();
    /// let error = Error::retry("send failed").with_source(boxed);
    /// assert_eq!(error.chain(), "send failed: connection refused");
    /// ```
    #[must_use]
    pub fn chain(&self) -> String {
        let mut rendered = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(next) = source {
            let text = next.to_string();
            // A `#[from]` wrapper whose Display is `{0}` repeats its source
            // verbatim; printing it twice makes a dead letter harder to read,
            // not easier.
            if !rendered.ends_with(&text) {
                rendered.push_str(": ");
                rendered.push_str(&text);
            }
            source = next.source();
        }
        rendered
    }
}

impl From<moso_core::Error> for Error {
    /// A framework error becomes a job error, keeping its own retry advice.
    ///
    /// [`moso_core::Error::retryable`] already distinguishes a 503 from a 422,
    /// so the mapping is exact rather than a guess.
    fn from(error: moso_core::Error) -> Self {
        let detail = error.chain();
        if error.retryable() {
            Self::Retryable {
                detail,
                source: Some(Box::new(error)),
            }
        } else {
            Self::Permanent {
                detail,
                source: Some(Box::new(error)),
            }
        }
    }
}

impl From<moso_orm::Error> for Error {
    /// A database error becomes a job error.
    ///
    /// A unique violation is *permanent* — the row is already there, which for
    /// an idempotent job usually means the work is done — and a connection
    /// failure is retryable.
    fn from(error: moso_orm::Error) -> Self {
        let detail = error.to_string();
        // `moso_orm::Error::retryable` already knows about serialisation
        // failures, deadlocks and pool timeouts. Connection loss and statement
        // timeouts are added here because for a *job* they are transient — a
        // request would give up, a job has a retry budget to spend.
        let retryable = error.is_retryable()
            || matches!(
                error,
                moso_orm::Error::Connection { .. } | moso_orm::Error::StatementTimeout { .. }
            );
        if retryable {
            Self::Retryable {
                detail,
                source: Some(Box::new(error)),
            }
        } else {
            Self::Permanent {
                detail,
                source: Some(Box::new(error)),
            }
        }
    }
}

impl From<moso_kv::Error> for Error {
    /// A cache or lock failure is retryable unless the store said otherwise.
    fn from(error: moso_kv::Error) -> Self {
        let detail = error.to_string();
        match error {
            // Asking a backend for something it does not have will not start
            // working on the second attempt.
            moso_kv::Error::Unsupported { .. }
            | moso_kv::Error::Config { .. }
            | moso_kv::Error::Key(_) => Self::Permanent {
                detail,
                source: Some(Box::new(error)),
            },
            _ => Self::Retryable {
                detail,
                source: Some(Box::new(error)),
            },
        }
    }
}

impl From<moso_orm::DecodeError> for Error {
    /// A column that did not decode.
    ///
    /// Permanent: the row is the shape it is, and reading it again will read
    /// the same bytes. This is a schema mismatch — a queue table written by a
    /// different version of this crate — and it belongs in the dead-letter
    /// queue where somebody will see it.
    fn from(error: moso_orm::DecodeError) -> Self {
        Self::Permanent {
            detail: format!("a job row did not decode: {error}"),
            source: Some(Box::new(error)),
        }
    }
}

impl From<serde_json::Error> for Error {
    /// A serialisation failure while *enqueuing* is permanent: the payload will
    /// not serialise any better on a second attempt.
    fn from(error: serde_json::Error) -> Self {
        Self::Permanent {
            detail: error.to_string(),
            source: Some(Box::new(error)),
        }
    }
}

impl From<Error> for moso_core::Error {
    /// A job error becomes an HTTP problem, for the enqueue-from-a-handler path.
    ///
    /// [`Error::Unavailable`] is a 503 marked retryable; everything else is a
    /// 500 whose detail is suppressed outside development.
    fn from(error: Error) -> Self {
        match &error {
            Error::Unavailable { backend, .. } => {
                moso_core::Error::unavailable(format!("the {backend} job queue is unavailable"))
                    .with_source(error)
            }
            Error::Config(_) | Error::Unregistered { .. } | Error::Unsupported { .. } => {
                moso_core::Error::internal_msg("the job queue is misconfigured").with_source(error)
            }
            _ => moso_core::Error::internal(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The retry decision is the whole point of this type, so every variant's
    /// answer is pinned rather than read off a `match`.
    #[test]
    fn every_variant_answers_the_retry_question() {
        assert!(Error::retry("x").retryable());
        assert!(!Error::permanent("x").retryable());
        assert!(
            Error::Timeout {
                job: "j",
                timeout: std::time::Duration::from_secs(1),
            }
            .retryable()
        );
        assert!(Error::unavailable("postgres", "down").retryable());
        assert!(
            !Error::Payload {
                job: "j".to_owned(),
                detail: "bad".to_owned(),
            }
            .retryable()
        );
        assert!(
            !Error::Unregistered {
                name: "j".to_owned(),
                suggestion: None,
                site: None,
            }
            .retryable()
        );
        assert!(
            !Error::Unsupported {
                backend: "redis",
                operation: "push_tx",
            }
            .retryable()
        );
        assert!(!Error::config("nope").retryable());
    }

    /// A poison payload must not spend the retry budget: 40,000 rows that
    /// cannot deserialise would otherwise become a million failed attempts.
    #[test]
    fn a_poison_payload_skips_the_budget_entirely() {
        let poisoned = Error::Payload {
            job: "send_welcome_email".to_owned(),
            detail: "missing field `user_id`".to_owned(),
        };
        assert!(poisoned.skips_retries());
        assert!(!poisoned.retryable());

        assert!(!Error::retry("later").skips_retries());
        assert!(!Error::permanent("gone").skips_retries());
    }

    /// The unregistered message is the one an operator reads at 3 a.m., so its
    /// three lines — headline, site, fix — are asserted verbatim.
    #[test]
    fn the_unregistered_message_names_the_enqueue_site_and_the_fix() {
        let error = Error::Unregistered {
            name: "ReindexSearch".to_owned(),
            suggestion: Some("ReindexSearchIndex"),
            site: Some("src/services/search.rs:42".to_owned()),
        };
        let rendered = error.to_string();
        assert!(rendered.contains("job `ReindexSearch` is enqueued but not registered"));
        assert!(rendered.contains("enqueued at  src/services/search.rs:42"));
        assert!(rendered.contains("did you mean `ReindexSearchIndex`?"));
        assert!(rendered.contains("add `.register::<ReindexSearch>()`"));
    }

    /// The chain is what lands in the dead-letter row, and a chain that repeats
    /// itself is a chain nobody reads to the end.
    #[test]
    fn the_chain_walks_sources_without_repeating_them() {
        let boxed: BoxError = "connection refused".into();
        let error = Error::retry("send failed").with_source(boxed);
        assert_eq!(error.chain(), "send failed: connection refused");

        let plain = Error::permanent("gone");
        assert_eq!(plain.chain(), "gone");
    }

    /// A source attached to a variant that has nowhere to put it must not be
    /// silently swallowed into a different variant.
    #[test]
    fn attaching_a_source_never_changes_the_variant() {
        let boxed: BoxError = "why".into();
        let timeout = Error::Timeout {
            job: "j",
            timeout: std::time::Duration::from_secs(1),
        }
        .with_source(boxed);
        assert!(matches!(timeout, Error::Timeout { .. }));
    }

    /// The whole reason `Error::retryable` is a property of the error: a 503
    /// from an upstream must keep its retry advice across the conversion.
    #[test]
    fn a_framework_error_keeps_its_own_retry_advice() {
        let retryable: Error = moso_core::Error::unavailable("upstream is down").into();
        assert!(retryable.retryable());

        let permanent: Error = moso_core::Error::bad_request("malformed").into();
        assert!(!permanent.retryable());
    }

    /// A unique violation usually means an idempotent job already did the work,
    /// so retrying it twenty-five times is pure cost.
    #[test]
    fn a_unique_violation_is_permanent_and_a_lost_connection_is_not() {
        let unique: Error = moso_orm::Error::UniqueViolation(Box::new(
            moso_orm::ConstraintViolation::unique("User", "users_email_key").with_column("email"),
        ))
        .into();
        assert!(!unique.retryable());

        let dropped: Error = moso_orm::Error::Connection {
            detail: "server closed the connection".to_owned(),
        }
        .into();
        assert!(dropped.retryable());
    }

    /// Enqueuing serialises the payload; a payload that will not serialise now
    /// will not serialise in thirty seconds either.
    #[test]
    fn a_serialisation_failure_while_enqueuing_is_permanent() {
        // A map with a composite key: JSON objects have string keys, so this
        // is a payload that genuinely cannot be serialised.
        let unserialisable: std::collections::BTreeMap<(u8, u8), u8> =
            std::collections::BTreeMap::from([((1, 2), 3)]);
        let failure = serde_json::to_value(&unserialisable).expect_err("keys must be strings");
        let error: Error = failure.into();
        assert!(!error.retryable());
    }

    /// The enqueue-from-a-handler path has to produce a 503 and not a 500 when
    /// the queue is the thing that is down.
    #[test]
    fn an_unavailable_queue_becomes_a_503() {
        let http: moso_core::Error = Error::unavailable("postgres", "connection refused").into();
        assert_eq!(http.status().as_u16(), 503);
        assert!(http.retryable());

        let internal: moso_core::Error = Error::permanent("business rule").into();
        assert_eq!(internal.status().as_u16(), 500);
        assert!(internal.is_server_error());
    }
}
