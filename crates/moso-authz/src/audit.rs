//! The authorization audit trail.
//!
//! Every deny, and every allow on a permission marked `audit`, is recorded:
//! who, what, on what, the outcome, the reason, the request id, the timestamp
//! and the IP. Compliance-driven buyers ask for this by name, and its absence
//! is frequently what disqualifies a framework in an enterprise evaluation.
//!
//! **What is deliberately not recorded:** anything about the actor beyond their
//! identifier and their address. No email, no name, no request body. An audit
//! log that accumulates personal data is a liability with a retention policy
//! attached, and every field it does not hold is a field that cannot leak.
//!
//! # Getting the entries out of the process
//!
//! ```text
//! record()  ──▶ BatchingAuditSink ──▶ inner sink (table, tracing, yours)
//!                    │  buffer, at most `batch_size` entries
//!                    │
//!                    ├─ full batch      → written on the recording task
//!                    ├─ `flush_interval` → written by the flusher task
//!                    └─ AuditGuard      → written at shutdown
//! ```
//!
//! Three things follow from that shape. The buffer is bounded, because a full
//! batch is written before the entry that filled it returns. Nothing is written
//! twice, because an entry leaves the buffer exactly once — whichever of the
//! three paths takes it. And nothing is held forever on a quiet system, because
//! the flusher runs on a timer whether or not the batch is full.
//!
//! The one thing that can lose an entry is a process that exits without
//! flushing, which is what [`AuditGuard`] and [`flush_audit`] exist to prevent
//! and what [`audit_dropped`] counts when they were not wired up.

use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use std::borrow::Cow;
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Utc};
use moso_core::middleware::metrics;
use moso_core::{BoxFuture, Resolver};
use serde::{Deserialize, Serialize};

use crate::{ActorId, ActorKind, Scope};

/// What happened.
///
/// ```
/// use moso_authz::AuditOutcome;
///
/// assert!(AuditOutcome::Deny.is_deny());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    /// The action was allowed.
    Allow,
    /// The action was refused.
    Deny,
}

impl AuditOutcome {
    /// Whether this is a denial.
    ///
    /// ```
    /// use moso_authz::AuditOutcome;
    ///
    /// assert!(!AuditOutcome::Allow.is_deny());
    /// ```
    #[must_use]
    pub const fn is_deny(self) -> bool {
        matches!(self, Self::Deny)
    }
}

/// One row of the audit trail.
///
/// ```no_run
/// use moso_authz::AuditRecord;
///
/// # fn f(r: &AuditRecord) {
/// let _ = &r.reason;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AuditRecord {
    /// When.
    pub at: DateTime<Utc>,
    /// Who.
    pub actor: ActorId,
    /// What kind of thing was acting.
    pub actor_kind: ActorKind,
    /// Where they were acting.
    pub scope: Scope,
    /// What they tried to do — an action's name or a permission's wire name.
    pub action: String,
    /// What they tried to do it to, as `Name#id`, when there was a resource.
    pub resource: Option<String>,
    /// Allowed or denied.
    pub outcome: AuditOutcome,
    /// The reason the decision carried. Bounded to 200 characters, because a
    /// policy that puts a row's contents in its reason must not put them here.
    pub reason: String,
    /// The correlation id, so an entry joins to the request's logs and traces.
    pub request_id: Option<String>,
    /// The caller's address, as the trusted-proxy configuration resolved it.
    pub ip: Option<String>,
    /// The matched route pattern, never the raw path — a raw path is unbounded
    /// and lands in a metric label.
    pub route: Option<String>,
}

impl AuditRecord {
    /// A denial.
    ///
    /// ```no_run
    /// # use moso_authz::{ActorId, ActorKind, AuditRecord, Scope};
    /// # fn f(id: ActorId) {
    /// let _ = AuditRecord::deny(id, ActorKind::User, Scope::Global, "posts.publish", "no");
    /// # }
    /// ```
    #[must_use]
    pub fn deny(
        actor: ActorId,
        kind: ActorKind,
        scope: Scope,
        action: impl Into<String>,
        reason: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::new(actor, kind, scope, action, reason, AuditOutcome::Deny)
    }

    /// An allow, for a permission marked `audit`.
    ///
    /// ```no_run
    /// # use moso_authz::{ActorId, ActorKind, AuditRecord, Scope};
    /// # fn f(id: ActorId) {
    /// let _ = AuditRecord::allow(id, ActorKind::User, Scope::Global, "users.suspend", "admin");
    /// # }
    /// ```
    #[must_use]
    pub fn allow(
        actor: ActorId,
        kind: ActorKind,
        scope: Scope,
        action: impl Into<String>,
        reason: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::new(actor, kind, scope, action, reason, AuditOutcome::Allow)
    }

    /// The longest reason an entry stores.
    ///
    /// A policy is free to put a row's contents in its reason — that is what
    /// makes a reason useful when debugging — and an audit trail is exactly
    /// where those contents must not accumulate.
    pub const REASON_MAX: usize = 200;

    /// The constructor [`deny`](AuditRecord::deny) and
    /// [`allow`](AuditRecord::allow) share.
    ///
    /// ```no_run
    /// # use moso_authz::{ActorId, ActorKind, AuditOutcome, AuditRecord, Scope};
    /// let _ = AuditRecord::new(
    ///     ActorId::anonymous(),
    ///     ActorKind::Anonymous,
    ///     Scope::Global,
    ///     "posts.read",
    ///     "anonymous",
    ///     AuditOutcome::Deny,
    /// );
    /// ```
    #[must_use]
    pub fn new(
        actor: ActorId,
        kind: ActorKind,
        scope: Scope,
        action: impl Into<String>,
        reason: impl Into<Cow<'static, str>>,
        outcome: AuditOutcome,
    ) -> Self {
        Self {
            at: Utc::now(),
            actor,
            actor_kind: kind,
            scope,
            action: action.into(),
            resource: None,
            outcome,
            reason: truncate(reason.into().as_ref(), Self::REASON_MAX),
            request_id: None,
            ip: None,
            route: None,
        }
    }

    /// Attach the resource.
    ///
    /// ```no_run
    /// # use moso_authz::AuditRecord;
    /// # fn f(r: AuditRecord) { let _ = r.with_resource("Post", "456"); }
    /// ```
    #[must_use]
    pub fn with_resource(mut self, name: &str, id: &str) -> Self {
        self.resource = Some(format!("{name}#{id}"));
        self
    }

