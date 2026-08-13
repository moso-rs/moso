//! The suppression list: who must not be mailed, and why.
//!
//! A hard bounce or a spam complaint is a permanent signal. Continuing to send
//! to that address is the single fastest way to lose a sending domain's
//! reputation, and every provider will eventually suspend an account that does
//! it. Moso records both automatically from verified provider webhooks and
//! refuses the send with [`Error::Suppressed`](crate::Error::Suppressed).

use std::borrow::Cow;

use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::{Address, Result};

/// Why an address is suppressed.
///
/// ```
/// use moso_mail::SuppressionReason;
///
/// // A hard bounce is permanent; a soft one never reaches this list.
/// assert!(SuppressionReason::HardBounce.is_permanent());
/// assert!(!SuppressionReason::Manual.is_permanent());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SuppressionReason {
    /// The address does not exist. Permanent.
    HardBounce,
    /// The recipient marked a message as spam. Permanent, and legally
    /// significant in several jurisdictions.
    Complaint,
    /// The recipient unsubscribed from marketing. Transactional mail is still
    /// allowed; [`SuppressionList::check`] takes the message's
    /// [`marketing`](crate::Email::marketing) flag into account.
    Unsubscribed,
    /// An operator added it. Reversible.
    Manual,
    /// The provider rejected the address as invalid before delivery.
    Invalid,
}

impl SuppressionReason {
    /// Whether the suppression should never be lifted automatically.
    ///
    /// ```
    /// use moso_mail::SuppressionReason;
    ///
    /// assert!(SuppressionReason::Complaint.is_permanent());
    /// ```
    #[must_use]
    pub const fn is_permanent(self) -> bool {
        matches!(self, Self::HardBounce | Self::Complaint | Self::Invalid)
    }

    /// Whether this suppression blocks transactional mail too.
    ///
    /// [`SuppressionReason::Unsubscribed`] blocks only marketing; everything
    /// else blocks everything.
    ///
    /// ```
    /// use moso_mail::SuppressionReason;
    ///
    /// assert!(!SuppressionReason::Unsubscribed.blocks_transactional());
    /// assert!(SuppressionReason::HardBounce.blocks_transactional());
    /// ```
    #[must_use]
    pub const fn blocks_transactional(self) -> bool {
        !matches!(self, Self::Unsubscribed)
    }
}

/// One entry on the list.
///
/// ```
/// use moso_mail::{Address, Suppression, SuppressionReason};
///
/// let address = Address::new("bounced@example.com")?;
/// let entry = Suppression::new(address, SuppressionReason::HardBounce);
/// assert!(entry.reason().is_permanent());
/// # Ok::<(), moso_mail::Error>(())
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Suppression {
    /// The suppressed address.
    address: Address,
    /// Why.
    reason: SuppressionReason,
    /// When it was recorded.
    recorded_at: DateTime<Utc>,
    /// Free-form detail from the provider, already redacted.
    detail: Option<String>,
    /// The provider that reported it, when a webhook did.
    source: Option<String>,
}

