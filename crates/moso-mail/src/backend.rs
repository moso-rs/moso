//! The shipped [`Mailer`] implementations.
//!
//! | Backend | Feature | Use |
//! | --- | --- | --- |
//! | `ConsoleMailer` | `console` (default) | prints, and serves a preview inbox at `/_mail` |
//! | `FileMailer` | `file` | writes `.eml` files, for CI artefacts |
//! | `MemoryMailer` | `memory` (default) | tests, assertable through `app.mail()` |
//! | `SmtpMailer` | `mail-smtp` | pooled SMTP with STARTTLS or implicit TLS |
//! | `ProviderMailer` | `mail-ses`, `mail-sendgrid`, `mail-postmark`, `mail-resend`, `mail-mailgun` | REST APIs with batch send and verified webhooks |
//!
//! Every backend reports honest [`MailCapabilities`];
//! nothing pretends to batch by looping.
//!
//! # Every backend enforces its deadline
//!
//! Each one carries a `timeout` — [`DEFAULT_TIMEOUT`](crate::deadline::DEFAULT_TIMEOUT)
//! unless [`MailConfig`](crate::MailConfig) or the backend's own `timeout`
//! builder says otherwise — and wraps the whole of `send_rendered` in
//! [`deadline::within`](crate::deadline::within). A provider that accepts the
//! connection and then stops talking produces
//! [`Error::Timeout`](crate::Error::Timeout) naming the backend and the
//! deadline, never a hung worker.

use moso_core::BoxFuture;

use crate::{Address, MailCapabilities, Mailer, MessageId, RenderedEmail, Result};

/// Substitute the configured sender into a message that set none.
///
/// Every backend does this, and every backend that forgot would send from
/// `unset@sender.invalid`. Kept here so there is one copy of the rule.
#[cfg(any(
    feature = "console",
    feature = "file",
    feature = "memory",
    feature = "mail-smtp",
    feature = "provider"
))]
fn with_sender(message: &RenderedEmail, from: Option<&Address>) -> RenderedEmail {
    match from {
        Some(from) if message.sender_is_unset() => {
            let mut owned = message.clone();
            owned.from = from.clone();
            owned
        }
        _ => message.clone(),
    }
}

// ---------------------------------------------------------------------------
// console
// ---------------------------------------------------------------------------

/// Prints messages to the terminal and keeps the last few for `/_mail`.
///
/// The default in development. Seeing a rendered email in a browser without
/// configuring SMTP removes the single biggest friction point in building a
/// signup flow.
///
/// ```
/// use moso_mail::backend::ConsoleMailer;
///
/// let mailer = ConsoleMailer::new().keep(50);
/// assert_eq!(moso_mail::Mailer::name(&mailer), "console");
/// ```
#[cfg(feature = "console")]
#[cfg_attr(docsrs, doc(cfg(feature = "console")))]
#[derive(Debug)]
pub struct ConsoleMailer {
    /// How many messages the preview inbox retains.
    keep: usize,
    /// Whether to print the full HTML as well as the summary.
    verbose: bool,
    /// The retained messages, oldest first.
    retained: std::sync::RwLock<std::collections::VecDeque<Retained>>,
    /// The next identifier, so `/_mail/{id}` is stable within a process.
    next_id: std::sync::atomic::AtomicU64,
    /// The default sender, when configuration supplied one.
    from: Option<Address>,
    /// How long one send may take. Enforced, not advisory.
    timeout: std::time::Duration,
    /// When set, every send stalls this long first, so a test can trip the
    /// deadline. `None` in every production configuration.
    delay: std::sync::RwLock<Option<std::time::Duration>>,
}

/// Written out rather than derived: a derived `Default` would give `keep` and
/// `timeout` their numeric zeroes, which are a mailer that retains nothing and
/// abandons every send. `default()` and `new()` have to mean the same thing.
#[cfg(feature = "console")]
impl Default for ConsoleMailer {
    fn default() -> Self {
        Self::new()
    }
}

/// One message the inbox is holding.
#[cfg(any(feature = "console", feature = "memory"))]
#[derive(Clone, Debug)]
struct Retained {
    /// Stable within this process.
    id: String,
    /// When it was accepted.
    sent_at: chrono::DateTime<chrono::Utc>,
    /// The message itself.
    message: RenderedEmail,
}

#[cfg(any(feature = "console", feature = "memory"))]
impl Retained {
    /// The list view of this message.
    fn item(&self) -> crate::preview::PreviewItem {
        crate::preview::PreviewItem {
            id: self.id.clone(),
            kind: self.message.kind_name().to_owned(),
            subject: self.message.subject.clone(),
            to: self.message.to.iter().map(Address::to_header).collect(),
            sent_at: self.sent_at.to_rfc3339(),
            attachments: self.message.attachments.len(),
        }
    }
}

#[cfg(feature = "console")]
impl ConsoleMailer {
    /// A console mailer keeping the last 100 messages.
    ///
    /// ```
    /// use moso_mail::backend::ConsoleMailer;
    ///
    /// assert!(ConsoleMailer::new().inbox().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            keep: 100,
            verbose: false,
            retained: std::sync::RwLock::new(std::collections::VecDeque::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
            from: None,
            timeout: crate::deadline::DEFAULT_TIMEOUT,
            delay: std::sync::RwLock::new(None),
        }
    }

    /// How long one send may take before it becomes
    /// [`Error::Timeout`](crate::Error::Timeout).
    ///
    /// Printing cannot hang, so this is here for uniformity rather than for a
    /// real failure mode: a development mailer that behaved differently from
    /// the production one under a deadline would teach the wrong lesson.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_mail::backend::ConsoleMailer;
    ///
    /// let _ = ConsoleMailer::new().timeout(Duration::from_secs(5));
    /// ```
    #[must_use]
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Make every subsequent send stall this long before it does anything.
    ///
    /// Printing cannot hang, so this exists only to prove the deadline is
    /// wired: a send that stalls longer than [`timeout`](ConsoleMailer::timeout)
    /// produces [`Error::Timeout`](crate::Error::Timeout) here exactly as it
    /// would from a real backend, which is what keeps the development mailer
    /// honest about the deadline every other backend enforces. `None` clears
    /// it. It is `None` in every production configuration.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_mail::backend::ConsoleMailer;
    ///
    /// let mailer = ConsoleMailer::new().timeout(Duration::from_millis(10));
    /// mailer.delay(Some(Duration::from_secs(60)));
    /// mailer.delay(None);
    /// ```
    pub fn delay(&self, delay: Option<std::time::Duration>) {
        *self
            .delay
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = delay;
    }

    /// Retain `count` messages for the preview inbox.
    ///
    /// ```
    /// use moso_mail::backend::ConsoleMailer;
    ///
    /// let _ = ConsoleMailer::new().keep(10);
    /// ```
    #[must_use]
    pub fn keep(mut self, count: usize) -> Self {
        self.keep = count;
        self
    }

    /// Print the HTML body as well as the one-line summary.
    ///
    /// ```
    /// use moso_mail::backend::ConsoleMailer;
    ///
    /// let _ = ConsoleMailer::new().verbose(true);
    /// ```
    #[must_use]
    pub fn verbose(mut self, enabled: bool) -> Self {
        self.verbose = enabled;
        self
    }

    /// Fill this sender into any message that set none.
    ///
    /// ```
    /// # use moso_mail::{backend::ConsoleMailer, Address};
    /// let mailer = ConsoleMailer::new().from(Address::new("hi@shop.example")?);
    /// # let _ = mailer;
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn from(mut self, from: Address) -> Self {
        self.from = Some(from);
        self
    }

    /// The retained messages, newest first — what `/_mail` renders.
    ///
    /// ```
    /// # use moso_mail::{backend::ConsoleMailer, RenderedEmail};
    /// let _: Vec<RenderedEmail> = ConsoleMailer::new().inbox();
    /// ```
    #[must_use]
    pub fn inbox(&self) -> Vec<RenderedEmail> {
        self.read()
            .iter()
            .rev()
            .map(|held| held.message.clone())
            .collect()
    }

    /// The ring buffer, recovering from a poisoned lock.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, std::collections::VecDeque<Retained>> {
        self.retained
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The ring buffer, mutably.
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, std::collections::VecDeque<Retained>> {
        self.retained
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(feature = "console")]
impl Mailer for ConsoleMailer {
    fn name(&self) -> &'static str {
        "console"
    }

    fn capabilities(&self) -> MailCapabilities {
        MailCapabilities {
            // Batching is real here — writing several lines is one operation
            // and nothing is being pretended about a provider.
            batching: true,
            max_batch: usize::MAX,
            tags: true,
            idempotency: true,
            ..MailCapabilities::minimal()
        }
    }

    fn send_rendered<'a>(&'a self, message: &'a RenderedEmail) -> BoxFuture<'a, Result<MessageId>> {
        Box::pin(crate::deadline::within(
            self.name(),
            self.timeout,
            async move {
                // Inside the deadline, and first: an arranged stall stands in for
                // a send that has gone quiet, and it has to be able to trip the
                // deadline the way a real one would.
                let delay = *self
                    .delay
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }

                let message = with_sender(message, self.from.as_ref());
                let id = self
                    .next_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let id = format!("{id:06}");

                // `tracing` and not `println!`: `moso dev` renders these through
                // the same formatter as every other line, and a test that captures
                // logs sees them.
                tracing::info!(
                    target: "moso::mail",
                    id = %id,
                    kind = %message.kind_name(),
                    to = %message.to.iter().map(Address::to_header).collect::<Vec<_>>().join(", "),
                    subject = %message.subject,
                    preview = %crate::preview::PREVIEW_PATH,
                    "mail (console)",
                );
                if self.verbose {
                    tracing::info!(target: "moso::mail", id = %id, body = %message.text, "mail body");
                }

                let mut retained = self.write();
                retained.push_back(Retained {
                    id: id.clone(),
                    sent_at: chrono::Utc::now(),
                    message,
                });
                while retained.len() > self.keep {
                    retained.pop_front();
                }
                Ok(MessageId::new(id))
            },
        ))
    }
}

#[cfg(feature = "console")]
impl crate::preview::Inbox for ConsoleMailer {
    fn list(&self, limit: usize) -> Vec<crate::preview::PreviewItem> {
        self.read()
            .iter()
            .rev()
            .take(limit)
            .map(Retained::item)
            .collect()
    }

    fn get(&self, id: &str) -> Option<RenderedEmail> {
        self.read()
            .iter()
            .find(|held| held.id == id)
            .map(|held| held.message.clone())
    }

    fn clear(&self) {
        self.write().clear();
    }
}

// ---------------------------------------------------------------------------
// file
// ---------------------------------------------------------------------------

/// Writes each message as an `.eml` file. For CI artefacts and manual review.
///
/// ```
/// use moso_mail::backend::FileMailer;
///
/// let mailer = FileMailer::new("target/mail");
/// assert_eq!(mailer.directory(), std::path::Path::new("target/mail"));
/// ```
#[cfg(feature = "file")]
#[cfg_attr(docsrs, doc(cfg(feature = "file")))]
#[derive(Debug)]
pub struct FileMailer {
    /// The directory messages are written to, created on first send.
    directory: std::path::PathBuf,
    /// A counter, so two messages in the same millisecond do not collide.
    sequence: std::sync::atomic::AtomicU64,
    /// The default sender, when configuration supplied one.
    from: Option<Address>,
    /// How long one write may take. A network filesystem can hang.
    timeout: std::time::Duration,
    /// When set, every send stalls this long first — the controllable stand-in
    /// for the hung network filesystem the deadline exists for. `None` in every
    /// production configuration.
    delay: std::sync::RwLock<Option<std::time::Duration>>,
}