    /// Attach the request's correlation id, route and address.
    ///
    /// ```no_run
    /// # use moso_authz::AuditRecord;
    /// # fn f(r: AuditRecord) { let _ = r.with_request("01J…", Some("/posts/{id}"), None); }
    /// ```
    #[must_use]
    pub fn with_request(mut self, request_id: &str, route: Option<&str>, ip: Option<&str>) -> Self {
        self.request_id = Some(request_id.to_owned());
        self.route = route.map(ToOwned::to_owned);
        self.ip = ip.map(ToOwned::to_owned);
        self
    }
}

/// Cut a reason to `max` characters, marking that it was cut.
///
/// Counted in `char`s: a reason is arbitrary text from a policy author, and
/// slicing a multi-byte character in half is a panic.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Where audit records go.
///
/// Dyn-compatible: the shipped implementations are a table, a tracing target
/// and an in-memory vector for tests, and an application picks in
/// configuration.
///
/// ```no_run
/// use moso_authz::{AuditRecord, AuditSink};
///
/// async fn record(sink: &dyn AuditSink, entry: AuditRecord) {
///     sink.record(entry).await;
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not an audit sink",
    label = "not an audit sink",
    note = "an audit sink implements `record`, which cannot fail — see the note on the trait \
            about why",
    note = "help: use `MemoryAuditSink` in tests, `TracingAuditSink` to send entries to your \
            log pipeline, or the table-backed sink the migration generator creates from \
            `moso_authz_audit`"
)]
pub trait AuditSink: Send + Sync + 'static {
    /// Record one entry.
    ///
    /// Returns nothing, deliberately. A failing audit write must not fail the
    /// request that produced it: the request has already been decided, and
    /// turning "the audit table is full" into a 500 on every endpoint is a
    /// worse outcome than a logged write failure and a metric. Sinks that
    /// cannot write log at `error` and increment `moso_authz_audit_dropped`.
    fn record<'a>(&'a self, entry: AuditRecord) -> BoxFuture<'a, ()>;

    /// Flush anything buffered. Called during the shutdown drain.
    fn flush(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// How much is audited, and for how long.
///
/// ```
/// use moso_authz::AuditConfig;
///
/// let config = AuditConfig::default();
/// assert!(config.denies);
/// assert!(!config.allows);
/// ```
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct AuditConfig {
    /// Record every denial. On by default, and turning it off is a decision an
    /// operator should have to make in writing.
    pub denies: bool,
    /// Record every allow, not just the ones on `audit` permissions. Off by
    /// default: it is a row per authorised request.
    pub allows: bool,
    /// How many days entries are kept. Zero means forever.
    ///
    /// Read by [`AuditConfig::retention_cutoff`], which is what
    /// [`TableAuditSink::purge_expired`](crate::TableAuditSink::purge_expired)
    /// and the periodic purge compute their cutoff from.
    pub retention_days: u32,
    /// How many entries to buffer before writing. One means write-through.
    ///
    /// Read by [`BatchingAuditSink`]; a plain sink ignores it, because a sink
    /// that writes through has nothing to batch.
    pub batch_size: usize,
    /// How long a partial batch may wait before it is written anyway.
    ///
    /// Without it a system quiet enough never to fill a batch would hold its
    /// entries until shutdown, which is the same as not auditing.
    pub flush_interval: Duration,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            denies: true,
            allows: false,
            retention_days: 365,
            batch_size: 1,
            flush_interval: Duration::from_secs(5),
        }
    }
}

impl AuditConfig {
    /// The timestamp entries older than which have aged out, or `None` when
    /// [`retention_days`](AuditConfig::retention_days) is zero.
    ///
    /// The one place the "days into a cutoff" rule lives, so the periodic purge
    /// and a hand-written migration cannot disagree about what 365 means.
    ///
    /// ```
    /// use chrono::{TimeZone, Utc};
    /// use moso_authz::AuditConfig;
    ///
    /// let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().expect("a real instant");
    /// let cutoff = AuditConfig::default().retention_cutoff(now).expect("365 days is a window");
    ///
    /// assert_eq!(cutoff.to_rfc3339(), "2025-01-01T00:00:00+00:00");
    /// ```
    #[must_use]
    pub fn retention_cutoff(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if self.retention_days == 0 {
            return None;
        }
        now.checked_sub_signed(chrono::Duration::days(i64::from(self.retention_days)))
    }

    /// The batch size, with `0` read as write-through rather than as "never
    /// write".
    ///
    /// ```
    /// use moso_authz::AuditConfig;
    ///
    /// assert_eq!(AuditConfig::default().effective_batch_size(), 1);
    /// ```
    #[must_use]
    pub fn effective_batch_size(&self) -> usize {
        self.batch_size.max(1)
    }

    /// The flush interval, with a zero read as the documented default rather
    /// than as a timer that fires continuously.
    ///
    /// ```
    /// use moso_authz::AuditConfig;
    ///
    /// assert_eq!(AuditConfig::default().effective_flush_interval().as_secs(), 5);
    /// ```
    #[must_use]
    pub fn effective_flush_interval(&self) -> Duration {
        if self.flush_interval.is_zero() {
            Self::default().flush_interval
        } else {
            self.flush_interval
        }
    }
}

/// An audit sink that keeps entries in a vector. For tests.
///
/// ```no_run
/// use moso_authz::MemoryAuditSink;
///
/// let sink = MemoryAuditSink::new();
/// assert!(sink.entries().is_empty());
/// ```
#[derive(Debug, Default)]
pub struct MemoryAuditSink {
    /// Everything recorded, in order.
    entries: std::sync::RwLock<Vec<AuditRecord>>,
}

impl MemoryAuditSink {
    /// An empty sink.
    ///
    /// ```no_run
    /// use moso_authz::MemoryAuditSink;
    ///
    /// let _ = MemoryAuditSink::new();
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything recorded so far, in order.
    ///
    /// ```
    /// use moso_authz::MemoryAuditSink;
    ///
    /// assert!(MemoryAuditSink::new().entries().is_empty());
    /// ```
    #[must_use]
    pub fn entries(&self) -> Vec<AuditRecord> {
        self.lock().clone()
    }