impl Suppression {
    /// Record a suppression as of now.
    ///
    /// ```
    /// # use moso_mail::{Address, Suppression, SuppressionReason};
    /// let address = Address::new("a@example.com")?;
    /// let entry = Suppression::new(address, SuppressionReason::Complaint);
    /// assert_eq!(entry.reason(), SuppressionReason::Complaint);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn new(address: Address, reason: SuppressionReason) -> Self {
        Self {
            address,
            reason,
            recorded_at: Utc::now(),
            detail: None,
            source: None,
        }
    }

    /// Record a suppression at a given instant.
    ///
    /// A provider webhook reports *when* the bounce happened, which is often
    /// minutes before the webhook arrives. Recording the provider's timestamp
    /// keeps the list's ordering honest and makes replaying a backlog of
    /// events idempotent in effect.
    ///
    /// ```
    /// # use chrono::Utc;
    /// # use moso_mail::{Address, Suppression, SuppressionReason};
    /// let address = Address::new("a@example.com")?;
    /// let _ = Suppression::at(address, SuppressionReason::HardBounce, Utc::now());
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn at(address: Address, reason: SuppressionReason, recorded_at: DateTime<Utc>) -> Self {
        Self {
            address,
            reason,
            recorded_at,
            detail: None,
            source: None,
        }
    }

    /// The provider that reported this, when a webhook did.
    ///
    /// ```
    /// # use moso_mail::{Address, Suppression, SuppressionReason};
    /// # let a = Address::new("a@example.com")?;
    /// let entry = Suppression::new(a, SuppressionReason::HardBounce).with_detail("ses", "550");
    /// assert_eq!(entry.source(), Some("ses"));
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// The suppressed address.
    ///
    /// ```
    /// # use moso_mail::{Address, Suppression, SuppressionReason};
    /// # let a = Address::new("a@example.com")?;
    /// let entry = Suppression::new(a, SuppressionReason::Manual);
    /// assert_eq!(entry.address().address(), "a@example.com");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Why it is suppressed.
    ///
    /// ```
    /// # use moso_mail::{Address, Suppression, SuppressionReason};
    /// # let a = Address::new("a@example.com")?;
    /// assert_eq!(Suppression::new(a, SuppressionReason::Manual).reason(), SuppressionReason::Manual);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn reason(&self) -> SuppressionReason {
        self.reason
    }

    /// When it was recorded.
    ///
    /// ```
    /// # use chrono::{DateTime, Utc};
    /// # use moso_mail::{Address, Suppression, SuppressionReason};
    /// # let a = Address::new("a@example.com")?;
    /// let _: DateTime<Utc> = Suppression::new(a, SuppressionReason::Manual).recorded_at();
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn recorded_at(&self) -> DateTime<Utc> {
        self.recorded_at
    }

    /// The provider's detail, when there was one.
    ///
    /// ```
    /// # use moso_mail::{Address, Suppression, SuppressionReason};
    /// # let a = Address::new("a@example.com")?;
    /// assert_eq!(Suppression::new(a, SuppressionReason::Manual).detail(), None);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// Attach provider detail.
    ///
    /// ```
    /// # use moso_mail::{Address, Suppression, SuppressionReason};
    /// # let a = Address::new("a@example.com")?;
    /// let entry = Suppression::new(a, SuppressionReason::HardBounce)
    ///     .with_detail("ses", "550 5.1.1 user unknown");
    /// assert_eq!(entry.detail(), Some("550 5.1.1 user unknown"));
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_detail(mut self, source: impl Into<String>, detail: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self.detail = Some(detail.into());
        self
    }
}

