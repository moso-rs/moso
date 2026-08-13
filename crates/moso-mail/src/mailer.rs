//! The [`Mailer`] trait and what a backend says it can do.
//!
//! # Why the trait is dyn-compatible
//!
//! An application injects `Inject<dyn Mailer>` and never names the backend, so
//! the same code sends through the console in `moso dev`, through memory in a
//! test and through SES in production. That requires a trait object, which
//! requires boxed futures (decision D4). The cost is one allocation per send,
//! against a network round trip.

use moso_core::BoxFuture;

use crate::{Email, MessageId, RenderedEmail, Result};

/// What a backend can and cannot do.
///
/// Not decoration: [`Mailer::send_batch`] is *optional*, and a caller that
/// batches without checking gets an
/// [`Error::Unsupported`](crate::Error::Unsupported) rather than a silent loop
/// of single sends. Declaring the limits also lets the framework reject an
/// oversized attachment before it is uploaded to a provider that would refuse
/// it.
///
/// ```
/// use moso_mail::MailCapabilities;
///
/// // The conservative default: send one message, no extras.
/// let caps = MailCapabilities::minimal();
/// assert!(!caps.batching);
/// assert!(caps.attachments);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MailCapabilities {
    /// Whether [`Mailer::send_batch`] does anything better than a loop.
    pub batching: bool,
    /// The largest batch the provider accepts. Zero when `batching` is false.
    pub max_batch: usize,
    /// Whether the provider renders its own templates.
    pub templates: bool,
    /// Whether open/click tracking is available.
    pub tracking: bool,
    /// Whether attachments are supported at all.
    pub attachments: bool,
    /// The largest single attachment, in bytes.
    pub max_attachment_bytes: u64,
    /// The largest recipient count in one message.
    pub max_recipients: usize,
    /// Whether provider-side analytics tags are carried.
    pub tags: bool,
    /// Whether the provider posts delivery webhooks Moso can verify.
    pub webhooks: bool,
    /// Whether the provider honours a send-at time.
    pub scheduling: bool,
    /// Whether the provider deduplicates on
    /// [`MessageKey`](crate::MessageKey), so retries are free.
    pub idempotency: bool,
}

impl MailCapabilities {
    /// The conservative set: one message at a time, attachments, nothing else.
    ///
    /// A new backend should start here and turn things on as it implements
    /// them, so an unimplemented feature is an honest `false` rather than a
    /// runtime surprise.
    ///
    /// ```
    /// use moso_mail::MailCapabilities;
    ///
    /// assert_eq!(MailCapabilities::minimal().max_batch, 0);
    /// ```
    #[must_use]
    pub const fn minimal() -> Self {
        Self {
            batching: false,
            max_batch: 0,
            templates: false,
            tracking: false,
            attachments: true,
            max_attachment_bytes: 10 * 1024 * 1024,
            max_recipients: 50,
            tags: false,
            webhooks: false,
            scheduling: false,
            idempotency: false,
        }
    }
}

impl Default for MailCapabilities {
    fn default() -> Self {
        Self::minimal()
    }
}

/// Sends mail. The one trait an application depends on.
///
/// ```no_run
/// use moso_mail::{Email, Mailer, MessageId};
///
/// async fn notify(mailer: &dyn Mailer, message: &dyn Email) -> moso_mail::Result<MessageId> {
///     mailer.send(message).await
/// }
/// ```
///
/// # Sending from a request handler
///
/// Do not. SMTP inside a request is a latency and reliability trap: the user
/// waits for a third party, and a provider outage becomes a 500 on signup. The
/// shipped pattern is to enqueue a [`RenderedEmail`] as a `moso-jobs` payload
/// and call [`send_rendered`](Mailer::send_rendered) from the worker — retries
/// and the dead-letter queue then come for free. `moso-mail` deliberately does
/// not depend on `moso-jobs` to do this; the payload type is the whole seam.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a mailer",
    label = "not a mailer",
    note = "a mailer is `Send + Sync + 'static` and implements `name`, `capabilities` and \
            `send_rendered`",
    note = "help: use a shipped backend — `ConsoleMailer` in development, `MemoryMailer` in \
            tests, `SmtpMailer` or a provider backend in production",
    note = "help: to write your own, `impl Mailer for {Self}` and start from \
            `MailCapabilities::minimal()`"
)]
pub trait Mailer: Send + Sync + 'static {
    /// The backend's name, for logs, metrics and error messages.
    ///
    /// A short lowercase word: `"smtp"`, `"ses"`, `"console"`.
    fn name(&self) -> &'static str;

    /// What this backend supports.
    fn capabilities(&self) -> MailCapabilities;

    /// Send an already-rendered message. The primitive every other method uses.
    ///
    /// # Errors
    ///
    /// [`Error::Suppressed`](crate::Error::Suppressed) when a recipient is on
    /// the suppression list, [`Error::Rejected`](crate::Error::Rejected) for a
    /// permanent provider refusal, and
    /// [`Error::Unavailable`](crate::Error::Unavailable) for anything worth
    /// retrying.
    fn send_rendered<'a>(&'a self, message: &'a RenderedEmail) -> BoxFuture<'a, Result<MessageId>>;

    /// Render and send.
    ///
    /// # Errors
    ///
    /// Everything [`send_rendered`](Mailer::send_rendered) reports, plus
    /// [`Error::Template`](crate::Error::Template) from rendering.
    fn send<'a>(&'a self, message: &'a dyn Email) -> BoxFuture<'a, Result<MessageId>> {
        Box::pin(async move {
            let rendered = RenderedEmail::render(message)?;
            self.send_rendered(&rendered).await
        })
    }

    /// Send many messages, using the provider's batch API when there is one.
    ///
    /// The outer result fails only when the batch could not be attempted; each
    /// message's own outcome is in the vector, in the order given.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error::Unsupported) when
    /// [`MailCapabilities::batching`] is false and the caller asked for more
    /// than [`MailCapabilities::max_batch`] messages.
    fn send_batch<'a>(
        &'a self,
        messages: &'a [RenderedEmail],
    ) -> BoxFuture<'a, Result<Vec<Result<MessageId>>>> {
        Box::pin(async move {
            let capabilities = self.capabilities();
            // A backend that cannot batch says so, rather than looping and
            // letting a caller believe it made one request. The loop is still
            // available — it just has to be asked for one message at a time.
            if !capabilities.batching && messages.len() > 1 {
                return Err(crate::Error::unsupported(self.name(), "send_batch"));
            }
            if capabilities.batching && messages.len() > capabilities.max_batch {
                return Err(crate::Error::unsupported(self.name(), "send_batch"));
            }

            let mut outcomes = Vec::with_capacity(messages.len());
            for message in messages {
                outcomes.push(self.send_rendered(message).await);
            }
            Ok(outcomes)
        })
    }

    /// Send without any queueing wrapper, whatever the composition around it.
    ///
    /// Identical to [`send`](Mailer::send) on a bare backend. It exists so an
    /// application that wrapped its mailer in a queueing decorator can still
    /// say "this one goes now" — a password reset, typically — at the call
    /// site instead of in configuration.
    ///
    /// # Errors
    ///
    /// As [`send`](Mailer::send).
    fn send_now<'a>(&'a self, message: &'a dyn Email) -> BoxFuture<'a, Result<MessageId>> {
        self.send(message)
    }

    /// A readiness probe: can this backend reach its provider right now?
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) when it cannot.
    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        // A backend with no remote is always reachable. One that has a remote
        // overrides this; leaving the default in place would be the dishonest
        // choice, and is why every shipped remote backend does.
        Box::pin(async { Ok(()) })
    }
}