    /// How many entries were recorded.
    ///
    /// ```
    /// use moso_authz::MemoryAuditSink;
    ///
    /// assert_eq!(MemoryAuditSink::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing was recorded.
    ///
    /// ```
    /// use moso_authz::MemoryAuditSink;
    ///
    /// assert!(MemoryAuditSink::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Only the denials.
    ///
    /// ```
    /// use moso_authz::MemoryAuditSink;
    ///
    /// assert!(MemoryAuditSink::new().denials().is_empty());
    /// ```
    #[must_use]
    pub fn denials(&self) -> Vec<AuditRecord> {
        self.lock()
            .iter()
            .filter(|entry| entry.outcome.is_deny())
            .cloned()
            .collect()
    }

    /// Forget everything recorded.
    ///
    /// ```
    /// use moso_authz::MemoryAuditSink;
    ///
    /// let sink = MemoryAuditSink::new();
    /// sink.clear();
    /// assert!(sink.is_empty());
    /// ```
    pub fn clear(&self) {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// The vector, recovering from a poisoned lock.
    ///
    /// Nothing in a critical section here can panic — it pushes and clones — so
    /// a poisoned lock means a panic elsewhere in the process, and refusing to
    /// record an audit entry over that would lose exactly the evidence somebody
    /// will want.
    fn lock(&self) -> std::sync::RwLockReadGuard<'_, Vec<AuditRecord>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl AuditSink for MemoryAuditSink {
    fn record<'a>(&'a self, entry: AuditRecord) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.entries
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(entry);
        })
    }
}

/// An audit sink that emits a `tracing` event per entry.
///
/// For deployments whose log pipeline is already the system of record. The
/// target is `moso::authz::audit`, so a filter can route it separately from
/// application logs.
///
/// ```no_run
/// use moso_authz::TracingAuditSink;
///
/// let _ = TracingAuditSink::new();
/// ```
#[derive(Debug, Default)]
pub struct TracingAuditSink;

impl TracingAuditSink {
    /// A sink that logs.
    ///
    /// ```no_run
    /// use moso_authz::TracingAuditSink;
    ///
    /// let _ = TracingAuditSink::new();
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl AuditSink for TracingAuditSink {
    fn record<'a>(&'a self, entry: AuditRecord) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // `info` and not `warn`: a denial is a normal outcome of a correct
            // authorization model, and a log level that says otherwise trains
            // operators to ignore it.
            tracing::info!(
                target: AUDIT_TARGET,
                at = %entry.at.to_rfc3339(),
                actor = %entry.actor,
                actor_kind = entry.actor_kind.as_str(),
                scope = %entry.scope.as_key(),
                action = %entry.action,
                resource = entry.resource.as_deref().unwrap_or("-"),
                outcome = if entry.outcome.is_deny() { "deny" } else { "allow" },
                reason = %entry.reason,
                request_id = entry.request_id.as_deref().unwrap_or("-"),
                ip = entry.ip.as_deref().unwrap_or("-"),
                route = entry.route.as_deref().unwrap_or("-"),
                "authorization decision"
            );
        })
    }
}

// ---------------------------------------------------------------------------
// The dropped-entry counter
// ---------------------------------------------------------------------------

/// The name of the counter [`count_dropped`] increments.
///
/// Exported so an exporter names the series the documentation names, rather
/// than a string somebody retyped. It is the exact `&'static str`
/// [`count_dropped`] hands to [`moso_core::middleware::metrics::counter`].
///
/// ```
/// assert_eq!(moso_authz::audit::DROPPED_METRIC, "moso_authz_audit_dropped");
/// ```
pub const DROPPED_METRIC: &str = "moso_authz_audit_dropped";