/// Where suppressions are kept.
///
/// Dyn-compatible: the shipped implementations are a table, an in-memory set
/// for tests, and a KV-backed one, and an application picks in configuration.
///
/// ```no_run
/// use moso_mail::{RenderedEmail, SuppressionList};
///
/// async fn guard(list: &dyn SuppressionList, m: &RenderedEmail) -> moso_mail::Result<()> {
///     list.check(m).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a suppression list",
    label = "not a suppression list",
    note = "a suppression list implements `record`, `lookup`, `release` and `list`",
    note = "help: use `MemorySuppressionList` in tests, or the table-backed one the migration \
            generator creates from `moso_mail_suppressions`"
)]
pub trait SuppressionList: Send + Sync + 'static {
    /// Record a suppression, replacing any earlier one for the same address.
    ///
    /// # Errors
    ///
    /// Whatever the storage reports, as
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    fn record<'a>(&'a self, entry: Suppression) -> BoxFuture<'a, Result<()>>;

    /// Look one address up.
    ///
    /// # Errors
    ///
    /// Whatever the storage reports.
    fn lookup<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Option<Suppression>>>;

    /// Remove a suppression. Refuses a
    /// [permanent](SuppressionReason::is_permanent) one unless `force`.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) when asked to release a
    /// permanent suppression without `force`.
    fn release<'a>(&'a self, address: &'a str, force: bool) -> BoxFuture<'a, Result<bool>>;

    /// Page through the list, newest first.
    ///
    /// # Errors
    ///
    /// Whatever the storage reports.
    fn list<'a>(
        &'a self,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, Result<(Vec<Suppression>, Option<String>)>>;

    /// Refuse the send when any recipient is suppressed.
    ///
    /// Honours [`RenderedEmail::marketing`](crate::RenderedEmail::marketing):
    /// an unsubscribed address still receives a password reset.
    ///
    /// # Errors
    ///
    /// [`Error::Suppressed`](crate::Error::Suppressed) naming the first
    /// blocked recipient.
    fn check<'a>(&'a self, message: &'a crate::RenderedEmail) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            for recipient in message.recipients() {
                let Some(entry) = self.lookup(&recipient.normalised()).await? else {
                    continue;
                };
                // An unsubscribed address still receives a password reset: the
                // unsubscribe was from marketing, and refusing a transactional
                // message would lock somebody out of their account.
                if message.marketing || entry.reason().blocks_transactional() {
                    return Err(crate::Error::suppressed(
                        recipient.address().to_owned(),
                        describe_reason(entry.reason()),
                    ));
                }
            }
            Ok(())
        })
    }
}

/// A suppression list in a `BTreeMap`. For tests and single-process dev.
///
/// ```
/// use moso_mail::MemorySuppressionList;
///
/// let list = MemorySuppressionList::new();
/// assert_eq!(list.len(), 0);
/// ```
#[derive(Debug, Default)]
pub struct MemorySuppressionList {
    /// Entries by lowercased address.
    entries: std::sync::RwLock<std::collections::BTreeMap<String, Suppression>>,
}

impl MemorySuppressionList {
    /// An empty list.
    ///
    /// ```
    /// use moso_mail::MemorySuppressionList;
    ///
    /// assert!(MemorySuppressionList::new().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many addresses are suppressed.
    ///
    /// ```
    /// # use moso_mail::MemorySuppressionList;
    /// assert_eq!(MemorySuppressionList::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether nothing is suppressed.
    ///
    /// ```
    /// # use moso_mail::MemorySuppressionList;
    /// assert!(MemorySuppressionList::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The map, recovering from a poisoned lock.
    ///
    /// A panic in a test that held the lock must not turn every later
    /// assertion into a second, unrelated panic.
    fn read(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, std::collections::BTreeMap<String, Suppression>> {
        self.entries
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The map, mutably, recovering from a poisoned lock.
    fn write(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, std::collections::BTreeMap<String, Suppression>> {
        self.entries
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SuppressionList for MemorySuppressionList {
    fn record<'a>(&'a self, entry: Suppression) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.write().insert(entry.address().normalised(), entry);
            Ok(())
        })
    }

    fn lookup<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Option<Suppression>>> {
        Box::pin(async move { Ok(self.read().get(&address.to_lowercase()).cloned()) })
    }

    fn release<'a>(&'a self, address: &'a str, force: bool) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let key = address.to_lowercase();
            let mut entries = self.write();
            let Some(entry) = entries.get(&key) else {
                return Ok(false);
            };
            if entry.reason().is_permanent() && !force {
                return Err(crate::Error::config(format!(
                    "`{address}` is suppressed for a permanent reason ({}); releasing it will \
                     send to an address the provider already rejected — pass `force` if that is \
                     what you mean",
                    describe_reason(entry.reason()),
                )));
            }
            entries.remove(&key);
            Ok(true)
        })
    }

    fn list<'a>(
        &'a self,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, Result<(Vec<Suppression>, Option<String>)>> {
        Box::pin(async move {
            // Newest first, so the operator sees what just happened. The
            // cursor is the address of the last entry on the previous page:
            // stable under insertion, unlike an offset.
            let entries = self.read();
            let mut all: Vec<Suppression> = entries.values().cloned().collect();
            all.sort_by(|a, b| {
                b.recorded_at()
                    .cmp(&a.recorded_at())
                    .then_with(|| a.address().normalised().cmp(&b.address().normalised()))
            });

            let start = match cursor {
                None => 0,
                Some(after) => all
                    .iter()
                    .position(|entry| entry.address().normalised() == after.to_lowercase())
                    .map_or(all.len(), |index| index + 1),
            };

            let limit = limit.max(1) as usize;
            let page: Vec<Suppression> = all.iter().skip(start).take(limit).cloned().collect();
            let next = (start + page.len() < all.len())
                .then(|| page.last().map(|entry| entry.address().normalised()))
                .flatten();
            Ok((page, next))
        })
    }
}