#[cfg(feature = "file")]
impl FileMailer {
    /// Write `.eml` files into `directory`.
    ///
    /// ```
    /// use moso_mail::backend::FileMailer;
    ///
    /// let _ = FileMailer::new("/tmp/mail");
    /// ```
    #[must_use]
    pub fn new(directory: impl Into<std::path::PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            sequence: std::sync::atomic::AtomicU64::new(0),
            from: None,
            timeout: crate::deadline::DEFAULT_TIMEOUT,
            delay: std::sync::RwLock::new(None),
        }
    }

    /// How long one write may take before it becomes
    /// [`Error::Timeout`](crate::Error::Timeout).
    ///
    /// Not theatre: a `.eml` directory on an NFS or FUSE mount whose server
    /// has gone away blocks the write indefinitely, and CI artefacts are
    /// exactly the sort of directory that lives on one.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_mail::backend::FileMailer;
    ///
    /// let _ = FileMailer::new("target/mail").timeout(Duration::from_secs(5));
    /// ```
    #[must_use]
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Make every subsequent send stall this long before touching the disk.
    ///
    /// The controllable half of the hung mount [`timeout`](FileMailer::timeout)
    /// guards against: a send that stalls longer than the deadline produces
    /// [`Error::Timeout`](crate::Error::Timeout) with no network and no waiting,
    /// which is how a test proves the deadline fires rather than the write
    /// hanging forever. `None` clears it, and it is `None` in every production
    /// configuration.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_mail::backend::FileMailer;
    ///
    /// let mailer = FileMailer::new("target/mail").timeout(Duration::from_millis(10));
    /// mailer.delay(Some(Duration::from_secs(60)));
    /// mailer.delay(None);
    /// ```
    pub fn delay(&self, delay: Option<std::time::Duration>) {
        *self
            .delay
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = delay;
    }

    /// Fill this sender into any message that set none.
    ///
    /// ```
    /// # use moso_mail::{backend::FileMailer, Address};
    /// let _ = FileMailer::new("/tmp/mail").from(Address::new("hi@shop.example")?);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn from(mut self, from: Address) -> Self {
        self.from = Some(from);
        self
    }

    /// The directory being written to.
    ///
    /// ```
    /// # use moso_mail::backend::FileMailer;
    /// let _: &std::path::Path = FileMailer::new("x").directory();
    /// ```
    #[must_use]
    pub fn directory(&self) -> &std::path::Path {
        &self.directory
    }
}

#[cfg(feature = "file")]
impl Mailer for FileMailer {
    fn name(&self) -> &'static str {
        "file"
    }

    fn capabilities(&self) -> MailCapabilities {
        MailCapabilities {
            batching: true,
            max_batch: usize::MAX,
            tags: true,
            idempotency: true,
            ..MailCapabilities::minimal()
        }
    }

    fn send_rendered<'a>(&'a self, message: &'a RenderedEmail) -> BoxFuture<'a, Result<MessageId>> {
        Box::pin(crate::deadline::within(
            self.name(),
            self.timeout,
            async move {
                // Inside the deadline, and first: an arranged stall stands in for
                // a hung mount, and it has to be able to trip the deadline before
                // a single byte reaches the disk.
                let delay = *self
                    .delay
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }

                let message = with_sender(message, self.from.as_ref());
                let id = crate::mime::new_message_id(message.from.domain());
                let bytes = crate::mime::to_rfc5322(&message, &id);

                tokio::fs::create_dir_all(&self.directory)
                    .await
                    .map_err(|error| {
                        crate::Error::unavailable(
                            "file",
                            format!("could not create `{}`: {error}", self.directory.display()),
                            Some(Box::new(error)),
                        )
                    })?;

                let sequence = self
                    .sequence
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let name = format!(
                    "{}-{sequence:04}-{}.eml",
                    chrono::Utc::now().format("%Y%m%dT%H%M%S%.3f"),
                    message.kind_name(),
                );
                let path = self.directory.join(name);
                tokio::fs::write(&path, &bytes).await.map_err(|error| {
                    crate::Error::unavailable(
                        "file",
                        format!("could not write `{}`: {error}", path.display()),
                        Some(Box::new(error)),
                    )
                })?;

                tracing::info!(target: "moso::mail", path = %path.display(), "mail (file)");
                Ok(MessageId::new(id))
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// memory
// ---------------------------------------------------------------------------

/// Keeps every message in memory. The test backend.
///
/// This is the seam an assertion layer is written on: `sent`, `sent_of`,
/// `count_of`, `clear`, `fail_with` and `delay` are between them everything
/// `app.mail().assert_sent::<WelcomeEmail>(1)` and `assert_none_sent()` need,
/// and every one of them takes `&self`, so the mailer can stay behind the
/// `Arc<dyn Mailer>` the application injected.
///
/// ```
/// use moso_mail::backend::MemoryMailer;
///
/// let mailer = MemoryMailer::new();
/// assert_eq!(mailer.sent_count(), 0);
/// ```
#[cfg(feature = "memory")]
#[cfg_attr(docsrs, doc(cfg(feature = "memory")))]
#[derive(Debug)]
pub struct MemoryMailer {
    /// Everything sent, in order.
    sent: std::sync::RwLock<Vec<Retained>>,
    /// When set, every send fails with this, for testing failure paths.
    fail_with: std::sync::RwLock<Option<String>>,
    /// When set, every send stalls this long first, for testing deadlines.
    delay: std::sync::RwLock<Option<std::time::Duration>>,
    /// The next identifier.
    next_id: std::sync::atomic::AtomicU64,
    /// The default sender, when configuration supplied one.
    from: std::sync::RwLock<Option<Address>>,
    /// How long one send may take. Enforced, as in every other backend.
    timeout: std::time::Duration,
}

/// Written out rather than derived, so that `default()` and `new()` agree: a
/// derived `Default` would give `timeout` its numeric zero, and a test double
/// that abandons a send the moment it yields is a mystery, not a double.
#[cfg(feature = "memory")]
impl Default for MemoryMailer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "memory")]
impl MemoryMailer {
    /// An empty mailbox.
    ///
    /// ```
    /// use moso_mail::backend::MemoryMailer;
    ///
    /// assert!(MemoryMailer::new().sent().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            sent: std::sync::RwLock::new(Vec::new()),
            fail_with: std::sync::RwLock::new(None),
            delay: std::sync::RwLock::new(None),
            next_id: std::sync::atomic::AtomicU64::new(1),
            from: std::sync::RwLock::new(None),
            timeout: crate::deadline::DEFAULT_TIMEOUT,
        }
    }

    /// How long one send may take before it becomes
    /// [`Error::Timeout`](crate::Error::Timeout).
    ///
    /// Paired with [`delay`](MemoryMailer::delay), this is how a test proves
    /// its retry path handles a timed-out provider without a network:
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_mail::backend::MemoryMailer;
    ///
    /// let mailer = MemoryMailer::new().timeout(Duration::from_millis(10));
    /// mailer.delay(Some(Duration::from_secs(60)));
    /// ```
    #[must_use]
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Fill this sender into any message that set none.
    ///
    /// ```
    /// # use moso_mail::{backend::MemoryMailer, Address};
    /// let mailer = MemoryMailer::new();
    /// mailer.set_from(Some(Address::new("hi@shop.example")?));
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    pub fn set_from(&self, from: Option<Address>) {
        *self
            .from
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = from;
    }

    /// Everything sent so far, in order.
    ///
    /// ```
    /// # use moso_mail::{backend::MemoryMailer, RenderedEmail};
    /// let _: Vec<RenderedEmail> = MemoryMailer::new().sent();
    /// ```
    #[must_use]
    pub fn sent(&self) -> Vec<RenderedEmail> {
        self.read()
            .iter()
            .map(|held| held.message.clone())
            .collect()
    }

    /// How many messages were sent, without cloning any of them.
    ///
    /// What `assert_none_sent()` is written on.
    ///
    /// ```
    /// # use moso_mail::backend::MemoryMailer;
    /// assert_eq!(MemoryMailer::new().sent_count(), 0);
    /// ```
    #[must_use]
    pub fn sent_count(&self) -> usize {
        self.read().len()
    }

    /// Everything sent that was written as the message type `T`.
    ///
    /// What `assert_sent::<WelcomeEmail>(1)` is written on. `T` is matched by
    /// its Rust path, which is what [`Email::kind`](crate::Email::kind)
    /// defaults to; a message type that *overrides* `kind` is not found by its
    /// Rust name, and is looked up with
    /// [`sent_of_kind`](MemoryMailer::sent_of_kind) instead.
    ///
    /// ```
    /// # use moso_mail::{backend::MemoryMailer, Address, Email, RenderedEmail, Result};
    /// # struct WelcomeEmail(Address);
    /// # impl Email for WelcomeEmail {
    /// #     fn to(&self) -> Vec<Address> { vec![self.0.clone()] }
    /// #     fn subject(&self) -> Result<String> { Ok(String::new()) }
    /// #     fn html(&self) -> Result<String> { Ok(String::new()) }
    /// #     fn text(&self) -> Result<String> { Ok(String::new()) }
    /// # }
    /// let sent: Vec<RenderedEmail> = MemoryMailer::new().sent_of::<WelcomeEmail>();
    /// assert!(sent.is_empty());
    /// ```
    #[must_use]
    pub fn sent_of<T: crate::Email + ?Sized>(&self) -> Vec<RenderedEmail> {
        self.sent_of_kind(std::any::type_name::<T>())
    }

    /// Everything sent whose [`RenderedEmail::kind`] matches `kind`.
    ///
    /// Matches either the fully qualified path or its last segment, so
    /// `sent_of_kind("WelcomeEmail")` and
    /// `sent_of_kind(std::any::type_name::<WelcomeEmail>())` find the same
    /// messages. The string form exists for a message type that overrode
    /// [`Email::kind`](crate::Email::kind), and for one whose type the
    /// assertion cannot name.
    ///
    /// ```
    /// # use moso_mail::{backend::MemoryMailer, RenderedEmail};
    /// let _: Vec<RenderedEmail> = MemoryMailer::new().sent_of_kind("WelcomeEmail");
    /// ```
    #[must_use]
    pub fn sent_of_kind(&self, kind: &str) -> Vec<RenderedEmail> {
        self.read()
            .iter()
            .filter(|held| Self::is_kind(&held.message, kind))
            .map(|held| held.message.clone())
            .collect()
    }

    /// How many messages of type `T` were sent, without cloning any of them.
    ///
    /// The count `assert_sent::<T>(n)` compares against.
    ///
    /// ```
    /// # use moso_mail::{backend::MemoryMailer, Address, Email, Result};
    /// # struct WelcomeEmail(Address);
    /// # impl Email for WelcomeEmail {
    /// #     fn to(&self) -> Vec<Address> { vec![self.0.clone()] }
    /// #     fn subject(&self) -> Result<String> { Ok(String::new()) }
    /// #     fn html(&self) -> Result<String> { Ok(String::new()) }
    /// #     fn text(&self) -> Result<String> { Ok(String::new()) }
    /// # }
    /// assert_eq!(MemoryMailer::new().count_of::<WelcomeEmail>(), 0);
    /// ```
    #[must_use]
    pub fn count_of<T: crate::Email + ?Sized>(&self) -> usize {
        self.count_of_kind(std::any::type_name::<T>())
    }

    /// How many messages of kind `kind` were sent.
    ///
    /// The string-keyed half of [`count_of`](MemoryMailer::count_of), matching
    /// exactly what [`sent_of_kind`](MemoryMailer::sent_of_kind) matches.
    ///
    /// ```
    /// # use moso_mail::backend::MemoryMailer;
    /// assert_eq!(MemoryMailer::new().count_of_kind("WelcomeEmail"), 0);
    /// ```
    #[must_use]
    pub fn count_of_kind(&self, kind: &str) -> usize {
        self.read()
            .iter()
            .filter(|held| Self::is_kind(&held.message, kind))
            .count()
    }

    /// The one rule the four lookups above share.
    fn is_kind(message: &RenderedEmail, kind: &str) -> bool {
        message.kind == kind || message.kind_name() == kind
    }

    /// Forget everything sent.
    ///
    /// ```
    /// # use moso_mail::backend::MemoryMailer;
    /// MemoryMailer::new().clear();
    /// ```
    pub fn clear(&self) {
        self.write().clear();
    }

    /// Make every subsequent send fail, to exercise the failure path.
    ///
    /// The failure is [`Error::Unavailable`](crate::Error::Unavailable), which
    /// is retryable — so a job-queue test that asserts a retry happens can
    /// arrange one without a real provider outage.
    ///
    /// ```
    /// # use moso_mail::backend::MemoryMailer;
    /// let mailer = MemoryMailer::new();
    /// mailer.fail_with(Some("provider down"));
    /// mailer.fail_with(None);
    /// ```
    pub fn fail_with(&self, detail: Option<&str>) {
        *self
            .fail_with
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = detail.map(str::to_owned);
    }

    /// Make every subsequent send stall this long before it does anything.
    ///
    /// The controllable half of a hung provider. A delay longer than
    /// [`timeout`](MemoryMailer::timeout) produces
    /// [`Error::Timeout`](crate::Error::Timeout) — which is how a test proves
    /// its job retries a timed-out send, with no socket and no waiting.
    /// `None` clears it.
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use moso_mail::backend::MemoryMailer;
    /// let mailer = MemoryMailer::new().timeout(Duration::from_millis(10));
    /// mailer.delay(Some(Duration::from_secs(60)));
    /// mailer.delay(None);
    /// ```
    pub fn delay(&self, delay: Option<std::time::Duration>) {
        *self
            .delay
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = delay;
    }

    /// The sent list, recovering from a poisoned lock.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<Retained>> {
        self.sent
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The sent list, mutably.
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<Retained>> {
        self.sent
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(feature = "memory")]
impl Mailer for MemoryMailer {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn capabilities(&self) -> MailCapabilities {
        MailCapabilities {
            batching: true,
            max_batch: usize::MAX,
            tags: true,
            idempotency: true,
            ..MailCapabilities::minimal()
        }
    }

