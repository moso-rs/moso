//! What can go wrong when sending, and what each failure becomes over HTTP.
//!
//! Mail failures are not all alike, and collapsing them into one string is why
//! "the email did not arrive" is such a bad incident to debug. The variants
//! below separate the four cases that need different *actions*: fix the
//! template, fix the address, retry later, stop sending to this person.

use std::borrow::Cow;
use std::time::Duration;

/// The result of every fallible operation in this crate.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// A boxed error from a backend, kept as a source without naming its crate.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Something went wrong rendering, addressing or sending a message.
///
/// ```
/// use moso_mail::Error;
///
/// let err = Error::suppressed("bounced@example.com", "hard bounce on 2026-01-04");
/// assert!(err.is_suppressed());
/// assert!(!err.retryable());
/// ```
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A template referenced a variable the context does not have, or failed
    /// to render.
    ///
    /// [`Jinja`](crate::Jinja) is strict about undefined variables, so this is
    /// what a typo in a template becomes.
    /// [`TemplateEngine::variables`](crate::TemplateEngine::variables) is how a
    /// test catches one before a send does.
    #[error("template `{template}` failed to render: {detail}")]
    Template {
        /// The template's path, as it was registered with the engine.
        template: Cow<'static, str>,
        /// What the engine said.
        detail: String,
    },

    /// An address is not a valid mailbox.
    #[error("`{address}` is not a valid address: {detail}")]
    Address {
        /// The offending address, verbatim.
        address: String,
        /// Why it was rejected.
        detail: Cow<'static, str>,
    },

    /// The message is addressed to somebody on the suppression list.
    ///
    /// Sending anyway damages the sending domain's reputation, so this is an
    /// error and not a warning.
    #[error("`{address}` is suppressed: {reason}")]
    Suppressed {
        /// The suppressed address.
        address: String,
        /// Why it was suppressed, for the operator reading the log.
        reason: Cow<'static, str>,
    },

    /// The provider refused the message and will refuse it again.
    #[error("{backend} rejected the message: {detail}")]
    Rejected {
        /// The backend's name, as [`Mailer::name`](crate::Mailer::name) reports it.
        backend: &'static str,
        /// The provider's message, already redacted of any credential.
        detail: String,
    },

    /// The provider was unreachable or rate-limited. Retrying may work.
    #[error("{backend} is unavailable: {detail}")]
    Unavailable {
        /// The backend's name.
        backend: &'static str,
        /// What the transport reported.
        detail: String,
        /// The source, when the backend had one.
        #[source]
        source: Option<BoxError>,
    },

    /// The send did not finish inside its configured deadline.
    ///
    /// Distinct from [`Error::Unavailable`] on purpose. "Unavailable" says the
    /// provider answered and said no; this says it never answered, and the two
    /// call for different operator action — one is a provider incident, the
    /// other is usually a network path or a wildly undersized
    /// [`MailConfig::timeout`](crate::MailConfig::timeout). Both are
    /// retryable, but only this one may have been *accepted* before the answer
    /// was lost, which is why a message worth retrying carries a
    /// [`MessageKey`](crate::MessageKey).
    #[error("{backend} did not finish sending within {after:?}")]
    Timeout {
        /// The backend's name, as [`Mailer::name`](crate::Mailer::name) reports it.
        backend: &'static str,
        /// The deadline that elapsed.
        after: Duration,
    },

    /// The backend does not support what was asked of it.
    ///
    /// Checked against [`MailCapabilities`](crate::MailCapabilities) rather
    /// than discovered at the provider.
    #[error("{backend} does not support {operation}")]
    Unsupported {
        /// The backend's name.
        backend: &'static str,
        /// The operation that is not available, e.g. `"send_batch"`.
        operation: &'static str,
    },

    /// A provider webhook did not carry a valid signature.
    #[error("webhook signature from {backend} did not verify")]
    Signature {
        /// The backend whose webhook was received.
        backend: &'static str,
    },

    /// Configuration is missing or contradictory.
    #[error("mail configuration is invalid: {0}")]
    Config(Cow<'static, str>),
}

impl Error {
    /// A [`Error::Suppressed`], the variant applications match on most.
    ///
    /// ```
    /// use moso_mail::Error;
    ///
    /// let err = Error::suppressed("x@example.com", "complaint");
    /// assert!(err.is_suppressed());
    /// ```
    pub fn suppressed(address: impl Into<String>, reason: impl Into<Cow<'static, str>>) -> Self {
        Self::Suppressed {
            address: address.into(),
            reason: reason.into(),
        }
    }

    /// A [`Error::Template`] naming the template and the engine's message.
    ///
    /// ```
    /// use moso_mail::Error;
    ///
    /// let err = Error::template("emails/welcome.html", "undefined variable `user`");
    /// assert!(!err.retryable());
    /// ```
    pub fn template(template: impl Into<Cow<'static, str>>, detail: impl Into<String>) -> Self {
        Self::Template {
            template: template.into(),
            detail: detail.into(),
        }
    }