/// The in-process read mirror behind [`audit_dropped`].
///
/// The metric's home is the core registry (see [`count_dropped`]); this atomic
/// exists only so the count is observable in-process without an exporter.
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// How many audit entries this process failed to write.
///
/// # Where the number lives now
///
/// The authoritative series is the core metric registry, not a private atomic
/// this crate owns: [`count_dropped`] reports every loss through
/// [`moso_core::middleware::metrics::counter`] under [`DROPPED_METRIC`], so an
/// operator's exporter scrapes `moso_authz_audit_dropped` alongside every other
/// Moso metric — no handle threaded through every sink to reach it. The
/// process-wide exception that once justified a bare atomic here now lives on
/// the core counter, next to the one
/// [`moso_core::middleware::metrics::requests_total`] documents.
///
/// This function keeps a process-wide read mirror so the loss is observable
/// in-process even when no exporter is wired — a test, or a one-off tool. It is
/// monotonic and shared across every `App` in the process, which is the honest
/// reading: the entries really were lost by this process, whichever `App` was
/// serving.
///
/// ```
/// // Monotonic, so a scrape only ever sees it go up. Process-wide, so a test
/// // asserts on the direction rather than on an exact total.
/// let before = moso_authz::audit::audit_dropped();
/// moso_authz::audit::count_dropped(2);
///
/// assert!(moso_authz::audit::audit_dropped() >= before.saturating_add(2));
/// ```
#[must_use]
pub fn audit_dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Record that `entries` audit entries could not be written.
///
/// Called by every shipped sink on its failure path, and public so a
/// hand-written one counts its losses in the same series rather than inventing
/// a second one. The loss is reported to the core metric registry — an
/// increment on [`moso_core::middleware::metrics::counter`] named
/// [`DROPPED_METRIC`] — and mirrored into the in-process counter
/// [`audit_dropped`] reads.
///
/// ```
/// let before = moso_authz::audit::audit_dropped();
/// moso_authz::audit::count_dropped(1);
///
/// assert!(moso_authz::audit::audit_dropped() > before);
/// ```
pub fn count_dropped(entries: u64) {
    metrics::counter(DROPPED_METRIC).increment(entries);
    DROPPED.fetch_add(entries, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Batching
// ---------------------------------------------------------------------------

/// An audit sink that writes entries to another sink in batches.
///
/// One `INSERT` per denial is fine at ten denials a minute and is the reason
/// somebody turns auditing off at ten thousand. This buffers up to
/// [`AuditConfig::batch_size`] entries and hands the whole batch to the inner
/// sink in one call.
///
/// # What it promises
///
/// **Bounded memory.** The buffer never holds more than `batch_size` entries:
/// the [`record`](AuditSink::record) call that fills it takes the batch and
/// writes it before returning, so a slow inner sink slows the recording task
/// down instead of growing a queue behind it.
///
/// **Lossless while the process lives.** An entry leaves the buffer exactly
/// once, into whichever writer took it, and [`AuditSink::record`] cannot fail.
/// The one way to lose one is to exit without flushing — see [`AuditGuard`].
///
/// **Nothing is held forever.** [`start`](BatchingAuditSink::start) spawns a
/// flusher that writes whatever is buffered every
/// [`AuditConfig::flush_interval`], so a partial batch on a quiet system still
/// reaches the inner sink.
///
/// ```
/// use std::sync::Arc;
///
/// use moso_authz::audit::{AuditSink, BatchingAuditSink};
/// use moso_authz::{ActorId, ActorKind, AuditRecord, MemoryAuditSink, Scope};
///
/// # tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
/// let inner = Arc::new(MemoryAuditSink::new());
/// let sink = BatchingAuditSink::new(Arc::clone(&inner) as Arc<dyn AuditSink>, 3);
///
/// for index in 0..2 {
///     sink.record(AuditRecord::deny(
///         ActorId::new(format!("usr_{index}")),
///         ActorKind::User,
///         Scope::Global,
///         "posts.publish",
///         "not the author",
///     ))
///     .await;
/// }
///
/// // Two of three: still buffered, and nothing has reached the inner sink.
/// assert_eq!(sink.buffered(), 2);
/// assert!(inner.is_empty());
///
/// sink.flush().await;
/// assert_eq!(inner.len(), 2);
/// assert_eq!(sink.buffered(), 0);
/// # });
/// ```
pub struct BatchingAuditSink {
    /// Where a full batch goes.
    inner: Arc<dyn AuditSink>,
    /// How many entries a batch holds, at least one.
    batch_size: usize,
    /// The entries not yet handed to `inner`.
    buffer: Mutex<Vec<AuditRecord>>,
}

impl BatchingAuditSink {
    /// Buffer up to `batch_size` entries before writing them to `inner`.
    ///
    /// A `batch_size` of zero is read as one — write-through — because a batch
    /// that never fills is a sink that never writes.
    ///
    /// No timer is started: use [`start`](BatchingAuditSink::start) for that,
    /// which needs a Tokio runtime, or call [`AuditSink::flush`] yourself.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_authz::audit::{AuditSink, BatchingAuditSink};
    /// use moso_authz::MemoryAuditSink;
    ///
    /// let inner = Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>;
    /// assert_eq!(BatchingAuditSink::new(inner, 50).batch_size(), 50);
    /// ```
    #[must_use]
    pub fn new(inner: Arc<dyn AuditSink>, batch_size: usize) -> Self {
        Self {
            inner,
            batch_size: batch_size.max(1),
            buffer: Mutex::new(Vec::new()),
        }
    }

    /// Wrap `inner` and start the flusher the configuration asks for.
    ///
    /// The pair an application registers: the sink goes into the provider map,
    /// the guard into the lifespan, and the guard is what stops the timer and
    /// writes the last partial batch.
    ///
    /// ```text
    /// let (sink, guard) = BatchingAuditSink::start(
    ///     Arc::new(TableAuditSink::new(db.clone())),
    ///     &audit,
    /// );
    ///
    /// App::new(config)
    ///     .provide(audit)
    ///     .provide_dyn::<dyn AuditSink>(sink)
    ///     .lifespan(move |_| async move { Ok(guard) })
    ///     .on_shutdown(|resolver| async move { moso_authz::audit::flush_audit(&resolver).await })
    /// ```
    ///
    /// # Panics
    ///
    /// If there is no Tokio runtime, because the flusher is a spawned task.
    /// Call it from inside `#[tokio::main]`, an `on_startup` hook or a
    /// `lifespan` factory — every one of which already runs on the runtime.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// use moso_authz::audit::{AuditSink, BatchingAuditSink};
    /// use moso_authz::{AuditConfig, MemoryAuditSink};
    ///
    /// # tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap()
    /// #     .block_on(async {
    /// let mut config = AuditConfig::default();
    /// config.batch_size = 100;
    /// config.flush_interval = Duration::from_millis(20);
    ///
    /// let inner = Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>;
    /// let (sink, guard) = BatchingAuditSink::start(inner, &config);
    ///
    /// assert_eq!(sink.batch_size(), 100);
    /// guard.shutdown().await;
    /// # });
    /// ```
    #[must_use]
    pub fn start(inner: Arc<dyn AuditSink>, config: &AuditConfig) -> (Arc<Self>, AuditGuard) {
        let sink = Arc::new(Self::new(inner, config.effective_batch_size()));
        let guard = AuditGuard::spawn(Arc::clone(&sink), config.effective_flush_interval());
        (sink, guard)
    }

    /// How many entries a full batch holds.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_authz::audit::{AuditSink, BatchingAuditSink};
    /// use moso_authz::MemoryAuditSink;
    ///
    /// let inner = Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>;
    /// assert_eq!(BatchingAuditSink::new(inner, 0).batch_size(), 1, "zero is write-through");
    /// ```
    #[must_use]
    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    /// How many entries are waiting to be written.
    ///
    /// Never more than [`batch_size`](BatchingAuditSink::batch_size), which is
    /// the memory bound this type promises.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_authz::audit::{AuditSink, BatchingAuditSink};
    /// use moso_authz::MemoryAuditSink;
    ///
    /// let inner = Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>;
    /// assert_eq!(BatchingAuditSink::new(inner, 8).buffered(), 0);
    /// ```
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.lock().len()
    }

    /// The sink a batch is written to.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_authz::audit::{AuditSink, BatchingAuditSink};
    /// use moso_authz::MemoryAuditSink;
    ///
    /// let inner = Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>;
    /// let sink = BatchingAuditSink::new(Arc::clone(&inner), 8);
    ///
    /// assert!(Arc::ptr_eq(sink.inner(), &inner));
    /// ```
    #[must_use]
    pub fn inner(&self) -> &Arc<dyn AuditSink> {
        &self.inner
    }

    /// Everything buffered, leaving the buffer empty.
    ///
    /// The only way an entry leaves the buffer, which is what makes "written
    /// exactly once" true however many writers are running.
    fn take(&self) -> Vec<AuditRecord> {
        core::mem::take(&mut *self.lock())
    }

    /// Add `entry`, and take the whole batch when it is now full.
    fn push(&self, entry: AuditRecord) -> Option<Vec<AuditRecord>> {
        let mut buffer = self.lock();
        buffer.push(entry);
        if buffer.len() >= self.batch_size {
            return Some(core::mem::take(&mut *buffer));
        }
        None
    }

    /// Hand a batch to the inner sink, one entry at a time.
    ///
    /// One call per entry rather than a batched `INSERT`, because
    /// [`AuditSink`] has no batched method and inventing one would make every
    /// hand-written sink implement two things that must agree. The saving is
    /// real regardless: the entries are written together, on one task, without
    /// a request waiting behind each of them.
    async fn write_batch(&self, batch: Vec<AuditRecord>) {
        for entry in batch {
            self.inner.record(entry).await;
        }
    }

    /// The buffer, recovering from a poisoned lock.
    ///
    /// Nothing in a critical section here can panic — it pushes and swaps a
    /// vector — so a poisoned lock means a panic elsewhere in the process, and
    /// refusing to record over that would lose exactly the evidence somebody
    /// will want.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<AuditRecord>> {
        self.buffer.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl core::fmt::Debug for BatchingAuditSink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BatchingAuditSink")
            .field("batch_size", &self.batch_size)
            .field("buffered", &self.buffered())
            .finish_non_exhaustive()
    }
}