/// The reason string a [`Suppression`] renders into an error.
///
/// Kept short and free of the provider's raw text so it can be logged without
/// leaking a recipient's bounce message.
///
/// ```
/// use moso_mail::{describe_reason, SuppressionReason};
///
/// assert_eq!(describe_reason(SuppressionReason::Complaint), "spam complaint");
/// ```
#[must_use]
pub fn describe_reason(reason: SuppressionReason) -> Cow<'static, str> {
    match reason {
        SuppressionReason::HardBounce => Cow::Borrowed("hard bounce"),
        SuppressionReason::Complaint => Cow::Borrowed("spam complaint"),
        SuppressionReason::Unsubscribed => Cow::Borrowed("unsubscribed"),
        SuppressionReason::Manual => Cow::Borrowed("suppressed by an operator"),
        SuppressionReason::Invalid => Cow::Borrowed("invalid address"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rendered message to `to`, marked marketing or not.
    fn message(to: &str, marketing: bool) -> crate::RenderedEmail {
        struct M {
            to: Address,
            marketing: bool,
        }
        impl crate::Email for M {
            fn to(&self) -> Vec<Address> {
                vec![self.to.clone()]
            }
            fn subject(&self) -> Result<String> {
                Ok("s".to_owned())
            }
            fn html(&self) -> Result<String> {
                Ok("<p>h</p>".to_owned())
            }
            fn text(&self) -> Result<String> {
                Ok("h".to_owned())
            }
            fn headers(&self) -> http::HeaderMap {
                let mut headers = http::HeaderMap::new();
                if self.marketing {
                    headers.insert(
                        "list-unsubscribe",
                        http::HeaderValue::from_static("<https://x.example/u>"),
                    );
                }
                headers
            }
            fn marketing(&self) -> bool {
                self.marketing
            }
        }

        crate::RenderedEmail::render(&M {
            to: Address::new(to).expect("valid"),
            marketing,
        })
        .expect("renders")
    }

    /// Suppression keys on the mailbox, so a differently-cased spelling of a
    /// bounced address is still suppressed.
    #[tokio::test]
    async fn a_lookup_is_case_insensitive() {
        let list = MemorySuppressionList::new();
        list.record(Suppression::new(
            Address::new("Ada@Example.com").expect("valid"),
            SuppressionReason::HardBounce,
        ))
        .await
        .expect("records");

        assert!(
            list.lookup("ada@example.com")
                .await
                .expect("looks up")
                .is_some()
        );
        assert!(
            list.lookup("ADA@EXAMPLE.COM")
                .await
                .expect("looks up")
                .is_some()
        );
        assert_eq!(list.len(), 1);
    }

    /// The acceptance criterion: suppression prevents the send.
    #[tokio::test]
    async fn a_hard_bounce_blocks_the_send() {
        let list = MemorySuppressionList::new();
        list.record(Suppression::new(
            Address::new("bounced@example.com").expect("valid"),
            SuppressionReason::HardBounce,
        ))
        .await
        .expect("records");

        let error = list
            .check(&message("bounced@example.com", false))
            .await
            .expect_err("suppressed");
        assert!(error.is_suppressed());
        assert!(error.to_string().contains("hard bounce"));
    }

    /// An unsubscribe is from marketing. Refusing a password reset because
    /// somebody left a newsletter would lock them out of their account.
    #[tokio::test]
    async fn an_unsubscribe_blocks_marketing_and_lets_transactional_through() {
        let list = MemorySuppressionList::new();
        list.record(Suppression::new(
            Address::new("left@example.com").expect("valid"),
            SuppressionReason::Unsubscribed,
        ))
        .await
        .expect("records");

        list.check(&message("left@example.com", false))
            .await
            .expect("transactional is allowed");
        let error = list
            .check(&message("left@example.com", true))
            .await
            .expect_err("marketing is refused");
        assert!(error.is_suppressed());
    }

    /// Nothing on the list means nothing is refused.
    #[tokio::test]
    async fn an_unlisted_recipient_passes() {
        let list = MemorySuppressionList::new();
        list.check(&message("fresh@example.com", true))
            .await
            .expect("not suppressed");
    }

    /// Releasing a permanent suppression by accident is how a sending domain
    /// gets blocklisted twice, so it takes an explicit `force`.
    #[tokio::test]
    async fn a_permanent_suppression_is_not_released_by_accident() {
        let list = MemorySuppressionList::new();
        list.record(Suppression::new(
            Address::new("gone@example.com").expect("valid"),
            SuppressionReason::Complaint,
        ))
        .await
        .expect("records");

        let error = list
            .release("gone@example.com", false)
            .await
            .expect_err("permanent");
        assert!(error.to_string().contains("spam complaint"));

        assert!(
            list.release("gone@example.com", true)
                .await
                .expect("forced")
        );
        assert!(list.is_empty());
    }

    /// A manual suppression is reversible without ceremony.
    #[tokio::test]
    async fn a_manual_suppression_releases_freely() {
        let list = MemorySuppressionList::new();
        list.record(Suppression::new(
            Address::new("oops@example.com").expect("valid"),
            SuppressionReason::Manual,
        ))
        .await
        .expect("records");

        assert!(
            list.release("oops@example.com", false)
                .await
                .expect("released")
        );
        assert!(!list.release("oops@example.com", false).await.expect("gone"));
    }

    /// Recording the same address twice replaces rather than duplicates: a
    /// soft signal followed by a hard one must end as the hard one.
    #[tokio::test]
    async fn recording_the_same_address_twice_replaces_the_entry() {
        let list = MemorySuppressionList::new();
        let address = Address::new("a@example.com").expect("valid");
        list.record(Suppression::new(address.clone(), SuppressionReason::Manual))
            .await
            .expect("records");
        list.record(Suppression::new(address, SuppressionReason::HardBounce))
            .await
            .expect("records");

        assert_eq!(list.len(), 1);
        let entry = list
            .lookup("a@example.com")
            .await
            .expect("looks up")
            .expect("present");
        assert_eq!(entry.reason(), SuppressionReason::HardBounce);
    }

    /// Paging walks the whole list exactly once, newest first, with no entry
    /// repeated or skipped across pages.
    #[tokio::test]
    async fn paging_covers_every_entry_exactly_once() {
        let list = MemorySuppressionList::new();
        let base = Utc::now();
        for index in 0..7_u32 {
            list.record(Suppression::at(
                Address::new(format!("a{index}@example.com")).expect("valid"),
                SuppressionReason::Manual,
                base + chrono::Duration::seconds(i64::from(index)),
            ))
            .await
            .expect("records");
        }

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let (page, next) = list.list(cursor.as_deref(), 3).await.expect("pages");
            assert!(page.len() <= 3);
            seen.extend(page.iter().map(|entry| entry.address().normalised()));
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        assert_eq!(seen.len(), 7);
        // Newest first.
        assert_eq!(seen[0], "a6@example.com");
        assert_eq!(seen[6], "a0@example.com");
        let unique: std::collections::BTreeSet<_> = seen.iter().collect();
        assert_eq!(unique.len(), 7);
    }
}