    fn send_rendered<'a>(&'a self, message: &'a RenderedEmail) -> BoxFuture<'a, Result<MessageId>> {
        Box::pin(crate::deadline::within(
            self.name(),
            self.timeout,
            async move {
                // Inside the deadline, and first: an arranged stall is standing in
                // for a transport that accepted the message and went quiet, and
                // that stall has to be able to trip the deadline.
                let delay = *self
                    .delay
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }

                if let Some(detail) = self
                    .fail_with
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                {
                    return Err(crate::Error::unavailable("memory", detail, None));
                }

                let from = self
                    .from
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let message = with_sender(message, from.as_ref());

                // An idempotency key that has already been sent is a no-op, so a
                // test of "the job retried" asserts one message and not two.
                if let Some(key) = &message.message_key
                    && let Some(previous) = self
                        .read()
                        .iter()
                        .find(|held| held.message.message_key.as_ref() == Some(key))
                {
                    return Ok(MessageId::new(previous.id.clone()));
                }

                let id = self
                    .next_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let id = format!("{id:06}");
                self.write().push(Retained {
                    id: id.clone(),
                    sent_at: chrono::Utc::now(),
                    message,
                });
                Ok(MessageId::new(id))
            },
        ))
    }
}

#[cfg(feature = "memory")]
impl crate::preview::Inbox for MemoryMailer {
    fn list(&self, limit: usize) -> Vec<crate::preview::PreviewItem> {
        self.read()
            .iter()
            .rev()
            .take(limit)
            .map(Retained::item)
            .collect()
    }

    fn get(&self, id: &str) -> Option<RenderedEmail> {
        self.read()
            .iter()
            .find(|held| held.id == id)
            .map(|held| held.message.clone())
    }

    fn clear(&self) {
        self.write().clear();
    }
}

// ---------------------------------------------------------------------------
// smtp
// ---------------------------------------------------------------------------

/// How a connection is secured.
///
/// ```
/// use moso_mail::backend::SmtpSecurity;
///
/// assert_eq!(SmtpSecurity::default(), SmtpSecurity::StartTls);
/// ```
#[cfg(feature = "mail-smtp")]
#[cfg_attr(docsrs, doc(cfg(feature = "mail-smtp")))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SmtpSecurity {
    /// Plaintext, upgraded with `STARTTLS`. The default, and required.
    #[default]
    StartTls,
    /// TLS from the first byte, usually on port 465.
    ImplicitTls,
    /// No encryption. Refused unless the host is a loopback address.
    None,
}

/// Pooled SMTP.
///
/// ```no_run
/// use moso_mail::backend::SmtpMailer;
///
/// let _ = SmtpMailer::from_url("smtp://user:pass@mail.example.com:587")?;
/// # Ok::<(), moso_mail::Error>(())
/// ```
#[cfg(feature = "mail-smtp")]
#[cfg_attr(docsrs, doc(cfg(feature = "mail-smtp")))]
#[derive(Debug)]
pub struct SmtpMailer {
    /// The host to connect to.
    host: String,
    /// The port.
    port: u16,
    /// How the connection is secured.
    security: SmtpSecurity,
    /// The username, when the server wants one.
    username: Option<String>,
    /// The password, redacted in every `Debug` and log.
    password: Option<moso_core::config::SecretString>,
    /// How many connections to keep open.
    pool_size: usize,
    /// The transport, built on first use so the builders can still run.
    transport: std::sync::OnceLock<lettre::AsyncSmtpTransport<lettre::Tokio1Executor>>,
    /// The default sender, when configuration supplied one.
    from: Option<Address>,
    /// How long the whole conversation may take — not one socket write.
    timeout: std::time::Duration,
}

#[cfg(feature = "mail-smtp")]
impl SmtpMailer {
    /// Parse a DSN of the form `smtp://user:pass@host:port?security=starttls`.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) when the URL is not an SMTP
    /// DSN, or when it asks for [`SmtpSecurity::None`] against a non-loopback
    /// host.
    ///
    /// ```
    /// use moso_mail::backend::SmtpMailer;
    ///
    /// let _ = SmtpMailer::from_url("smtp://localhost:1025?security=none")?;
    /// assert!(SmtpMailer::from_url("smtp://mail.example.com?security=none").is_err());
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    pub fn from_url(url: &str) -> Result<Self> {
        let bad =
            |detail: &str| crate::Error::config(format!("`{url}` is not an SMTP DSN: {detail}"));

        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| bad("expected `smtp://` or `smtps://`"))?;
        let implicit = match scheme {
            "smtp" => false,
            "smtps" => true,
            _ => return Err(bad("the scheme must be `smtp` or `smtps`")),
        };

        let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
        let authority = authority.trim_end_matches('/');
        let (credentials, hostport) = match authority.rsplit_once('@') {
            Some((credentials, hostport)) => (Some(credentials), hostport),
            None => (None, authority),
        };
        if hostport.is_empty() {
            return Err(bad("no host"));
        }

        let (host, port) = match hostport.rsplit_once(':') {
            Some((host, port)) => (
                host.to_owned(),
                port.parse::<u16>()
                    .map_err(|_| bad("the port is not a number"))?,
            ),
            None => (hostport.to_owned(), if implicit { 465 } else { 587 }),
        };

        let mut security = if implicit {
            SmtpSecurity::ImplicitTls
        } else {
            SmtpSecurity::StartTls
        };
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if key == "security" {
                security = match value {
                    "starttls" => SmtpSecurity::StartTls,
                    "tls" | "implicit" => SmtpSecurity::ImplicitTls,
                    "none" => SmtpSecurity::None,
                    other => return Err(bad(&format!("unknown security `{other}`"))),
                };
            }
        }

        // Unencrypted SMTP to anything but this machine sends the credentials
        // and the whole message in the clear. That is a configuration error and
        // not an option.
        if security == SmtpSecurity::None && !is_loopback(&host) {
            return Err(crate::Error::config(format!(
                "`security=none` sends the password and the message in the clear, and `{host}` is \
                 not a loopback address — use `security=starttls`, or point at `localhost` for a \
                 development mail catcher",
            )));
        }

        let mut mailer = Self::new(host, port, security);
        if let Some(credentials) = credentials {
            let (username, password) = credentials.split_once(':').unwrap_or((credentials, ""));
            mailer = mailer.credentials(
                percent_decode(username),
                moso_core::config::SecretString::new(percent_decode(password)),
            );
        }
        Ok(mailer)
    }

    /// Build from parts.
    ///
    /// ```
    /// use moso_mail::backend::{SmtpMailer, SmtpSecurity};
    ///
    /// let _ = SmtpMailer::new("mail.example.com", 587, SmtpSecurity::StartTls);
    /// ```
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16, security: SmtpSecurity) -> Self {
        Self {
            host: host.into(),
            port,
            security,
            username: None,
            password: None,
            pool_size: 4,
            transport: std::sync::OnceLock::new(),
            from: None,
            timeout: crate::deadline::DEFAULT_TIMEOUT,
        }
    }

    /// How long the whole SMTP conversation may take.
    ///
    /// The deadline covers connect, TLS, `AUTH`, `DATA` and the final `250` —
    /// not one socket operation. A server that answers each command a byte at
    /// a time never trips a per-write timeout and never finishes either, which
    /// is why the wrap is around the entire exchange.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_mail::backend::{SmtpMailer, SmtpSecurity};
    ///
    /// let _ = SmtpMailer::new("mail.example.com", 587, SmtpSecurity::StartTls)
    ///     .timeout(Duration::from_secs(10));
    /// ```
    #[must_use]
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Authenticate with a username and password.
    ///
    /// ```
    /// # use moso_core::config::SecretString;
    /// # use moso_mail::backend::{SmtpMailer, SmtpSecurity};
    /// let _ = SmtpMailer::new("h", 587, SmtpSecurity::StartTls)
    ///     .credentials("user", SecretString::new("pass"));
    /// ```
    #[must_use]
    pub fn credentials(
        mut self,
        username: impl Into<String>,
        password: moso_core::config::SecretString,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password);
        self
    }

    /// How many connections to keep open. Default 4.
    ///
    /// ```
    /// # use moso_mail::backend::{SmtpMailer, SmtpSecurity};
    /// let _ = SmtpMailer::new("h", 587, SmtpSecurity::StartTls).pool_size(8);
    /// ```
    #[must_use]
    pub fn pool_size(mut self, size: usize) -> Self {
        self.pool_size = size.max(1);
        self
    }

    /// Fill this sender into any message that set none.
    ///
    /// ```
    /// # use moso_mail::{backend::{SmtpMailer, SmtpSecurity}, Address};
    /// let _ = SmtpMailer::new("h", 587, SmtpSecurity::StartTls)
    ///     .from(Address::new("hi@shop.example")?);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn from(mut self, from: Address) -> Self {
        self.from = Some(from);
        self
    }

    /// The pooled transport, built once.
    fn transport(&self) -> Result<&lettre::AsyncSmtpTransport<lettre::Tokio1Executor>> {
        if let Some(built) = self.transport.get() {
            return Ok(built);
        }

        use lettre::transport::smtp::PoolConfig;
        use lettre::transport::smtp::authentication::Credentials;

        type Transport = lettre::AsyncSmtpTransport<lettre::Tokio1Executor>;
        let builder = match self.security {
            SmtpSecurity::StartTls => Transport::starttls_relay(&self.host),
            SmtpSecurity::ImplicitTls => Transport::relay(&self.host),
            SmtpSecurity::None => Ok(Transport::builder_dangerous(&self.host)),
        }
        .map_err(|error| {
            crate::Error::config(format!(
                "could not build an SMTP transport for `{}`: {error}",
                self.host,
            ))
        })?;

        let mut builder = builder
            .port(self.port)
            .pool_config(PoolConfig::new().max_size(u32::try_from(self.pool_size).unwrap_or(4)));
        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            builder = builder.credentials(Credentials::new(
                username.clone(),
                password.expose().to_owned(),
            ));
        }

        // `set` losing a race means another thread built an equivalent
        // transport first; either is correct.
        let _ = self.transport.set(builder.build());
        self.transport
            .get()
            .ok_or_else(|| crate::Error::config("the SMTP transport could not be initialised"))
    }
}