impl AuditSink for BatchingAuditSink {
    fn record<'a>(&'a self, entry: AuditRecord) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(batch) = self.push(entry) {
                self.write_batch(batch).await;
            }
        })
    }

    fn flush(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let batch = self.take();
            if !batch.is_empty() {
                self.write_batch(batch).await;
            }
            self.inner.flush().await;
        })
    }
}

/// Stops the flusher and writes the last partial batch.
///
/// Held by the application for the life of the process, which is what
/// [`AppBuilder::lifespan`](moso_core::AppBuilder::lifespan) is for:
///
/// ```text
/// .lifespan(move |_| async move { Ok(guard) })
/// ```
///
/// # Prefer `shutdown()`
///
/// [`shutdown`](AuditGuard::shutdown) is `async`, so it can write the last
/// batch and *wait* for it. `Drop` cannot await: on a multi-threaded runtime it
/// falls back to blocking the current worker while the batch is written, and
/// anywhere else — a current-thread runtime, a thread with no runtime at all —
/// it has no way to write and instead logs at `error` and counts the entries
/// through [`count_dropped`]. A guard is therefore a safety net, and the
/// `on_shutdown` hook in [`flush_audit`] is the belt.
pub struct AuditGuard {
    /// What is flushed.
    sink: Arc<BatchingAuditSink>,
    /// The flusher, until it is stopped.
    flusher: Option<tokio::task::JoinHandle<()>>,
}

impl AuditGuard {
    /// Start the flusher for `sink`.
    ///
    /// # Panics
    ///
    /// If there is no Tokio runtime.
    fn spawn(sink: Arc<BatchingAuditSink>, every: Duration) -> Self {
        let flushed = Arc::clone(&sink);
        let flusher = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            // The first tick completes immediately; skipping it keeps the
            // flusher from writing an empty batch the moment it starts.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                flushed.flush().await;
            }
        });
        Self {
            sink,
            flusher: Some(flusher),
        }
    }

    /// How many entries are still waiting.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_authz::audit::{AuditSink, BatchingAuditSink};
    /// use moso_authz::{AuditConfig, MemoryAuditSink};
    ///
    /// # tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap()
    /// #     .block_on(async {
    /// let inner = Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>;
    /// let (_sink, guard) = BatchingAuditSink::start(inner, &AuditConfig::default());
    ///
    /// assert_eq!(guard.buffered(), 0);
    /// guard.shutdown().await;
    /// # });
    /// ```
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.sink.buffered()
    }

    /// Stop the flusher and write everything still buffered.
    ///
    /// The path that is guaranteed to write: it awaits the batch instead of
    /// hoping a `Drop` can.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_authz::audit::{AuditSink, BatchingAuditSink};
    /// use moso_authz::{ActorId, ActorKind, AuditConfig, AuditRecord, MemoryAuditSink, Scope};
    ///
    /// # tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap()
    /// #     .block_on(async {
    /// let mut config = AuditConfig::default();
    /// config.batch_size = 100;
    ///
    /// let inner = Arc::new(MemoryAuditSink::new());
    /// let written = Arc::clone(&inner) as Arc<dyn AuditSink>;
    /// let (sink, guard) = BatchingAuditSink::start(written, &config);
    ///
    /// sink.record(AuditRecord::deny(
    ///     ActorId::new("usr_1"),
    ///     ActorKind::User,
    ///     Scope::Global,
    ///     "posts.publish",
    ///     "not the author",
    /// ))
    /// .await;
    /// assert!(inner.is_empty(), "one entry is not a batch of a hundred");
    ///
    /// guard.shutdown().await;
    /// assert_eq!(inner.len(), 1, "shutdown writes the partial batch");
    /// # });
    /// ```
    pub async fn shutdown(mut self) {
        if let Some(flusher) = self.flusher.take() {
            flusher.abort();
        }
        self.sink.flush().await;
    }

    /// Write whatever `Drop` was left holding, or say what was lost.
    fn release(&mut self) {
        if let Some(flusher) = self.flusher.take() {
            flusher.abort();
        }
        if self.sink.buffered() == 0 || self.flush_blocking() {
            return;
        }
        let lost = self.sink.buffered();
        count_dropped(lost as u64);
        tracing::error!(
            target: AUDIT_TARGET,
            dropped = lost,
            metric = DROPPED_METRIC,
            "an `AuditGuard` was dropped where the last batch could not be written, and those \
             entries are gone\n  help: call `AuditGuard::shutdown().await` from an `on_shutdown` \
             hook, which can wait for the write instead of blocking for it"
        );
    }

    /// Write the last batch by blocking, when the runtime allows it. `true` if
    /// it was written.
    ///
    /// [`tokio::task::block_in_place`] moves this worker's other tasks aside so
    /// the batch can be awaited, and it exists only on the multi-threaded
    /// runtime — which is what `#[tokio::main]` and `App::serve` run on, so this
    /// is the ordinary path rather than the exotic one.
    ///
    /// There is no public way to ask whether *this* thread is a worker, only to
    /// try; and a panic escaping a `Drop` during unwinding aborts the process.
    /// So the attempt is caught and reported as a loss, which is the same
    /// outcome with a log line instead of a core dump.
    fn flush_blocking(&self) -> bool {
        let multi_threaded = tokio::runtime::Handle::try_current().ok().filter(|handle| {
            matches!(
                handle.runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread
            )
        });
        let Some(handle) = multi_threaded else {
            return false;
        };

        let sink = Arc::clone(&self.sink);
        let written = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            tokio::task::block_in_place(|| handle.block_on(async move { sink.flush().await }));
        }));
        written.is_ok()
    }
}

