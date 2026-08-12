//! The per-send deadline, in one place, applied by every shipped backend.
//!
//! [`MailConfig::timeout`](crate::MailConfig::timeout) used to be a number an
//! application stored and nothing read. That is the worst kind of setting: it
//! looks like a guarantee, and a provider that accepts a connection and then
//! stops talking holds the send — and the job slot, or the request — open for
//! as long as the operating system's own keepalive allows, which is measured
//! in tens of minutes. A deadline that is configuration rather than a deadline
//! is a hang waiting for a bad afternoon at a provider.
//!
//! # Why one function and not one wrapper per backend
//!
//! Every backend calls [`within`] with its own name and its own configured
//! deadline, so the rule is written once and the error says which backend ran
//! out of time:
//!
//! ```text
//! send_rendered ─→ within(name, timeout, ────────────────────────┐
//!                     the whole conversation: connect, TLS,      │
//!                     auth, DATA, the provider's response        │
//!                  ) ──→ Ok(MessageId) | Err(Error::Timeout { .. })
//! ```
//!
//! The deadline covers the **whole** send and not one socket operation. A
//! per-write timeout is the classic mistake: an SMTP server that answers every
//! command one byte at a time never trips a write timeout and never finishes
//! either. Wrapping the entire future is the only formulation that bounds what
//! a caller actually waits for.
//!
//! What it does **not** cover is the composition around the backend.
//! [`Suppressing`](crate::backend::Suppressing) and
//! [`Redirecting`](crate::backend::Redirecting) delegate to an inner mailer
//! that has its own deadline, so a send is bounded; but a
//! [`SuppressionList`](crate::SuppressionList) of your own that queries a
//! database is your dependency, on your deadline, and this one says nothing
//! about it.
//!
//! # Why a timed-out send is retryable, and what that costs
//!
//! [`Error::Timeout`](crate::Error::Timeout) is retryable, because the common
//! cause is a provider that is briefly overloaded. But a send that timed out
//! may still have been accepted — the answer was lost, not the message. That
//! is exactly what [`MessageKey`](crate::MessageKey) is for: give a retryable
//! message an idempotency key and a duplicate becomes a no-op instead of a
//! second email.

use core::future::Future;
use std::time::Duration;

use crate::Result;

/// The per-send deadline when configuration does not say: 30 seconds.
///
/// Generous enough for a large attachment over a slow link, and short enough
/// that a stuck send is noticed within one job attempt rather than at the end
/// of the shift.
///
/// ```
/// assert_eq!(moso_mail::deadline::DEFAULT_TIMEOUT.as_secs(), 30);
/// ```
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Run one send under `timeout`, naming `backend` if it overruns.
///
/// This is what every shipped backend wraps its `send_rendered` body in, and
/// what a hand-written [`Mailer`](crate::Mailer) should wrap its own in — a
/// backend that skips it is a backend that can hang a worker.
///
/// # Errors
///
/// [`Error::Timeout`](crate::Error::Timeout) naming `backend` and `timeout`
/// when the future has not finished by the deadline, and whatever the future
/// itself reports when it finishes in time.
///
/// ```
/// use std::time::Duration;
///
/// use moso_mail::{MessageId, deadline};
///
/// let runtime = tokio::runtime::Runtime::new()?;
/// let id = runtime.block_on(async {
///     // A send that finishes in time is handed back untouched.
///     deadline::within("memory", Duration::from_secs(5), async {
///         Ok(MessageId::new("m1"))
///     })
///     .await
/// })?;
/// assert_eq!(id.as_str(), "m1");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub async fn within<T, F>(backend: &'static str, timeout: Duration, send: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match tokio::time::timeout(timeout, send).await {
        Ok(outcome) => outcome,
        Err(_elapsed) => Err(crate::Error::timeout(backend, timeout)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole module exists for: a transport that never
    /// answers returns, rather than holding the caller until the kernel gives
    /// up on the socket.
    #[tokio::test]
    async fn a_send_that_never_finishes_becomes_a_timeout_naming_the_backend() {
        let error = within::<(), _>("smtp", Duration::from_millis(20), async {
            std::future::pending::<()>().await;
            Ok(())
        })
        .await
        .expect_err("the deadline fires");

        assert!(matches!(error, crate::Error::Timeout { .. }));
        assert_eq!(error.backend(), Some("smtp"));
        // The message names both halves an operator needs: which backend, and
        // how long it was given.
        assert_eq!(error.to_string(), "smtp did not finish sending within 20ms");
    }

    /// A deadline that does not fire must be invisible: the send's own outcome
    /// is what the caller sees, success or failure.
    #[tokio::test]
    async fn a_send_that_finishes_in_time_is_handed_back_untouched() {
        let ok = within("memory", Duration::from_secs(30), async {
            Ok(crate::MessageId::new("m1"))
        })
        .await
        .expect("finishes in time");
        assert_eq!(ok.as_str(), "m1");

        let error = within::<(), _>("ses", Duration::from_secs(30), async {
            Err(crate::Error::rejected("ses", "550 rejected"))
        })
        .await
        .expect_err("the send itself failed");
        assert!(matches!(error, crate::Error::Rejected { .. }));
    }

    /// A job's retry policy branches on `retryable`, and a provider that ran
    /// out of time is the case retrying was invented for.
    #[tokio::test]
    async fn a_timed_out_send_is_retryable() {
        let error = within::<(), _>("resend", Duration::ZERO, async {
            std::future::pending::<()>().await;
            Ok(())
        })
        .await
        .expect_err("zero is a deadline like any other");
        assert!(error.retryable());
    }
}