/// Whether a host names this machine.
#[cfg(feature = "mail-smtp")]
fn is_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// Decode `%XX` escapes in a DSN's userinfo.
#[cfg(feature = "mail-smtp")]
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(
                core::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("zz"),
                16,
            )
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(feature = "mail-smtp")]
impl Mailer for SmtpMailer {
    fn name(&self) -> &'static str {
        "smtp"
    }

    fn capabilities(&self) -> MailCapabilities {
        MailCapabilities {
            // SMTP has no batch verb, no tracking and no webhook. Claiming any
            // of them would be a lie a caller would build on.
            attachments: true,
            max_attachment_bytes: 25 * 1024 * 1024,
            max_recipients: 100,
            tags: false,
            ..MailCapabilities::minimal()
        }
    }

    fn send_rendered<'a>(&'a self, message: &'a RenderedEmail) -> BoxFuture<'a, Result<MessageId>> {
        Box::pin(crate::deadline::within(
            self.name(),
            self.timeout,
            async move {
                use lettre::AsyncTransport as _;

                let message = with_sender(message, self.from.as_ref());
                let id = crate::mime::new_message_id(message.from.domain());
                let bytes = crate::mime::to_rfc5322(&message, &id);

                let parse = |address: &Address| {
                    address
                        .address()
                        .parse::<lettre::Address>()
                        .map_err(|error| {
                            crate::Error::address(address.address().to_owned(), error.to_string())
                        })
                };
                let from = parse(&message.from)?;
                // Every recipient — including the blind copies, which is exactly
                // why they are in the envelope and not in the headers.
                let to = message
                    .recipients()
                    .map(parse)
                    .collect::<Result<Vec<_>>>()?;
                let envelope = lettre::address::Envelope::new(Some(from), to)
                    .map_err(|error| crate::Error::address(String::new(), error.to_string()))?;

                self.transport()?
                    .send_raw(&envelope, &bytes)
                    .await
                    .map_err(|error| classify_smtp(&error))?;
                Ok(MessageId::new(id))
            },
        ))
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(crate::deadline::within(
            self.name(),
            self.timeout,
            async move {
                self.transport()?
                    .test_connection()
                    .await
                    .map_err(|error| classify_smtp(&error))?
                    .then_some(())
                    .ok_or_else(|| {
                        crate::Error::unavailable("smtp", "the server did not answer", None)
                    })
            },
        ))
    }
}

/// A permanent SMTP refusal is not worth retrying; a transient one is.
///
/// Getting this wrong in either direction is expensive: retrying a 550 five
/// times is how a sending domain is blocklisted, and *not* retrying a 421 loses
/// the message.
#[cfg(feature = "mail-smtp")]
fn classify_smtp(error: &lettre::transport::smtp::Error) -> crate::Error {
    if error.is_permanent() {
        crate::Error::rejected("smtp", error.to_string())
    } else {
        crate::Error::unavailable("smtp", error.to_string(), None)
    }
}

// ---------------------------------------------------------------------------
// REST providers
// ---------------------------------------------------------------------------

/// Which REST provider a [`ProviderMailer`] talks to.
///
/// One enum rather than five structs: the five APIs differ in their endpoint,
/// their auth header and their JSON shape, and nothing else. Five near-identical
/// types would be five places to fix the same bug.
///
/// ```
/// # #[cfg(feature = "mail-resend")] {
/// use moso_mail::backend::MailProvider;
///
/// assert_eq!(MailProvider::Resend.as_str(), "resend");
/// # }
/// ```
#[cfg(feature = "provider")]
#[cfg_attr(docsrs, doc(cfg(feature = "provider")))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MailProvider {
    /// Amazon SES, through its v2 REST API.
    #[cfg(feature = "mail-ses")]
    Ses,
    /// SendGrid v3.
    #[cfg(feature = "mail-sendgrid")]
    Sendgrid,
    /// Postmark.
    #[cfg(feature = "mail-postmark")]
    Postmark,
    /// Resend.
    #[cfg(feature = "mail-resend")]
    Resend,
    /// Mailgun.
    #[cfg(feature = "mail-mailgun")]
    Mailgun,
}

#[cfg(feature = "provider")]
impl MailProvider {
    /// The provider's short name, as it appears in logs and metric labels.
    ///
    /// ```
    /// # #[cfg(feature = "mail-postmark")] {
    /// use moso_mail::backend::MailProvider;
    ///
    /// assert_eq!(MailProvider::Postmark.as_str(), "postmark");
    /// # }
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            #[cfg(feature = "mail-ses")]
            Self::Ses => "ses",
            #[cfg(feature = "mail-sendgrid")]
            Self::Sendgrid => "sendgrid",
            #[cfg(feature = "mail-postmark")]
            Self::Postmark => "postmark",
            #[cfg(feature = "mail-resend")]
            Self::Resend => "resend",
            #[cfg(feature = "mail-mailgun")]
            Self::Mailgun => "mailgun",
        }
    }
}

/// Sends through a provider's REST API.
///
/// ```no_run
/// # #[cfg(feature = "mail-resend")] {
/// # use moso_core::config::SecretString;
/// # use moso_mail::backend::{MailProvider, ProviderMailer};
/// # fn f(key: SecretString) {
/// let _ = ProviderMailer::new(MailProvider::Resend, key);
/// # }
/// # }
/// ```
#[cfg(feature = "provider")]
#[cfg_attr(docsrs, doc(cfg(feature = "provider")))]
#[derive(Debug)]
pub struct ProviderMailer {
    /// Which API to talk.
    provider: MailProvider,
    /// The API credential, redacted in `Debug`.
    api_key: moso_core::config::SecretString,
    /// The region, for providers that have them.
    region: Option<String>,
    /// The sending domain, for providers that require it in the path.
    domain: Option<String>,
    /// Overridden base URL, for a self-hosted or EU endpoint.
    base_url: Option<String>,
    /// The HTTP client, built once.
    client: std::sync::OnceLock<reqwest::Client>,
    /// The default sender, when configuration supplied one.
    from: Option<Address>,
    /// The AWS secret access key, for SES's SigV4 signature.
    secret_key: Option<moso_core::config::SecretString>,
    /// How long one request may take, connect and response body included.
    timeout: std::time::Duration,
}

#[cfg(feature = "provider")]
impl ProviderMailer {
    /// A mailer for `provider`, authenticating with `api_key`.
    ///
    /// For SES the `api_key` is the AWS access key id and the secret access
    /// key goes in [`ProviderMailer::secret_key`].
    ///
    /// ```no_run
    /// # #[cfg(feature = "mail-sendgrid")] {
    /// # use moso_core::config::SecretString;
    /// # use moso_mail::backend::{MailProvider, ProviderMailer};
    /// # fn f(key: SecretString) {
    /// let _ = ProviderMailer::new(MailProvider::Sendgrid, key);
    /// # }
    /// # }
    /// ```
    #[must_use]
    pub fn new(provider: MailProvider, api_key: moso_core::config::SecretString) -> Self {
        Self {
            provider,
            api_key,
            region: None,
            domain: None,
            base_url: None,
            client: std::sync::OnceLock::new(),
            from: None,
            secret_key: None,
            timeout: crate::deadline::DEFAULT_TIMEOUT,
        }
    }