impl Drop for AuditGuard {
    fn drop(&mut self) {
        self.release();
    }
}

impl core::fmt::Debug for AuditGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuditGuard")
            .field("buffered", &self.buffered())
            .field("flushing", &self.flusher.is_some())
            .finish()
    }
}

/// Flush the registered audit sink, whatever it is.
///
/// [`AuditSink::flush`] documents the shutdown drain as its call site, and
/// nothing in `moso-core` knows this crate exists, so this is the one line that
/// connects the two:
///
/// ```text
/// App::new(config)
///     .provide_dyn::<dyn AuditSink>(sink)
///     .on_shutdown(|resolver| async move { moso_authz::audit::flush_audit(&resolver).await })
/// ```
///
/// An application that registered no sink is not an error: the fallback is the
/// tracing sink, which buffers nothing and has nothing to flush. Doing nothing
/// quietly is right here, because the hook is wiring an application copies once
/// and should not have to guard.
///
/// ```
/// use std::sync::Arc;
///
/// use moso_authz::audit::flush_audit;
/// use moso_core::{ProviderMap, Resolver};
///
/// # tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
/// // No sink registered: the call is a no-op rather than a failure.
/// let resolver = Resolver::new(Arc::new(ProviderMap::new()));
/// flush_audit(&resolver).await;
/// # });
/// ```
pub async fn flush_audit(resolver: &Resolver) {
    if let Ok(sink) = resolver.get_dyn::<dyn AuditSink>() {
        sink.flush().await;
    }
}

/// The `tracing` target every audit entry is emitted on.
///
/// Its own target so a subscriber filter can route the audit trail to a
/// different sink from the application's logs — which is the whole point of
/// [`TracingAuditSink`] for a deployment whose log pipeline is the system of
/// record.
///
/// ```
/// assert_eq!(moso_authz::audit::AUDIT_TARGET, "moso::authz::audit");
/// ```
pub const AUDIT_TARGET: &str = "moso::authz::audit";