    /// An [`Error::Address`] naming the rejected mailbox and the rule it broke.
    ///
    /// ```
    /// use moso_mail::Error;
    ///
    /// let err = Error::address("not an address", "missing `@`");
    /// assert!(!err.retryable());
    /// ```
    pub fn address(address: impl Into<String>, detail: impl Into<Cow<'static, str>>) -> Self {
        Self::Address {
            address: address.into(),
            detail: detail.into(),
        }
    }

    /// An [`Error::Rejected`]: the provider will refuse this message again.
    ///
    /// ```
    /// use moso_mail::Error;
    ///
    /// assert!(!Error::rejected("ses", "550 message rejected").retryable());
    /// ```
    pub fn rejected(backend: &'static str, detail: impl Into<String>) -> Self {
        Self::Rejected {
            backend,
            detail: detail.into(),
        }
    }

    /// An [`Error::Unavailable`] from a backend that could not reach its provider.
    ///
    /// ```
    /// use moso_mail::Error;
    ///
    /// let err = Error::unavailable("smtp", "connection reset", None);
    /// assert!(err.retryable());
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

    /// An [`Error::Timeout`] from a send that overran its deadline.
    ///
    /// Constructed by [`deadline::within`](crate::deadline::within), which is
    /// the one place a deadline is enforced.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_mail::Error;
    ///
    /// let err = Error::timeout("smtp", Duration::from_secs(30));
    /// assert_eq!(err.to_string(), "smtp did not finish sending within 30s");
    /// assert!(err.retryable());
    /// ```
    #[must_use]
    pub const fn timeout(backend: &'static str, after: Duration) -> Self {
        Self::Timeout { backend, after }
    }

    /// An [`Error::Unsupported`], for an operation a backend does not have.
    ///
    /// ```
    /// use moso_mail::Error;
    ///
    /// assert_eq!(Error::unsupported("smtp", "send_batch").backend(), Some("smtp"));
    /// ```
    #[must_use]
    pub const fn unsupported(backend: &'static str, operation: &'static str) -> Self {
        Self::Unsupported { backend, operation }
    }

    /// An [`Error::Config`] naming the field and the fix.
    ///
    /// ```
    /// use moso_mail::Error;
    ///
    /// let err = Error::config("`mail.url` is required for the smtp backend");
    /// assert!(!err.retryable());
    /// ```
    pub fn config(detail: impl Into<Cow<'static, str>>) -> Self {
        Self::Config(detail.into())
    }

    /// Whether this is [`Error::Suppressed`].
    ///
    /// ```
    /// use moso_mail::Error;
    ///
    /// assert!(Error::suppressed("a@b.com", "bounce").is_suppressed());
    /// ```
    #[must_use]
    pub const fn is_suppressed(&self) -> bool {
        matches!(self, Self::Suppressed { .. })
    }

    /// Whether retrying the same send could succeed.
    ///
    /// [`Error::Unavailable`] and [`Error::Timeout`] are retryable and nothing
    /// else is. A rejected message, a bad address and a suppressed recipient
    /// are all permanent, and retrying them five times is how a sending domain
    /// gets blocklisted.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_mail::Error;
    ///
    /// assert!(!Error::template("t", "boom").retryable());
    /// assert!(Error::unavailable("smtp", "reset", None).retryable());
    /// assert!(Error::timeout("smtp", Duration::from_secs(30)).retryable());
    /// ```
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::Unavailable { .. } | Self::Timeout { .. })
    }

    /// The backend that produced this, when one did.
    ///
    /// ```
    /// use moso_mail::Error;
    ///
    /// assert_eq!(Error::template("t", "boom").backend(), None);
    /// assert_eq!(Error::rejected("ses", "no").backend(), Some("ses"));
    /// ```
    #[must_use]
    pub const fn backend(&self) -> Option<&'static str> {
        match self {
            Self::Rejected { backend, .. }
            | Self::Unavailable { backend, .. }
            | Self::Timeout { backend, .. }
            | Self::Unsupported { backend, .. }
            | Self::Signature { backend } => Some(backend),
            Self::Template { .. }
            | Self::Address { .. }
            | Self::Suppressed { .. }
            | Self::Config(_) => None,
        }
    }
}

/// The JSON pointer a mail failure points a client at.
///
/// One constant rather than a literal repeated in four arms: a client that
/// keys off the pointer keys off one string.
const RECIPIENT_POINTER: &str = "/to";