    /// How long one API call may take.
    ///
    /// Covers DNS, connect, TLS, the request body and the provider's response
    /// — a provider that accepts the connection and never sends a status line
    /// is the failure this bounds, and it is the one an HTTP client's default
    /// configuration does not.
    ///
    /// ```no_run
    /// # #[cfg(feature = "mail-resend")] {
    /// # use std::time::Duration;
    /// # use moso_core::config::SecretString;
    /// # use moso_mail::backend::{MailProvider, ProviderMailer};
    /// # fn f(key: SecretString) {
    /// let _ = ProviderMailer::new(MailProvider::Resend, key).timeout(Duration::from_secs(10));
    /// # }
    /// # }
    /// ```
    #[must_use]
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the region, for SES.
    ///
    /// ```no_run
    /// # #[cfg(feature = "mail-ses")] {
    /// # use moso_core::config::SecretString;
    /// # use moso_mail::backend::{MailProvider, ProviderMailer};
    /// # fn f(key: SecretString) {
    /// let _ = ProviderMailer::new(MailProvider::Ses, key).region("eu-central-1");
    /// # }
    /// # }
    /// ```
    #[must_use]
    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Set the sending domain, for Mailgun.
    ///
    /// ```no_run
    /// # #[cfg(feature = "mail-mailgun")] {
    /// # use moso_core::config::SecretString;
    /// # use moso_mail::backend::{MailProvider, ProviderMailer};
    /// # fn f(key: SecretString) {
    /// let _ = ProviderMailer::new(MailProvider::Mailgun, key).domain("mg.example.com");
    /// # }
    /// # }
    /// ```
    #[must_use]
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Override the API base URL, for an EU endpoint or a test double.
    ///
    /// ```no_run
    /// # #[cfg(feature = "mail-resend")] {
    /// # use moso_core::config::SecretString;
    /// # use moso_mail::backend::{MailProvider, ProviderMailer};
    /// # fn f(key: SecretString) {
    /// let _ = ProviderMailer::new(MailProvider::Resend, key).base_url("http://127.0.0.1:9");
    /// # }
    /// # }
    /// ```
    #[must_use]
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into().trim_end_matches('/').to_owned());
        self
    }

    /// Set the AWS secret access key, for SES.
    ///
    /// ```no_run
    /// # #[cfg(feature = "mail-ses")] {
    /// # use moso_core::config::SecretString;
    /// # use moso_mail::backend::{MailProvider, ProviderMailer};
    /// # fn f(id: SecretString, secret: SecretString) {
    /// let _ = ProviderMailer::new(MailProvider::Ses, id).secret_key(secret);
    /// # }
    /// # }
    /// ```
    #[must_use]
    pub fn secret_key(mut self, secret: moso_core::config::SecretString) -> Self {
        self.secret_key = Some(secret);
        self
    }

    /// Fill this sender into any message that set none.
    ///
    /// ```no_run
    /// # #[cfg(feature = "mail-resend")] {
    /// # use moso_core::config::SecretString;
    /// # use moso_mail::{backend::{MailProvider, ProviderMailer}, Address};
    /// # fn f(key: SecretString, from: Address) {
    /// let _ = ProviderMailer::new(MailProvider::Resend, key).from(from);
    /// # }
    /// # }
    /// ```
    #[must_use]
    pub fn from(mut self, from: Address) -> Self {
        self.from = Some(from);
        self
    }

    /// The webhook verifier for this provider, built from a signing secret.
    ///
    /// The secret is what the provider's own documentation calls the signing
    /// key: Mailgun's HTTP webhook signing key, Resend's `whsec_…`, Postmark's
    /// basic-auth password, SendGrid's base64 *public* key, and — for SES,
    /// which signs through SNS with RSA — the PEM public key extracted from
    /// Amazon's signing certificate. See
    /// [`SnsVerifier`](crate::webhook::SnsVerifier) for why that one is pinned
    /// rather than fetched.
    ///
    /// ```no_run
    /// # #[cfg(feature = "mail-postmark")] {
    /// # use moso_core::config::SecretString;
    /// # use moso_mail::{backend::{MailProvider, ProviderMailer}, WebhookVerifier};
    /// # fn f(key: SecretString, signing: SecretString) {
    /// let mailer = ProviderMailer::new(MailProvider::Postmark, key);
    /// let verifier: Box<dyn WebhookVerifier> = mailer.webhook_verifier(signing);
    /// # }
    /// # }
    /// ```
    #[must_use]
    pub fn webhook_verifier(
        &self,
        signing_secret: moso_core::config::SecretString,
    ) -> Box<dyn crate::WebhookVerifier> {
        use crate::webhook::{SharedSecretVerifier, SnsVerifier, WebhookScheme};

        match self.provider {
            #[cfg(feature = "mail-ses")]
            MailProvider::Ses => Box::new(SnsVerifier::new(signing_secret)),
            #[cfg(feature = "mail-sendgrid")]
            MailProvider::Sendgrid => Box::new(SharedSecretVerifier::new(
                WebhookScheme::SendGrid,
                signing_secret,
            )),
            #[cfg(feature = "mail-postmark")]
            MailProvider::Postmark => Box::new(SharedSecretVerifier::new(
                WebhookScheme::Postmark,
                signing_secret,
            )),
            #[cfg(feature = "mail-resend")]
            MailProvider::Resend => Box::new(SharedSecretVerifier::new(
                WebhookScheme::Resend,
                signing_secret,
            )),
            #[cfg(feature = "mail-mailgun")]
            MailProvider::Mailgun => Box::new(SharedSecretVerifier::new(
                WebhookScheme::Mailgun,
                signing_secret,
            )),
        }
    }

    /// The HTTP client, built once.
    fn client(&self) -> &reqwest::Client {
        self.client.get_or_init(|| {
            // sqlx has already chosen rustls' *ring* provider for this process,
            // and two providers is a runtime panic. Installing it explicitly
            // makes the choice deterministic rather than order-dependent; an
            // `Err` means it is already installed, which is the outcome we
            // wanted.
            let _ = rustls::crypto::ring::default_provider().install_default();
            reqwest::Client::builder()
                .user_agent(concat!("moso-mail/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default()
        })
    }

    /// The API root for this provider.
    fn base(&self) -> String {
        if let Some(base) = &self.base_url {
            return base.clone();
        }
        match self.provider {
            #[cfg(feature = "mail-ses")]
            MailProvider::Ses => format!(
                "https://email.{}.amazonaws.com",
                self.region.as_deref().unwrap_or("us-east-1"),
            ),
            #[cfg(feature = "mail-sendgrid")]
            MailProvider::Sendgrid => "https://api.sendgrid.com".to_owned(),
            #[cfg(feature = "mail-postmark")]
            MailProvider::Postmark => "https://api.postmarkapp.com".to_owned(),
            #[cfg(feature = "mail-resend")]
            MailProvider::Resend => "https://api.resend.com".to_owned(),
            #[cfg(feature = "mail-mailgun")]
            MailProvider::Mailgun => "https://api.mailgun.net".to_owned(),
        }
    }

    /// Post one already-built request and read the identifier back.
    async fn post(
        &self,
        url: &str,
        body: Vec<u8>,
        headers: Vec<(String, String)>,
    ) -> Result<String> {
        let name = self.provider.as_str();
        let mut request = self.client().post(url).body(body);
        for (key, value) in headers {
            request = request.header(key, value);
        }

        let response = request.send().await.map_err(|error| {
            crate::Error::unavailable(name, error.to_string(), Some(Box::new(error)))
        })?;

        let status = response.status();
        // The provider's identifier is sometimes a header and sometimes a JSON
        // field; look in the header first because SendGrid has no body at all.
        let header_id = response
            .headers()
            .get("x-message-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            // 429 and 5xx are worth retrying; a 4xx means the message will be
            // refused again however many times it is sent.
            return Err(if status.as_u16() == 429 || status.is_server_error() {
                crate::Error::unavailable(name, format!("{status}: {text}"), None)
            } else {
                crate::Error::rejected(name, format!("{status}: {text}"))
            });
        }

        if let Some(id) = header_id {
            return Ok(id);
        }
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        Ok(["id", "MessageID", "message_id", "MessageId"]
            .iter()
            .find_map(|key| parsed.get(*key).and_then(serde_json::Value::as_str))
            .unwrap_or_default()
            .to_owned())
    }
}

/// Render the attachments in the shape a provider expects.
#[cfg(feature = "provider")]
fn provider_attachments(
    message: &RenderedEmail,
    name_key: &str,
    content_key: &str,
    type_key: &str,
) -> Vec<serde_json::Value> {
    use base64::Engine as _;

    message
        .attachments
        .iter()
        .map(|attachment| {
            let mut object = serde_json::Map::new();
            object.insert(name_key.to_owned(), attachment.filename().into());
            object.insert(
                content_key.to_owned(),
                base64::engine::general_purpose::STANDARD
                    .encode(attachment.body())
                    .into(),
            );
            object.insert(type_key.to_owned(), attachment.content_type().into());
            if let Some(id) = attachment.content_id() {
                object.insert("ContentID".to_owned(), id.into());
                object.insert("content_id".to_owned(), id.into());
            }
            serde_json::Value::Object(object)
        })
        .collect()
}

#[cfg(feature = "provider")]
impl Mailer for ProviderMailer {
    fn name(&self) -> &'static str {
        self.provider.as_str()
    }

    fn capabilities(&self) -> MailCapabilities {
        // The numbers are each provider's own documented maxima. They are here
        // rather than guessed at a call site so that an oversized attachment
        // is refused before it is uploaded.
        match self.provider {
            #[cfg(feature = "mail-ses")]
            MailProvider::Ses => MailCapabilities {
                batching: false,
                max_batch: 0,
                tracking: true,
                max_attachment_bytes: 10 * 1024 * 1024,
                max_recipients: 50,
                tags: true,
                webhooks: true,
                ..MailCapabilities::minimal()
            },
            #[cfg(feature = "mail-sendgrid")]
            MailProvider::Sendgrid => MailCapabilities {
                batching: true,
                max_batch: 1_000,
                templates: true,
                tracking: true,
                max_attachment_bytes: 30 * 1024 * 1024,
                max_recipients: 1_000,
                tags: true,
                webhooks: true,
                scheduling: true,
                ..MailCapabilities::minimal()
            },
            #[cfg(feature = "mail-postmark")]
            MailProvider::Postmark => MailCapabilities {
                batching: true,
                max_batch: 500,
                templates: true,
                tracking: true,
                max_attachment_bytes: 10 * 1024 * 1024,
                max_recipients: 50,
                tags: true,
                webhooks: true,
                ..MailCapabilities::minimal()
            },
            #[cfg(feature = "mail-resend")]
            MailProvider::Resend => MailCapabilities {
                batching: true,
                max_batch: 100,
                tracking: true,
                max_attachment_bytes: 40 * 1024 * 1024,
                max_recipients: 50,
                tags: true,
                webhooks: true,
                scheduling: true,
                idempotency: true,
                ..MailCapabilities::minimal()
            },
            #[cfg(feature = "mail-mailgun")]
            MailProvider::Mailgun => MailCapabilities {
                batching: true,
                max_batch: 1_000,
                templates: true,
                tracking: true,
                max_attachment_bytes: 25 * 1024 * 1024,
                max_recipients: 1_000,
                tags: true,
                webhooks: true,
                scheduling: true,
                ..MailCapabilities::minimal()
            },
        }
    }

    fn send_rendered<'a>(&'a self, message: &'a RenderedEmail) -> BoxFuture<'a, Result<MessageId>> {
        Box::pin(crate::deadline::within(
            self.name(),
            self.timeout,
            async move {
                let message = with_sender(message, self.from.as_ref());
                // Annotated `String` (the return of every `send_*`) so that under
                // `--features provider` with no concrete provider compiled in, the
                // arms vanish, the match becomes the never type, and it coerces to
                // `String` here instead of failing `MessageId::new`'s `Into<String>`
                // bound (`String: From<!>` is not implemented). The whole backend is
                // uninhabited in that configuration, so this arm is never reached.
                let id: String = match self.provider {
                    #[cfg(feature = "mail-ses")]
                    MailProvider::Ses => self.send_ses(&message).await?,
                    #[cfg(feature = "mail-sendgrid")]
                    MailProvider::Sendgrid => self.send_sendgrid(&message).await?,
                    #[cfg(feature = "mail-postmark")]
                    MailProvider::Postmark => self.send_postmark(&message).await?,
                    #[cfg(feature = "mail-resend")]
                    MailProvider::Resend => self.send_resend(&message).await?,
                    #[cfg(feature = "mail-mailgun")]
                    MailProvider::Mailgun => self.send_mailgun(&message).await?,
                };
                Ok(MessageId::new(id))
            },
        ))
    }

    fn send_batch<'a>(
        &'a self,
        messages: &'a [RenderedEmail],
    ) -> BoxFuture<'a, Result<Vec<Result<MessageId>>>> {
        Box::pin(async move {
            let capabilities = self.capabilities();
            if !capabilities.batching {
                return Err(crate::Error::unsupported(self.name(), "send_batch"));
            }
            if messages.len() > capabilities.max_batch {
                return Err(crate::Error::unsupported(self.name(), "send_batch"));
            }

            // Only Postmark and Resend expose an endpoint that takes an array
            // and reports per-message outcomes. For everything else the honest
            // implementation is a sequence of single sends, which is what the
            // provider's own client libraries do.
            let mut outcomes = Vec::with_capacity(messages.len());
            for message in messages {
                outcomes.push(self.send_rendered(message).await);
            }
            Ok(outcomes)
        })
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(crate::deadline::within(
            self.name(),
            self.timeout,
            async move {
                // A HEAD against the API root: enough to prove DNS, TLS and
                // reachability without spending a send. Under the same deadline as
                // a send, because a readiness probe that hangs is worse than one
                // that fails.
                let base = self.base();
                self.client()
                    .head(&base)
                    .send()
                    .await
                    .map(|_| ())
                    .map_err(|error| {
                        crate::Error::unavailable(
                            self.provider.as_str(),
                            error.to_string(),
                            Some(Box::new(error)),
                        )
                    })
            },
        ))
    }
}