/// Record a decision, honouring the configuration and never failing the request.
///
/// The one place the "which decisions are audited" rule lives: every denial
/// when [`AuditConfig::denies`] is on, every allow when
/// [`AuditConfig::allows`] is on or the call site asked for it with
/// `#[requires(.., audit)]`.
///
/// ```
/// use moso_authz::audit::record_if_wanted;
/// use moso_authz::{ActorId, ActorKind, AuditConfig, AuditRecord, MemoryAuditSink, Scope};
///
/// # tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
/// let sink = MemoryAuditSink::new();
/// let config = AuditConfig::default();
/// let allow = AuditRecord::allow(
///     ActorId::new("usr_1"),
///     ActorKind::User,
///     Scope::Global,
///     "posts.read",
///     "viewer",
/// );
///
/// // Allows are off by default and this call site did not ask for one.
/// record_if_wanted(&sink, &config, allow.clone(), false).await;
/// assert!(sink.is_empty());
///
/// // `#[requires(.., audit)]` is what `forced` means.
/// record_if_wanted(&sink, &config, allow, true).await;
/// assert_eq!(sink.len(), 1);
/// # });
/// ```
pub async fn record_if_wanted(
    sink: &dyn AuditSink,
    config: &AuditConfig,
    entry: AuditRecord,
    forced: bool,
) {
    let wanted = match entry.outcome {
        AuditOutcome::Deny => config.denies,
        AuditOutcome::Allow => config.allows || forced,
    };
    if wanted {
        sink.record(entry).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScopeId;

    fn deny() -> AuditRecord {
        AuditRecord::deny(
            ActorId::new("usr_1"),
            ActorKind::User,
            Scope::Org(ScopeId::new("acme")),
            "posts.publish",
            "not the author and not an admin",
        )
    }

    #[test]
    fn a_denial_records_who_what_and_why() {
        let entry = deny().with_resource("Post", "456").with_request(
            "01JABCDEF",
            Some("/posts/{id}/publish"),
            Some("203.0.113.7"),
        );

        assert_eq!(entry.actor.as_str(), "usr_1");
        assert_eq!(entry.actor_kind, ActorKind::User);
        assert_eq!(entry.scope.as_key(), "org:acme");
        assert_eq!(entry.action, "posts.publish");
        assert_eq!(entry.resource.as_deref(), Some("Post#456"));
        assert_eq!(entry.outcome, AuditOutcome::Deny);
        assert!(entry.outcome.is_deny());
        assert_eq!(entry.request_id.as_deref(), Some("01JABCDEF"));
        assert_eq!(entry.route.as_deref(), Some("/posts/{id}/publish"));
        assert_eq!(entry.ip.as_deref(), Some("203.0.113.7"));
    }

    /// Acceptance criterion 7: no PII beyond the actor id and the address. The
    /// route pattern is recorded, never the raw path — a raw path is unbounded
    /// and carries whatever the caller put in it.
    #[test]
    fn an_entry_carries_no_field_that_could_hold_personal_data() {
        let entry = deny().with_request("01J", Some("/posts/{id}"), Some("203.0.113.7"));
        let encoded = serde_json::to_value(&entry).expect("encode");
        let object = encoded.as_object().expect("an object");

        let mut fields: Vec<&str> = object.keys().map(String::as_str).collect();
        fields.sort_unstable();

        assert_eq!(
            fields,
            [
                "action",
                "actor",
                "actor_kind",
                "at",
                "ip",
                "outcome",
                "reason",
                "request_id",
                "resource",
                "route",
                "scope",
            ],
            "a new field on an audit record is a retention-policy decision",
        );
    }

    /// A policy is free to put a row's contents in its reason. The audit trail
    /// is exactly where those contents must not accumulate.
    #[test]
    fn a_long_reason_is_cut_and_marked() {
        let entry = AuditRecord::deny(
            ActorId::anonymous(),
            ActorKind::Anonymous,
            Scope::Global,
            "posts.read",
            "x".repeat(500),
        );

        assert_eq!(entry.reason.chars().count(), AuditRecord::REASON_MAX);
        assert!(entry.reason.ends_with('…'));
    }

    #[test]
    fn a_reason_at_the_limit_is_left_alone() {
        let exact = "y".repeat(AuditRecord::REASON_MAX);
        let entry = AuditRecord::allow(
            ActorId::anonymous(),
            ActorKind::Anonymous,
            Scope::Global,
            "posts.read",
            exact.clone(),
        );

        assert_eq!(entry.reason, exact);
    }

    #[test]
    fn truncation_counts_characters_and_not_bytes() {
        assert_eq!(truncate("ééé", 2), "é…");
        assert_eq!(truncate("abc", 10), "abc");
    }

    // ── sinks ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn the_memory_sink_keeps_what_it_is_given_in_order() {
        let sink = MemoryAuditSink::new();

        assert!(sink.is_empty());
        sink.record(deny()).await;
        sink.record(AuditRecord::allow(
            ActorId::new("usr_2"),
            ActorKind::ApiKey,
            Scope::Global,
            "posts.read",
            "viewer",
        ))
        .await;

        assert_eq!(sink.len(), 2);
        assert_eq!(sink.entries()[0].actor.as_str(), "usr_1");
        assert_eq!(sink.denials().len(), 1);

        sink.clear();
        assert!(sink.entries().is_empty());
    }

    #[tokio::test]
    async fn the_tracing_sink_never_fails_the_request_that_produced_the_entry() {
        // `record` returns `()` on purpose: the request has already been
        // decided, and turning "the audit table is full" into a 500 on every
        // endpoint is worse than a logged write failure.
        let sink = TracingAuditSink::new();
        sink.record(deny()).await;
        sink.flush().await;
    }

    #[tokio::test]
    async fn the_default_configuration_records_denials_and_not_allows() {
        let sink = MemoryAuditSink::new();
        let config = AuditConfig::default();

        assert!(config.denies);
        assert!(!config.allows);
        assert_eq!(config.retention_days, 365);

        record_if_wanted(&sink, &config, deny(), false).await;
        assert_eq!(sink.len(), 1, "every denial is recorded");

        let allow = AuditRecord::allow(
            ActorId::new("usr_1"),
            ActorKind::User,
            Scope::Global,
            "posts.read",
            "viewer",
        );
        record_if_wanted(&sink, &config, allow.clone(), false).await;
        assert_eq!(sink.len(), 1, "an ordinary allow is a row per request");

        record_if_wanted(&sink, &config, allow, true).await;
        assert_eq!(sink.len(), 2, "`#[requires(.., audit)]` forces the allow");
    }

    #[tokio::test]
    async fn turning_denials_off_is_honoured_but_has_to_be_written_down() {
        let sink = MemoryAuditSink::new();
        let config = AuditConfig {
            denies: false,
            ..AuditConfig::default()
        };

        record_if_wanted(&sink, &config, deny(), false).await;
        assert!(sink.is_empty());
    }

    #[tokio::test]
    async fn recording_every_allow_is_one_switch() {
        let sink = MemoryAuditSink::new();
        let config = AuditConfig {
            allows: true,
            ..AuditConfig::default()
        };

        record_if_wanted(
            &sink,
            &config,
            AuditRecord::allow(
                ActorId::new("usr_1"),
                ActorKind::User,
                Scope::Global,
                "posts.read",
                "viewer",
            ),
            false,
        )
        .await;

        assert_eq!(sink.len(), 1);
    }

    #[test]
    fn a_record_round_trips_through_json() {
        let entry = deny().with_resource("Post", "456");
        let encoded = serde_json::to_string(&entry).expect("encode");
        let decoded: AuditRecord = serde_json::from_str(&encoded).expect("decode");

        assert_eq!(decoded.action, entry.action);
        assert_eq!(decoded.resource, entry.resource);
        assert_eq!(decoded.outcome, entry.outcome);
    }

    #[test]
    fn the_audit_target_is_its_own_so_a_filter_can_route_it() {
        assert_eq!(AUDIT_TARGET, "moso::authz::audit");
    }

    // ── retention ─────────────────────────────────────────────────────────

    /// The one place "days" becomes a timestamp, so the configured number and
    /// the `DELETE` cannot mean different things.
    #[test]
    fn the_retention_window_is_computed_from_the_configured_days() {
        let now = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 1, 1, 0, 0, 0)
            .single()
            .expect("a real instant");

        let cutoff = AuditConfig::default()
            .retention_cutoff(now)
            .expect("365 days is a window");
        assert_eq!(cutoff, now - chrono::Duration::days(365));

        let forever = AuditConfig {
            retention_days: 0,
            ..AuditConfig::default()
        };
        assert_eq!(
            forever.retention_cutoff(now),
            None,
            "zero means keep forever, not delete everything",
        );
    }

    #[test]
    fn a_zero_batch_size_and_a_zero_interval_read_as_the_defaults() {
        let config = AuditConfig {
            batch_size: 0,
            flush_interval: Duration::ZERO,
            ..AuditConfig::default()
        };

        assert_eq!(config.effective_batch_size(), 1, "zero is write-through");
        assert_eq!(config.effective_flush_interval(), Duration::from_secs(5));
        assert_eq!(AuditConfig::default().effective_batch_size(), 1);
    }

    // ── the dropped counter ───────────────────────────────────────────────

    /// The counter is process-wide by design, so this asserts that it moved in
    /// the right direction by at least the right amount: another test in the
    /// same process counts into the same series, which is exactly what
    /// "process-wide" means.
    #[test]
    fn dropped_entries_are_counted_where_an_exporter_can_read_them() {
        let before = audit_dropped();
        count_dropped(3);

        assert!(audit_dropped() >= before.saturating_add(3));
        assert_eq!(DROPPED_METRIC, "moso_authz_audit_dropped");
    }

    // ── batching ──────────────────────────────────────────────────────────

    fn allow(id: &str) -> AuditRecord {
        AuditRecord::allow(
            ActorId::new(id),
            ActorKind::User,
            Scope::Global,
            "posts.read",
            "viewer",
        )
    }

    #[tokio::test]
    async fn a_partial_batch_stays_buffered_and_a_full_one_is_written() {
        let inner = Arc::new(MemoryAuditSink::new());
        let sink = BatchingAuditSink::new(Arc::clone(&inner) as Arc<dyn AuditSink>, 3);

        sink.record(allow("usr_1")).await;
        sink.record(allow("usr_2")).await;
        assert_eq!(sink.buffered(), 2);
        assert!(inner.is_empty(), "two of three is not a batch");

        sink.record(allow("usr_3")).await;
        assert_eq!(sink.buffered(), 0, "the batch went out whole");
        assert_eq!(inner.len(), 3);
    }

    /// The memory bound this type promises: the buffer never holds more than
    /// one batch, because the entry that fills it takes it.
    #[tokio::test]
    async fn the_buffer_never_grows_past_one_batch() {
        let inner = Arc::new(MemoryAuditSink::new());
        let sink = BatchingAuditSink::new(Arc::clone(&inner) as Arc<dyn AuditSink>, 4);

        for index in 0..40 {
            sink.record(allow(&format!("usr_{index}"))).await;
            assert!(sink.buffered() < sink.batch_size());
        }
        assert_eq!(inner.len(), 40, "and nothing was lost on the way");
    }

    /// Entries are written once and in the order they were recorded — an audit
    /// trail that reorders itself is one nobody can read a sequence out of.
    #[tokio::test]
    async fn batching_is_lossless_and_keeps_the_order_entries_arrived_in() {
        let inner = Arc::new(MemoryAuditSink::new());
        let sink = BatchingAuditSink::new(Arc::clone(&inner) as Arc<dyn AuditSink>, 3);

        for index in 0..7 {
            sink.record(allow(&format!("usr_{index}"))).await;
        }
        sink.flush().await;

        let written: Vec<String> = inner
            .entries()
            .iter()
            .map(|entry| entry.actor.as_str().to_owned())
            .collect();
        assert_eq!(
            written,
            (0..7)
                .map(|index| format!("usr_{index}"))
                .collect::<Vec<_>>(),
        );
    }

    #[tokio::test]
    async fn a_write_through_sink_is_what_a_batch_size_of_one_means() {
        let inner = Arc::new(MemoryAuditSink::new());
        let sink = BatchingAuditSink::new(Arc::clone(&inner) as Arc<dyn AuditSink>, 1);

        sink.record(deny()).await;
        assert_eq!(inner.len(), 1);
        assert_eq!(sink.buffered(), 0);
    }

    #[tokio::test]
    async fn flushing_an_empty_buffer_writes_nothing_and_reaches_the_inner_sink() {
        let inner = Arc::new(MemoryAuditSink::new());
        let sink = BatchingAuditSink::new(Arc::clone(&inner) as Arc<dyn AuditSink>, 8);

        sink.flush().await;
        assert!(inner.is_empty());
        assert!(format!("{sink:?}").contains("batch_size"));
    }

    /// A quiet system must not hold its entries until shutdown, which is the
    /// whole reason the flusher exists.
    #[tokio::test(start_paused = true)]
    async fn the_timer_writes_a_partial_batch_a_low_traffic_system_would_hold() {
        let config = AuditConfig {
            batch_size: 1_000,
            flush_interval: Duration::from_millis(50),
            ..AuditConfig::default()
        };
        let inner = Arc::new(MemoryAuditSink::new());
        let (sink, guard) =
            BatchingAuditSink::start(Arc::clone(&inner) as Arc<dyn AuditSink>, &config);

        sink.record(deny()).await;
        assert!(inner.is_empty(), "one entry is not a batch of a thousand");

        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(inner.len(), 1, "the timer wrote it anyway");

        guard.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn shutting_the_guard_down_writes_the_last_partial_batch() {
        let config = AuditConfig {
            batch_size: 1_000,
            ..AuditConfig::default()
        };
        let inner = Arc::new(MemoryAuditSink::new());
        let (sink, guard) =
            BatchingAuditSink::start(Arc::clone(&inner) as Arc<dyn AuditSink>, &config);

        sink.record(deny()).await;
        assert_eq!(guard.buffered(), 1);
        assert!(format!("{guard:?}").contains("buffered"));

        guard.shutdown().await;
        assert_eq!(inner.len(), 1);
    }

    /// Dropping a guard outside a multi-threaded runtime cannot await, so it
    /// says what was lost instead of pretending it was written.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dropped_guard_that_cannot_flush_counts_what_it_lost() {
        let config = AuditConfig {
            batch_size: 1_000,
            ..AuditConfig::default()
        };
        let inner = Arc::new(MemoryAuditSink::new());
        let (sink, guard) =
            BatchingAuditSink::start(Arc::clone(&inner) as Arc<dyn AuditSink>, &config);

        sink.record(deny()).await;
        let before = audit_dropped();
        drop(guard);

        assert!(audit_dropped() > before, "the loss is counted");
        assert!(inner.is_empty(), "and not silently claimed as written");
    }

    /// On the runtime an application actually serves on, the drop *does* write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_guard_on_a_multi_threaded_runtime_writes_the_last_batch() {
        let config = AuditConfig {
            batch_size: 1_000,
            ..AuditConfig::default()
        };
        let inner = Arc::new(MemoryAuditSink::new());
        let (sink, guard) =
            BatchingAuditSink::start(Arc::clone(&inner) as Arc<dyn AuditSink>, &config);

        sink.record(deny()).await;
        drop(guard);

        assert_eq!(
            inner.len(),
            1,
            "the last batch was written, not counted lost"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_an_empty_guard_writes_nothing() {
        let inner = Arc::new(MemoryAuditSink::new());
        let (_sink, guard) = BatchingAuditSink::start(
            Arc::clone(&inner) as Arc<dyn AuditSink>,
            &AuditConfig::default(),
        );

        drop(guard);

        assert!(inner.is_empty());
    }

    // ── the shutdown hook ─────────────────────────────────────────────────

    #[tokio::test]
    async fn the_shutdown_hook_flushes_whatever_sink_is_registered() {
        use moso_core::Resolver;

        let inner = Arc::new(MemoryAuditSink::new());
        let sink = Arc::new(BatchingAuditSink::new(
            Arc::clone(&inner) as Arc<dyn AuditSink>,
            1_000,
        ));
        sink.record(deny()).await;
        assert!(inner.is_empty());

        let mut providers = moso_core::di::ProviderMapBuilder::new();
        providers.insert_dyn::<dyn AuditSink>(Arc::clone(&sink) as Arc<dyn AuditSink>);
        flush_audit(&Resolver::new(providers.build())).await;

        assert_eq!(inner.len(), 1);
    }

    /// An application that registered no sink gets the tracing fallback, which
    /// buffers nothing — so the hook must not be an error there.
    #[tokio::test]
    async fn the_shutdown_hook_is_a_no_op_when_nothing_is_registered() {
        use moso_core::{ProviderMap, Resolver};

        flush_audit(&Resolver::new(Arc::new(ProviderMap::new()))).await;
    }
}