impl From<Error> for moso_core::Error {
    /// A mail failure becomes the HTTP problem it means.
    ///
    /// [`Error::Suppressed`] is a 422 with a field pointer at the address, not
    /// a 500: the caller sent something the server will not act on, and the
    /// caller can fix it. [`Error::Unavailable`] is a 503 with `retryable`.
    fn from(error: Error) -> Self {
        use moso_core::ErrorKind;

        let message = error.to_string();
        match error {
            Error::Suppressed { address, reason } => moso_core::Error::new(ErrorKind::Validation)
                .with_detail(format!("`{address}` is suppressed: {reason}"))
                .with_field(RECIPIENT_POINTER, "suppressed", &reason),
            Error::Address { address, detail } => moso_core::Error::new(ErrorKind::Validation)
                .with_detail(format!("`{address}` is not a valid address: {detail}"))
                .with_field(RECIPIENT_POINTER, "address", &detail),
            Error::Signature { backend } => moso_core::Error::new(ErrorKind::Unauthenticated)
                .with_detail(format!("the {backend} webhook signature did not verify")),
            Error::Unavailable { .. } => moso_core::Error::unavailable(message),
            // 504 rather than 503: the provider is upstream of this process
            // and did not answer in time, which is precisely what a gateway
            // timeout means. `ErrorKind::GatewayTimeout` is retryable, so a
            // client's backoff and a job's backoff still agree.
            Error::Timeout { .. } => {
                moso_core::Error::new(ErrorKind::GatewayTimeout).with_detail(message)
            }
            // A template that will not render, a provider that will refuse the
            // message again, an operation the backend does not have and a
            // contradictory configuration are all bugs in this process. The
            // detail is suppressed outside development by `ErrorKind::Internal`.
            Error::Template { .. }
            | Error::Rejected { .. }
            | Error::Unsupported { .. }
            | Error::Config(_) => moso_core::Error::internal_msg(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Retrying a permanent failure is how a sending domain gets blocklisted,
    /// so the predicate is asserted variant by variant rather than trusted.
    #[test]
    fn only_an_unavailable_provider_is_worth_retrying() {
        assert!(Error::unavailable("smtp", "reset", None).retryable());
        assert!(Error::timeout("smtp", Duration::from_secs(30)).retryable());
        assert!(!Error::rejected("ses", "550").retryable());
        assert!(!Error::suppressed("a@b.com", "bounce").retryable());
        assert!(!Error::address("nope", "no `@`").retryable());
        assert!(!Error::template("t", "boom").retryable());
        assert!(!Error::unsupported("smtp", "send_batch").retryable());
        assert!(!Error::config("no url").retryable());
        assert!(!Error::Signature { backend: "ses" }.retryable());
    }

    /// A suppressed recipient is the caller's mistake and is fixable by the
    /// caller, so it must not become an opaque 500.
    #[test]
    fn a_suppressed_recipient_is_a_422_with_a_pointer() {
        let http: moso_core::Error = Error::suppressed("bounced@example.com", "hard bounce").into();
        assert_eq!(http.status(), http::StatusCode::UNPROCESSABLE_ENTITY);
        let fields = http.fields().expect("a field pointer");
        assert_eq!(fields.as_slice()[0].pointer, RECIPIENT_POINTER);
    }

    /// A provider outage is retryable at the HTTP layer too, so a client's
    /// backoff and the job queue's backoff agree.
    #[test]
    fn an_unreachable_provider_is_a_retryable_503() {
        let http: moso_core::Error = Error::unavailable("ses", "timed out", None).into();
        assert_eq!(http.status(), http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(http.retryable());
    }

    /// A send that ran out of time is a 504 and not a 503, because the thing
    /// that did not answer is upstream of this process. A caller that reads
    /// the status alone still learns it may retry.
    #[test]
    fn a_send_that_overran_its_deadline_is_a_retryable_504() {
        let error = Error::timeout("resend", Duration::from_millis(250));
        assert!(error.to_string().contains("250ms"), "{error}");

        let http: moso_core::Error = error.into();
        assert_eq!(http.status(), http::StatusCode::GATEWAY_TIMEOUT);
        assert!(http.retryable());
    }

    /// A forged webhook is an authentication failure, not a validation one:
    /// there is nothing in the body for the caller to fix.
    #[test]
    fn a_bad_webhook_signature_is_a_401() {
        let http: moso_core::Error = Error::Signature { backend: "mailgun" }.into();
        assert_eq!(http.status(), http::StatusCode::UNAUTHORIZED);
    }

    /// A broken template is this process's bug, and its detail names the
    /// template so an operator can find it in the log.
    #[test]
    fn a_broken_template_is_a_500_that_names_the_template() {
        let error = Error::template("emails/welcome.html", "undefined variable `user`");
        assert!(error.to_string().contains("emails/welcome.html"));
        let http: moso_core::Error = error.into();
        assert_eq!(http.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// Only the variants that came from a backend name one; a template error
    /// has no backend and must not invent one.
    #[test]
    fn the_backend_is_reported_only_where_there_was_one() {
        assert_eq!(Error::rejected("postmark", "x").backend(), Some("postmark"));
        assert_eq!(
            Error::unavailable("smtp", "x", None).backend(),
            Some("smtp")
        );
        assert_eq!(Error::unsupported("file", "probe").backend(), Some("file"));
        assert_eq!(
            Error::timeout("mailgun", Duration::from_secs(1)).backend(),
            Some("mailgun"),
        );
        assert_eq!(Error::Signature { backend: "ses" }.backend(), Some("ses"));
        assert_eq!(Error::template("t", "x").backend(), None);
        assert_eq!(Error::address("a", "x").backend(), None);
        assert_eq!(Error::suppressed("a", "x").backend(), None);
        assert_eq!(Error::config("x").backend(), None);
    }
}