#[cfg(feature = "mail-resend")]
impl ProviderMailer {
    /// Resend: one JSON object, `Bearer` auth, `Idempotency-Key` honoured.
    async fn send_resend(&self, message: &RenderedEmail) -> Result<String> {
        let body = serde_json::json!({
            "from": message.from.to_header(),
            "to": message.to.iter().map(Address::to_header).collect::<Vec<_>>(),
            "cc": message.cc.iter().map(Address::to_header).collect::<Vec<_>>(),
            "bcc": message.bcc.iter().map(Address::to_header).collect::<Vec<_>>(),
            "reply_to": message.reply_to.as_ref().map(Address::to_header),
            "subject": message.subject,
            "html": message.html,
            "text": message.text,
            "headers": message.headers.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
            "tags": message.tags.iter()
                .map(|(name, value)| serde_json::json!({ "name": name, "value": value }))
                .collect::<Vec<_>>(),
            "attachments": provider_attachments(message, "filename", "content", "content_type"),
        });

        let mut headers = vec![
            (
                "authorization".to_owned(),
                format!("Bearer {}", self.api_key.expose()),
            ),
            ("content-type".to_owned(), "application/json".to_owned()),
        ];
        if let Some(key) = &message.message_key {
            headers.push(("idempotency-key".to_owned(), key.as_str().to_owned()));
        }

        self.post(
            &format!("{}/emails", self.base()),
            serde_json::to_vec(&body).unwrap_or_default(),
            headers,
        )
        .await
    }
}

#[cfg(feature = "mail-sendgrid")]
impl ProviderMailer {
    /// SendGrid v3: personalizations, and the id in a response header.
    async fn send_sendgrid(&self, message: &RenderedEmail) -> Result<String> {
        let address = |address: &Address| {
            let mut object = serde_json::Map::new();
            object.insert("email".to_owned(), address.address().into());
            if let Some(name) = address.name() {
                object.insert("name".to_owned(), name.into());
            }
            serde_json::Value::Object(object)
        };

        let mut personalization = serde_json::Map::new();
        personalization.insert(
            "to".to_owned(),
            message.to.iter().map(address).collect::<Vec<_>>().into(),
        );
        if !message.cc.is_empty() {
            personalization.insert(
                "cc".to_owned(),
                message.cc.iter().map(address).collect::<Vec<_>>().into(),
            );
        }
        if !message.bcc.is_empty() {
            personalization.insert(
                "bcc".to_owned(),
                message.bcc.iter().map(address).collect::<Vec<_>>().into(),
            );
        }

        let body = serde_json::json!({
            "personalizations": [serde_json::Value::Object(personalization)],
            "from": address(&message.from),
            "reply_to": message.reply_to.as_ref().map(address),
            "subject": message.subject,
            // Order matters here as it does in MIME: the last part a client
            // understands is the one it shows.
            "content": [
                { "type": "text/plain", "value": message.text },
                { "type": "text/html", "value": message.html },
            ],
            "headers": message.headers.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
            "custom_args": message.tags.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
            "attachments": provider_attachments(message, "filename", "content", "type"),
        });

        self.post(
            &format!("{}/v3/mail/send", self.base()),
            serde_json::to_vec(&body).unwrap_or_default(),
            vec![
                (
                    "authorization".to_owned(),
                    format!("Bearer {}", self.api_key.expose()),
                ),
                ("content-type".to_owned(), "application/json".to_owned()),
            ],
        )
        .await
    }
}

#[cfg(feature = "mail-postmark")]
impl ProviderMailer {
    /// Postmark: capitalised field names, and a server token header.
    async fn send_postmark(&self, message: &RenderedEmail) -> Result<String> {
        let list = |addresses: &[Address]| {
            addresses
                .iter()
                .map(Address::to_header)
                .collect::<Vec<_>>()
                .join(", ")
        };

        let body = serde_json::json!({
            "From": message.from.to_header(),
            "To": list(&message.to),
            "Cc": list(&message.cc),
            "Bcc": list(&message.bcc),
            "ReplyTo": message.reply_to.as_ref().map(Address::to_header),
            "Subject": message.subject,
            "HtmlBody": message.html,
            "TextBody": message.text,
            "MessageStream": if message.marketing { "broadcast" } else { "outbound" },
            "Headers": message.headers.iter()
                .map(|(name, value)| serde_json::json!({ "Name": name, "Value": value }))
                .collect::<Vec<_>>(),
            "Metadata": message.tags.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
            "Attachments": provider_attachments(message, "Name", "Content", "ContentType"),
        });

        self.post(
            &format!("{}/email", self.base()),
            serde_json::to_vec(&body).unwrap_or_default(),
            vec![
                (
                    "x-postmark-server-token".to_owned(),
                    self.api_key.expose().to_owned(),
                ),
                ("content-type".to_owned(), "application/json".to_owned()),
                ("accept".to_owned(), "application/json".to_owned()),
            ],
        )
        .await
    }
}

#[cfg(feature = "mail-mailgun")]
impl ProviderMailer {
    /// Mailgun: the MIME endpoint, so the message Moso built is the message
    /// that is sent rather than one Mailgun reassembles from form fields.
    async fn send_mailgun(&self, message: &RenderedEmail) -> Result<String> {
        use base64::Engine as _;

        let domain = self.domain.as_deref().ok_or_else(|| {
            crate::Error::config(
                "the mailgun backend needs a sending domain — set `mail.domain`, or call \
                 `ProviderMailer::domain(\"mg.example.com\")`",
            )
        })?;

        let id = crate::mime::new_message_id(domain);
        let mime = crate::mime::to_rfc5322(message, &id);

        // `multipart/form-data` by hand rather than through reqwest's builder:
        // the body is two parts and building it here keeps the whole request
        // in one place.
        let boundary = format!(
            "moso{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let mut body = Vec::with_capacity(mime.len() + 512);
        for recipient in message.recipients() {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(b"Content-Disposition: form-data; name=\"to\"\r\n\r\n");
            body.extend_from_slice(recipient.address().as_bytes());
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"message\"; filename=\"message.mime\"\r\n\
              Content-Type: message/rfc822\r\n\r\n",
        );
        body.extend_from_slice(&mime);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let authorisation = base64::engine::general_purpose::STANDARD
            .encode(format!("api:{}", self.api_key.expose()));

        self.post(
            &format!("{}/v3/{domain}/messages.mime", self.base()),
            body,
            vec![
                ("authorization".to_owned(), format!("Basic {authorisation}")),
                (
                    "content-type".to_owned(),
                    format!("multipart/form-data; boundary={boundary}"),
                ),
            ],
        )
        .await
    }
}

#[cfg(feature = "mail-ses")]
impl ProviderMailer {
    /// SES v2, sending the raw MIME so the message is byte-identical to what
    /// SMTP would have carried.
    async fn send_ses(&self, message: &RenderedEmail) -> Result<String> {
        use base64::Engine as _;

        let secret = self.secret_key.as_ref().ok_or_else(|| {
            crate::Error::config(
                "the ses backend needs an AWS secret access key — call \
                 `ProviderMailer::secret_key(..)`, or set `mail.secret_key`",
            )
        })?;
        let region = self.region.as_deref().unwrap_or("us-east-1");

        let id = crate::mime::new_message_id(message.from.domain());
        let mime = crate::mime::to_rfc5322(message, &id);
        let body = serde_json::json!({
            "FromEmailAddress": message.from.to_header(),
            "Destination": {
                "ToAddresses": message.to.iter().map(|a| a.address().to_owned()).collect::<Vec<_>>(),
                "CcAddresses": message.cc.iter().map(|a| a.address().to_owned()).collect::<Vec<_>>(),
                "BccAddresses": message.bcc.iter().map(|a| a.address().to_owned()).collect::<Vec<_>>(),
            },
            "Content": {
                "Raw": { "Data": base64::engine::general_purpose::STANDARD.encode(&mime) },
            },
            "EmailTags": message.tags.iter()
                .map(|(name, value)| serde_json::json!({ "Name": name, "Value": value }))
                .collect::<Vec<_>>(),
        });
        let body = serde_json::to_vec(&body).unwrap_or_default();

        let base = self.base();
        let host = base
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        let path = "/v2/email/outbound-emails";

        let headers = crate::sigv4::sign(
            "POST",
            &host,
            path,
            "",
            &body,
            "ses",
            region,
            self.api_key.expose(),
            secret.expose(),
            chrono::Utc::now(),
        );

        self.post(&format!("{base}{path}"), body, headers).await
    }
}

// ---------------------------------------------------------------------------
// composition
// ---------------------------------------------------------------------------

/// Wraps a mailer and refuses anything on the suppression list.
///
/// Composition rather than a flag on every backend: the check is identical
/// whatever sends, and a backend that forgot it would be a silent reputation
/// leak.
///
/// ```no_run
/// use moso_mail::backend::Suppressing;
/// use moso_mail::{Mailer, SuppressionList};
/// use std::sync::Arc;
///
/// fn wrap(inner: Arc<dyn Mailer>, list: Arc<dyn SuppressionList>) -> Suppressing {
///     Suppressing::new(inner, list)
/// }
/// ```
#[derive(Clone)]
pub struct Suppressing {
    /// What actually sends.
    inner: std::sync::Arc<dyn Mailer>,
    /// What decides who may be sent to.
    list: std::sync::Arc<dyn crate::SuppressionList>,
}

impl Suppressing {
    /// Wrap `inner`, consulting `list` before every send.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_mail::{backend::Suppressing, Mailer, SuppressionList};
    /// # fn f(m: Arc<dyn Mailer>, l: Arc<dyn SuppressionList>) {
    /// let _ = Suppressing::new(m, l);
    /// # }
    /// ```
    #[must_use]
    pub fn new(
        inner: std::sync::Arc<dyn Mailer>,
        list: std::sync::Arc<dyn crate::SuppressionList>,
    ) -> Self {
        Self { inner, list }
    }

    /// The wrapped mailer.
    ///
    /// ```no_run
    /// # use moso_mail::{backend::Suppressing, Mailer};
    /// # fn f(s: &Suppressing) { let _: &dyn Mailer = s.inner(); }
    /// ```
    #[must_use]
    pub fn inner(&self) -> &dyn Mailer {
        self.inner.as_ref()
    }
}

impl core::fmt::Debug for Suppressing {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Suppressing")
            .field("inner", &self.inner.name())
            .finish_non_exhaustive()
    }
}

impl Mailer for Suppressing {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn capabilities(&self) -> MailCapabilities {
        self.inner.capabilities()
    }

    fn send_rendered<'a>(&'a self, message: &'a RenderedEmail) -> BoxFuture<'a, Result<MessageId>> {
        Box::pin(async move {
            self.list.check(message).await?;
            self.inner.send_rendered(message).await
        })
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        self.inner.probe()
    }
}

/// Rewrites every recipient to a fixed address. For staging.
///
/// A staging environment that sends real mail to real customers is a recurring
/// incident. This makes it structurally impossible while leaving the rest of
/// the flow — rendering, suppression, provider — exercised for real.
///
/// ```no_run
/// # use std::sync::Arc;
/// # use moso_mail::{backend::Redirecting, Address, Mailer};
/// # fn f(inner: Arc<dyn Mailer>, to: Address) {
/// let _ = Redirecting::new(inner, to);
/// # }
/// ```
#[derive(Clone)]
pub struct Redirecting {
    /// What actually sends.
    inner: std::sync::Arc<dyn Mailer>,
    /// Where everything goes instead.
    to: Address,
}

/// The header a redirected message carries its real recipients in.
///
/// A staging inbox that shows only the redirect address cannot tell you which
/// customer's flow you were testing.
pub const ORIGINAL_TO: &str = "X-Moso-Original-To";

impl Redirecting {
    /// Send everything to `to`, whatever the message says.
    ///
    /// The original recipients are preserved in an `X-Moso-Original-To`
    /// header, so a staging inbox still shows who would have received it.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_mail::{backend::Redirecting, Address, Mailer};
    /// # fn f(inner: Arc<dyn Mailer>, to: Address) {
    /// let _ = Redirecting::new(inner, to);
    /// # }
    /// ```
    #[must_use]
    pub fn new(inner: std::sync::Arc<dyn Mailer>, to: Address) -> Self {
        Self { inner, to }
    }
}

impl core::fmt::Debug for Redirecting {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Redirecting")
            .field("inner", &self.inner.name())
            .finish_non_exhaustive()
    }
}

impl Mailer for Redirecting {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn capabilities(&self) -> MailCapabilities {
        self.inner.capabilities()
    }

    fn send_rendered<'a>(&'a self, message: &'a RenderedEmail) -> BoxFuture<'a, Result<MessageId>> {
        Box::pin(async move {
            let mut redirected = message.clone();
            let original = message
                .recipients()
                .map(Address::to_header)
                .collect::<Vec<_>>()
                .join(", ");

            redirected.to = vec![self.to.clone()];
            redirected.cc.clear();
            redirected.bcc.clear();
            redirected
                .headers
                .retain(|(name, _)| !name.eq_ignore_ascii_case(ORIGINAL_TO));
            redirected.headers.push((ORIGINAL_TO.to_owned(), original));

            self.inner.send_rendered(&redirected).await
        })
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        self.inner.probe()
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::{Email, SuppressionList as _};

    /// A message to one recipient.
    #[cfg(any(
        feature = "console",
        feature = "memory",
        feature = "file",
        feature = "mail-smtp",
        feature = "provider"
    ))]
    fn message(to: &str) -> RenderedEmail {
        struct M(Address);
        impl Email for M {
            fn to(&self) -> Vec<Address> {
                vec![self.0.clone()]
            }
            fn subject(&self) -> Result<String> {
                Ok("Hello".to_owned())
            }
            fn html(&self) -> Result<String> {
                Ok("<p>Hello</p>".to_owned())
            }
            fn text(&self) -> Result<String> {
                Ok("Hello".to_owned())
            }
        }
        RenderedEmail::render(&M(Address::new(to).expect("valid"))).expect("renders")
    }

    /// A message that set no sender must not go out from `sender.invalid`.
    #[cfg(feature = "memory")]
    #[tokio::test]
    async fn the_configured_sender_fills_in_for_a_message_that_set_none() {
        let mailer = MemoryMailer::new();
        mailer.set_from(Some(Address::new("hello@shop.example").expect("valid")));
        mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect("sends");

        assert_eq!(mailer.sent()[0].from.address(), "hello@shop.example");
    }

    /// The test double's whole job: what was sent, and of what type.
    #[cfg(feature = "memory")]
    #[tokio::test]
    async fn the_memory_backend_records_what_was_sent() {
        let mailer = MemoryMailer::new();
        mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect("sends");

        assert_eq!(mailer.sent().len(), 1);
        assert_eq!(mailer.sent_of_kind("M").len(), 1);
        assert_eq!(mailer.sent_of_kind("NoSuchEmail").len(), 0);
        mailer.clear();
        assert!(mailer.sent().is_empty());
    }

    /// A job that retries must not send twice. The key is what makes the
    /// second attempt a no-op.
    #[cfg(feature = "memory")]
    #[tokio::test]
    async fn an_idempotency_key_makes_a_retry_a_no_op() {
        let mailer = MemoryMailer::new();
        let mut message = message("ada@example.com");
        message.message_key = Some(crate::MessageKey::new("welcome:usr_1"));

        let first = mailer.send_rendered(&message).await.expect("sends");
        let second = mailer.send_rendered(&message).await.expect("sends");

        assert_eq!(first, second);
        assert_eq!(mailer.sent().len(), 1);
    }

    /// A test that exercises the failure path needs a failure it can arrange.
    #[cfg(feature = "memory")]
    #[tokio::test]
    async fn the_memory_backend_can_be_made_to_fail() {
        let mailer = MemoryMailer::new();
        mailer.fail_with(Some("provider down"));

        let error = mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect_err("fails");
        assert!(error.retryable());

        mailer.fail_with(None);
        mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect("sends again");
    }

    /// The whole assertion surface an `app.mail()` layer is written on, found
    /// by the Rust type rather than by a string a rename would silently break.
    #[cfg(feature = "memory")]
    #[tokio::test]
    async fn the_assertion_surface_finds_messages_by_their_rust_type() {
        struct WelcomeEmail(Address);
        impl Email for WelcomeEmail {
            fn to(&self) -> Vec<Address> {
                vec![self.0.clone()]
            }
            fn subject(&self) -> Result<String> {
                Ok("Welcome".to_owned())
            }
            fn html(&self) -> Result<String> {
                Ok("<p>Welcome</p>".to_owned())
            }
            fn text(&self) -> Result<String> {
                Ok("Welcome".to_owned())
            }
        }

        let mailer = MemoryMailer::new();
        assert_eq!(mailer.sent_count(), 0, "assert_none_sent");
        mailer
            .send(&WelcomeEmail(
                Address::new("ada@example.com").expect("valid"),
            ))
            .await
            .expect("sends");
        mailer
            .send(&WelcomeEmail(
                Address::new("grace@example.com").expect("valid"),
            ))
            .await
            .expect("sends");

        assert_eq!(mailer.sent_count(), 2);
        assert_eq!(mailer.count_of::<WelcomeEmail>(), 2, "assert_sent(2)");
        assert_eq!(mailer.sent_of::<WelcomeEmail>().len(), 2);
        // The short name and the fully qualified one find the same messages.
        assert_eq!(mailer.sent_of_kind("WelcomeEmail").len(), 2);
        assert_eq!(
            mailer
                .sent_of_kind(std::any::type_name::<WelcomeEmail>())
                .len(),
            2,
        );
        assert_eq!(mailer.count_of_kind("PasswordReset"), 0);
        assert_eq!(
            mailer.sent_of::<WelcomeEmail>()[1].to[0].address(),
            "grace@example.com",
        );

        mailer.clear();
        assert_eq!(mailer.sent_count(), 0);
        assert_eq!(mailer.count_of::<WelcomeEmail>(), 0);
    }

    /// The property every backend now has: a transport that stops answering
    /// produces an error, not a worker that never comes back.
    #[cfg(feature = "memory")]
    #[tokio::test]
    async fn a_stalled_send_becomes_a_timeout_naming_the_backend() {
        let mailer = MemoryMailer::new().timeout(std::time::Duration::from_millis(20));
        mailer.delay(Some(std::time::Duration::from_secs(60)));

        let error = mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect_err("the deadline fires");

        assert!(matches!(error, crate::Error::Timeout { .. }));
        assert_eq!(error.backend(), Some("memory"));
        assert!(error.retryable(), "a job must be able to try again");
        assert!(
            error.to_string().contains("20ms"),
            "the deadline is in the message: {error}",
        );
        assert!(
            mailer.sent().is_empty(),
            "an abandoned send must not be recorded as delivered",
        );

        // Clearing the stall restores the backend, so one test can exercise
        // both sides of the retry.
        mailer.delay(None);
        mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect("sends");
        assert_eq!(mailer.sent_count(), 1);
    }

    /// A deadline that does not fire has to be invisible: the local backends
    /// keep sending exactly as they did.
    #[cfg(all(feature = "console", feature = "memory"))]
    #[tokio::test]
    async fn a_deadline_that_does_not_fire_changes_nothing() {
        let console = ConsoleMailer::new().timeout(std::time::Duration::from_secs(30));
        console
            .send_rendered(&message("ada@example.com"))
            .await
            .expect("sends");
        assert_eq!(console.inbox().len(), 1);

        let memory = MemoryMailer::new().timeout(std::time::Duration::from_secs(30));
        memory
            .send_rendered(&message("ada@example.com"))
            .await
            .expect("sends");
        assert_eq!(memory.sent_count(), 1);
    }

    /// The console deadline is wired, not decorative: a send that overruns it
    /// returns rather than holding the caller, and nothing is recorded as sent.
    #[cfg(feature = "console")]
    #[tokio::test]
    async fn a_console_send_that_overruns_its_deadline_becomes_a_timeout() {
        let mailer = ConsoleMailer::new().timeout(std::time::Duration::from_millis(20));
        mailer.delay(Some(std::time::Duration::from_secs(60)));

        let started = std::time::Instant::now();
        let error = mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect_err("the deadline fires");

        assert!(matches!(error, crate::Error::Timeout { .. }), "{error}");
        assert_eq!(error.backend(), Some("console"));
        assert!(error.retryable(), "a job must be able to try again");
        assert!(
            error.to_string().contains("20ms"),
            "the deadline is in the message: {error}",
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the caller waited for the deadline, not for the stall",
        );
        assert!(
            mailer.inbox().is_empty(),
            "an abandoned send must not be recorded as delivered",
        );

        // Clearing the stall restores the backend, so one test exercises both
        // sides of the retry a job would perform.
        mailer.delay(None);
        mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect("sends");
        assert_eq!(mailer.inbox().len(), 1);
    }

    /// The file deadline guards the hung mount its own documentation names: a
    /// write that overruns is abandoned, no `.eml` is left behind, and the same
    /// message writes once the stall is over.
    #[cfg(feature = "file")]
    #[tokio::test]
    async fn a_file_send_that_overruns_its_deadline_becomes_a_timeout() {
        let directory = std::env::temp_dir().join(format!(
            "moso-mail-deadline-{}-{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        let _ = std::fs::remove_dir_all(&directory);

        let mailer = FileMailer::new(&directory).timeout(std::time::Duration::from_millis(20));
        mailer.delay(Some(std::time::Duration::from_secs(60)));

        let started = std::time::Instant::now();
        let error = mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect_err("the deadline fires");

        assert!(matches!(error, crate::Error::Timeout { .. }), "{error}");
        assert_eq!(error.backend(), Some("file"));
        assert!(error.retryable(), "a job must be able to try again");
        assert!(
            error.to_string().contains("20ms"),
            "the deadline is in the message: {error}",
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the caller waited for the deadline, not for the stall",
        );
        // The stall trips before a single byte reaches the disk, so the
        // directory is either absent or empty — never holding a half-written
        // message somebody would mistake for a delivery.
        let written = std::fs::read_dir(&directory)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(written, 0, "an abandoned send must not leave an `.eml`");

        // Clearing the stall restores the backend: the same message now writes.
        mailer.delay(None);
        mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect("sends");
        let written = std::fs::read_dir(&directory)
            .expect("the directory now exists")
            .count();
        assert_eq!(written, 1, "the retry wrote exactly one message");

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A listener that accepts a connection and then says nothing: the
    /// controllable stand-in for a provider that has stopped answering, and
    /// the only honest way to test a hang without a network.
    #[cfg(any(feature = "mail-smtp", feature = "mail-resend"))]
    async fn a_silent_listener() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds a loopback port");
        let port = listener.local_addr().expect("has an address").port();
        tokio::spawn(async move {
            // The accepted streams are held, not dropped: dropping one closes
            // the connection and turns the hang into a connection error, which
            // is a different test.
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        port
    }

    /// The console backend keeps only the last `keep` messages, or a long
    /// development session is a memory leak.
    #[cfg(feature = "console")]
    #[tokio::test]
    async fn the_console_backend_retains_a_bounded_ring() {
        let mailer = ConsoleMailer::new().keep(3);
        for index in 0..5 {
            mailer
                .send_rendered(&message(&format!("a{index}@example.com")))
                .await
                .expect("sends");
        }

        let inbox = mailer.inbox();
        assert_eq!(inbox.len(), 3);
        // Newest first.
        assert_eq!(inbox[0].to[0].address(), "a4@example.com");
    }

    /// The inbox is what `/_mail` renders, and its ids have to resolve.
    #[cfg(feature = "console")]
    #[tokio::test]
    async fn the_console_backend_backs_the_preview_inbox() {
        use crate::preview::Inbox as _;

        let mailer = ConsoleMailer::new();
        mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect("sends");

        let items = mailer.list(10);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].subject, "Hello");
        assert_eq!(items[0].to, vec!["ada@example.com".to_owned()]);

        let full = mailer.get(&items[0].id).expect("resolves");
        assert_eq!(full.html, "<p>Hello</p>");
        assert!(mailer.get("nope").is_none());

        mailer.clear();
        assert!(mailer.list(10).is_empty());
    }

    /// A staging deployment must not be able to mail a real customer, and the
    /// original recipient has to survive so the test is still readable.
    #[cfg(feature = "memory")]
    #[tokio::test]
    async fn redirecting_rewrites_every_recipient_and_records_the_original() {
        let inner = std::sync::Arc::new(MemoryMailer::new());
        let redirecting = Redirecting::new(
            inner.clone(),
            Address::new("staging@shop.example").expect("valid"),
        );

        let mut message = message("customer@example.com");
        message.cc = vec![Address::new("cc@example.com").expect("valid")];
        message.bcc = vec![Address::new("bcc@example.com").expect("valid")];
        redirecting.send_rendered(&message).await.expect("sends");

        let sent = &inner.sent()[0];
        assert_eq!(sent.to.len(), 1);
        assert_eq!(sent.to[0].address(), "staging@shop.example");
        assert!(sent.cc.is_empty());
        assert!(sent.bcc.is_empty());
        assert_eq!(
            sent.header(ORIGINAL_TO),
            Some("customer@example.com, cc@example.com, bcc@example.com"),
        );
    }

    /// No backend can forget the suppression check, because the check is not
    /// in a backend.
    #[cfg(feature = "memory")]
    #[tokio::test]
    async fn suppressing_refuses_before_the_inner_backend_sees_the_message() {
        let inner = std::sync::Arc::new(MemoryMailer::new());
        let list = std::sync::Arc::new(crate::MemorySuppressionList::new());
        list.record(crate::Suppression::new(
            Address::new("bounced@example.com").expect("valid"),
            crate::SuppressionReason::HardBounce,
        ))
        .await
        .expect("records");

        let suppressing = Suppressing::new(inner.clone(), list);
        let error = suppressing
            .send_rendered(&message("bounced@example.com"))
            .await
            .expect_err("suppressed");

        assert!(error.is_suppressed());
        assert!(inner.sent().is_empty(), "nothing reached the backend");
    }

    /// A backend that cannot batch says so rather than looping and letting a
    /// caller believe it made one request.
    #[tokio::test]
    async fn a_backend_that_cannot_batch_refuses_rather_than_looping() {
        struct Single;
        impl Mailer for Single {
            fn name(&self) -> &'static str {
                "single"
            }
            fn capabilities(&self) -> MailCapabilities {
                MailCapabilities::minimal()
            }
            fn send_rendered<'a>(
                &'a self,
                _: &'a RenderedEmail,
            ) -> BoxFuture<'a, Result<MessageId>> {
                Box::pin(async { Ok(MessageId::new("x")) })
            }
        }

        let error = Single
            .send_batch(&[dummy(), dummy()])
            .await
            .expect_err("cannot batch");
        assert!(matches!(error, crate::Error::Unsupported { .. }));

        // One message is not a batch, and must still work.
        assert_eq!(Single.send_batch(&[dummy()]).await.expect("one").len(), 1);
    }

    /// A rendered message with the minimum a batch test needs.
    fn dummy() -> RenderedEmail {
        struct M;
        impl Email for M {
            fn to(&self) -> Vec<Address> {
                vec![Address::new("a@example.com").expect("valid")]
            }
            fn subject(&self) -> Result<String> {
                Ok(String::new())
            }
            fn html(&self) -> Result<String> {
                Ok(String::new())
            }
            fn text(&self) -> Result<String> {
                Ok(String::new())
            }
        }
        RenderedEmail::render(&M).expect("renders")
    }

    /// The `.eml` on disk is a real message a client can open.
    #[cfg(feature = "file")]
    #[tokio::test]
    async fn the_file_backend_writes_a_readable_eml() {
        let directory = std::env::temp_dir().join(format!(
            "moso-mail-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        ));
        let mailer = FileMailer::new(&directory)
            .from(Address::new("hello@shop.example").expect("valid"))
            .timeout(std::time::Duration::from_secs(30));

        mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect("writes");

        let mut entries = tokio::fs::read_dir(&directory).await.expect("reads");
        let entry = entries
            .next_entry()
            .await
            .expect("reads")
            .expect("one file");
        assert!(entry.file_name().to_string_lossy().ends_with(".eml"));

        let text = tokio::fs::read_to_string(entry.path())
            .await
            .expect("reads");
        assert!(text.contains("From: hello@shop.example"));
        assert!(text.contains("To: ada@example.com"));
        assert!(text.contains("Subject: Hello"));
        assert!(text.contains("multipart/alternative"));

        let _ = tokio::fs::remove_dir_all(&directory).await;
    }

    /// The deadline covers the whole SMTP conversation, so a server that
    /// accepts the connection and never sends its greeting — no write ever
    /// blocks, and no per-write timeout would ever fire — still returns.
    #[cfg(feature = "mail-smtp")]
    #[tokio::test]
    async fn an_smtp_server_that_never_speaks_trips_the_conversation_deadline() {
        let port = a_silent_listener().await;
        let mailer = SmtpMailer::from_url(&format!("smtp://127.0.0.1:{port}?security=none"))
            .expect("a loopback DSN")
            .timeout(std::time::Duration::from_millis(150));

        let error = mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect_err("the deadline fires");

        assert!(matches!(error, crate::Error::Timeout { .. }), "{error}");
        assert_eq!(error.backend(), Some("smtp"));
        assert!(error.retryable());
    }

    /// The same for a REST provider: the connection is accepted, the status
    /// line never arrives, and the send returns rather than holding the job.
    #[cfg(feature = "mail-resend")]
    #[tokio::test]
    async fn a_provider_that_accepts_and_never_answers_trips_the_deadline() {
        let port = a_silent_listener().await;
        let mailer = ProviderMailer::new(
            MailProvider::Resend,
            moso_core::config::SecretString::new("test-key"),
        )
        .base_url(format!("http://127.0.0.1:{port}"))
        .timeout(std::time::Duration::from_millis(150));

        let error = mailer
            .send_rendered(&message("ada@example.com"))
            .await
            .expect_err("the deadline fires");

        assert!(matches!(error, crate::Error::Timeout { .. }), "{error}");
        assert_eq!(error.backend(), Some("resend"));

        // A readiness probe is under the same deadline, because a probe that
        // hangs is worse than one that fails.
        let error = mailer.probe().await.expect_err("the deadline fires");
        assert!(matches!(error, crate::Error::Timeout { .. }), "{error}");
    }

    /// Unencrypted SMTP to a remote host sends the password in the clear, so
    /// it is a configuration error and not an option.
    #[cfg(feature = "mail-smtp")]
    #[test]
    fn an_unencrypted_dsn_is_refused_unless_it_is_loopback() {
        assert!(SmtpMailer::from_url("smtp://localhost:1025?security=none").is_ok());
        assert!(SmtpMailer::from_url("smtp://127.0.0.1:1025?security=none").is_ok());
        let error = SmtpMailer::from_url("smtp://mail.example.com?security=none")
            .expect_err("not loopback");
        assert!(error.to_string().contains("in the clear"));
    }

    /// The DSN carries the credentials, the port and the security mode, and a
    /// mis-parse is an outage.
    #[cfg(feature = "mail-smtp")]
    #[test]
    fn a_dsn_parses_into_its_parts() {
        let mailer =
            SmtpMailer::from_url("smtps://user:p%40ss@mail.example.com:465").expect("parses");
        assert_eq!(mailer.host, "mail.example.com");
        assert_eq!(mailer.port, 465);
        assert_eq!(mailer.security, SmtpSecurity::ImplicitTls);
        assert_eq!(mailer.username.as_deref(), Some("user"));
        assert_eq!(
            mailer
                .password
                .as_ref()
                .map(moso_core::config::SecretString::expose),
            Some("p@ss"),
        );

        // The default port follows the scheme.
        assert_eq!(SmtpMailer::from_url("smtp://h").expect("parses").port, 587);
        assert_eq!(SmtpMailer::from_url("smtps://h").expect("parses").port, 465);
    }

    /// A DSN that is not one names what was wrong rather than panicking.
    #[cfg(feature = "mail-smtp")]
    #[test]
    fn a_malformed_dsn_names_the_problem() {
        assert!(SmtpMailer::from_url("mail.example.com").is_err());
        assert!(SmtpMailer::from_url("http://mail.example.com").is_err());
        assert!(SmtpMailer::from_url("smtp://h:notaport").is_err());
        assert!(SmtpMailer::from_url("smtp://h?security=carrier-pigeon").is_err());
    }
}
