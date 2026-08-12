//! W3C trace context, carried from the enqueueing request into the job.
//!
//! One distributed trace spanning `HTTP request → job → outbound call` is a
//! genuinely rare capability, and it is what makes an async workflow debuggable
//! at all: without it, a failed welcome email and the signup that caused it are
//! two unrelated log lines an hour apart.
//!
//! The mechanism is deliberately small. A [`TraceContext`] is the 55 bytes of a
//! W3C `traceparent` header; [`scope`] puts one in a task-local for the duration
//! of a future; [`EnqueueBuilder`](crate::EnqueueBuilder) reads it and writes it
//! onto the queue row; and the worker reads it back, makes a **child** of it,
//! and runs the job inside that. Three hops, one trace id, each hop naming its
//! parent.
//!
//! # Why not OpenTelemetry
//!
//! `opentelemetry-otlp` is not a dependency of this workspace
//! (`docs/04-devex/44-observability.md`), and adding an exporter to the jobs
//! crate would decide the question for every other crate. What this module owns
//! is the *propagation*: the identifiers, the header format, and the parentage.
//! An application that wires an exporter reads [`current`] and attaches it; one
//! that does not still gets `trace_id` on every job's span, which is enough to
//! join the two log lines.

use std::fmt::Write as _;

tokio::task_local! {
    /// The trace context of whatever is running on this task.
    ///
    /// A task-local rather than a thread-local: a worker runs many jobs
    /// concurrently on one runtime thread, and a thread-local would leak one
    /// job's trace into the next one's outbound call.
    static CURRENT: TraceContext;
}

/// One hop of a distributed trace.
///
/// The wire form is the W3C `traceparent` header:
/// `00-<32 hex trace id>-<16 hex span id>-<2 hex flags>`.
///
/// ```
/// use moso_jobs::trace::TraceContext;
///
/// let root = TraceContext::root();
/// let header = root.to_traceparent();
/// assert_eq!(header.len(), 55);
/// assert_eq!(TraceContext::parse(&header).unwrap().trace_id(), root.trace_id());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceContext {
    /// The whole trace. Constant across every hop.
    trace_id: [u8; 16],
    /// This hop.
    span_id: [u8; 8],
    /// The hop that caused this one, when there was one.
    parent_span_id: Option<[u8; 8]>,
    /// The W3C flags byte. Bit 0 is "sampled".
    flags: u8,
}

/// The `sampled` bit of the flags byte.
const SAMPLED: u8 = 0x01;

impl TraceContext {
    /// A brand-new trace, sampled.
    ///
    /// ```
    /// use moso_jobs::trace::TraceContext;
    ///
    /// let root = TraceContext::root();
    /// assert!(root.is_sampled());
    /// assert!(root.parent_span_id_hex().is_none());
    /// ```
    #[must_use]
    pub fn root() -> Self {
        Self {
            trace_id: random_16(),
            span_id: random_8(),
            parent_span_id: None,
            flags: SAMPLED,
        }
    }

    /// The next hop: same trace, new span, this one as the parent.
    ///
    /// ```
    /// use moso_jobs::trace::TraceContext;
    ///
    /// let request = TraceContext::root();
    /// let job = request.child();
    ///
    /// assert_eq!(job.trace_id(), request.trace_id(), "one trace");
    /// assert_eq!(job.parent_span_id_hex().as_deref(), Some(request.span_id_hex().as_str()));
    /// assert_ne!(job.span_id_hex(), request.span_id_hex());
    /// ```
    #[must_use]
    pub fn child(&self) -> Self {
        Self {
            trace_id: self.trace_id,
            span_id: random_8(),
            parent_span_id: Some(self.span_id),
            flags: self.flags,
        }
    }

    /// Parse a `traceparent` header value.
    ///
    /// Only version `00` is accepted, and an all-zero identifier is rejected —
    /// both as the specification requires, because a zero trace id joins every
    /// unrelated request into one trace.
    ///
    /// Returns `None` rather than an error: a malformed header from an upstream
    /// is not this application's problem to report, and starting a fresh trace
    /// loses less than dropping the request.
    ///
    /// ```
    /// use moso_jobs::trace::TraceContext;
    ///
    /// let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    /// let parsed = TraceContext::parse(header).expect("a valid header");
    /// assert_eq!(parsed.trace_id(), "4bf92f3577b34da6a3ce929d0e0e4736");
    /// assert!(TraceContext::parse("garbage").is_none());
    /// assert!(TraceContext::parse("00-00000000000000000000000000000000-00f067aa0ba902b7-01").is_none());
    /// ```
    #[must_use]
    pub fn parse(header: &str) -> Option<Self> {
        let mut fields = header.trim().split('-');
        let version = fields.next()?;
        if version != "00" {
            return None;
        }
        let trace_id = hex_16(fields.next()?)?;
        let span_id = hex_8(fields.next()?)?;
        let flags = u8::from_str_radix(fields.next()?, 16).ok()?;
        if fields.next().is_some() {
            return None;
        }
        if trace_id == [0; 16] || span_id == [0; 8] {
            return None;
        }
        Some(Self {
            trace_id,
            span_id,
            parent_span_id: None,
            flags,
        })
    }

    /// The `traceparent` header value for this hop.
    ///
    /// ```
    /// use moso_jobs::trace::TraceContext;
    ///
    /// assert!(TraceContext::root().to_traceparent().starts_with("00-"));
    /// ```
    #[must_use]
    pub fn to_traceparent(&self) -> String {
        let mut out = String::with_capacity(55);
        out.push_str("00-");
        write_hex(&mut out, &self.trace_id);
        out.push('-');
        write_hex(&mut out, &self.span_id);
        out.push('-');
        let _ = write!(out, "{:02x}", self.flags);
        out
    }

    /// The trace id, lowercase hex.
    ///
    /// ```
    /// # use moso_jobs::trace::TraceContext;
    /// assert_eq!(TraceContext::root().trace_id().len(), 32);
    /// ```
    #[must_use]
    pub fn trace_id(&self) -> String {
        let mut out = String::with_capacity(32);
        write_hex(&mut out, &self.trace_id);
        out
    }

    /// This hop's span id, lowercase hex.
    ///
    /// ```
    /// # use moso_jobs::trace::TraceContext;
    /// assert_eq!(TraceContext::root().span_id_hex().len(), 16);
    /// ```
    #[must_use]
    pub fn span_id_hex(&self) -> String {
        let mut out = String::with_capacity(16);
        write_hex(&mut out, &self.span_id);
        out
    }

    /// The parent hop's span id, when this hop has one.
    ///
    /// ```
    /// # use moso_jobs::trace::TraceContext;
    /// assert!(TraceContext::root().parent_span_id_hex().is_none());
    /// assert!(TraceContext::root().child().parent_span_id_hex().is_some());
    /// ```
    #[must_use]
    pub fn parent_span_id_hex(&self) -> Option<String> {
        self.parent_span_id.map(|id| {
            let mut out = String::with_capacity(16);
            write_hex(&mut out, &id);
            out
        })
    }

    /// Whether the trace is sampled.
    ///
    /// ```
    /// # use moso_jobs::trace::TraceContext;
    /// assert!(TraceContext::root().is_sampled());
    /// ```
    #[must_use]
    pub const fn is_sampled(&self) -> bool {
        self.flags & SAMPLED != 0
    }
}

/// The trace context of whatever is running on this task.
///
/// ```
/// use moso_jobs::trace::{self, TraceContext};
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// assert!(trace::current().is_none());
///
/// let request = TraceContext::root();
/// let inside = trace::scope(request, async { trace::current() }).await;
/// assert_eq!(inside, Some(request));
/// # }
/// ```
#[must_use]
pub fn current() -> Option<TraceContext> {
    CURRENT.try_with(|context| *context).ok()
}

/// The current context as a `traceparent` header value.
///
/// What [`EnqueueBuilder`](crate::EnqueueBuilder) writes onto the row, and what
/// an outbound HTTP call should send.
///
/// ```
/// use moso_jobs::trace::{self, TraceContext};
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let header = trace::scope(TraceContext::root(), async {
///     trace::current_traceparent()
/// })
/// .await;
/// assert!(header.unwrap().starts_with("00-"));
/// # }
/// ```
#[must_use]
pub fn current_traceparent() -> Option<String> {
    current().map(|context| context.to_traceparent())
}

/// Run `future` with `context` as the current trace context.
///
/// The one seam an application needs: wrap the request handler (or the whole
/// service) in this and every enqueue underneath carries the trace onto its
/// row. The worker does the same on the way out.
///
/// ```
/// use moso_jobs::trace::{self, TraceContext};
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let seen = trace::scope(TraceContext::root(), async { trace::current().is_some() }).await;
/// assert!(seen);
/// # }
/// ```
pub async fn scope<F: Future>(context: TraceContext, future: F) -> F::Output {
    CURRENT.scope(context, future).await
}

/// The context a job should run under, given what its row carried.
///
/// A row with a `traceparent` produces a child of it — same trace, new span,
/// the enqueueing hop as the parent. A row with none, or with a malformed one,
/// starts a fresh trace, because a job with no trace at all is harder to debug
/// than one whose trace begins late.
pub(crate) fn context_for_job(traceparent: Option<&str>) -> TraceContext {
    traceparent
        .and_then(TraceContext::parse)
        .map_or_else(TraceContext::root, |parent| parent.child())
}

/// Sixteen random bytes, never all zero.
fn random_16() -> [u8; 16] {
    let mut bytes = uuid::Uuid::new_v4().into_bytes();
    if bytes == [0; 16] {
        bytes[0] = 1;
    }
    bytes
}

/// Eight random bytes, never all zero.
fn random_8() -> [u8; 8] {
    let source = uuid::Uuid::new_v4().into_bytes();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&source[..8]);
    if bytes == [0; 8] {
        bytes[0] = 1;
    }
    bytes
}

/// Append `bytes` as lowercase hex.
fn write_hex(out: &mut String, bytes: &[u8]) {
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
}

/// Parse exactly 32 hex characters.
fn hex_16(text: &str) -> Option<[u8; 16]> {
    let mut bytes = [0_u8; 16];
    parse_hex(text, &mut bytes)?;
    Some(bytes)
}

/// Parse exactly 16 hex characters.
fn hex_8(text: &str) -> Option<[u8; 8]> {
    let mut bytes = [0_u8; 8];
    parse_hex(text, &mut bytes)?;
    Some(bytes)
}

/// Fill `out` from `text`, or fail if the length or the alphabet is wrong.
fn parse_hex(text: &str, out: &mut [u8]) -> Option<()> {
    if text.len() != out.len() * 2 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // The specification says lowercase; an uppercase header is a bug upstream
    // and accepting it silently makes two spellings of one trace id.
    if text.bytes().any(|b| b.is_ascii_uppercase()) {
        return None;
    }
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header round-trips byte for byte, which is what makes it usable as
    /// an outbound header without re-encoding.
    #[test]
    fn a_header_round_trips() {
        let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let parsed = TraceContext::parse(header).expect("valid");
        assert_eq!(parsed.to_traceparent(), header);
        assert_eq!(parsed.trace_id(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(parsed.span_id_hex(), "00f067aa0ba902b7");
        assert!(parsed.is_sampled());
    }

    /// Every way a header can be wrong, since a bad one that parses joins
    /// unrelated requests into one trace.
    #[test]
    fn a_malformed_header_is_refused_rather_than_guessed_at() {
        for bad in [
            "",
            "garbage",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-00f067aa0ba902b7-01",
        ] {
            assert!(TraceContext::parse(bad).is_none(), "accepted `{bad}`");
        }
    }

    /// Acceptance criterion 7, in miniature: three hops, one trace, each
    /// naming its parent.
    #[test]
    fn three_hops_share_a_trace_and_chain_their_parents() {
        let request = TraceContext::root();
        let job = context_for_job(Some(&request.to_traceparent()));
        let outbound = job.child();

        assert_eq!(job.trace_id(), request.trace_id());
        assert_eq!(outbound.trace_id(), request.trace_id());

        assert_eq!(
            job.parent_span_id_hex().as_deref(),
            Some(request.span_id_hex().as_str())
        );
        assert_eq!(
            outbound.parent_span_id_hex().as_deref(),
            Some(job.span_id_hex().as_str())
        );

        let spans = [
            request.span_id_hex(),
            job.span_id_hex(),
            outbound.span_id_hex(),
        ];
        let unique: std::collections::BTreeSet<&String> = spans.iter().collect();
        assert_eq!(unique.len(), 3, "each hop needs its own span id");
    }

    /// A row with no trace context still gets a trace, because a job with none
    /// is harder to debug than one whose trace starts late.
    #[test]
    fn a_row_without_a_context_starts_a_fresh_trace() {
        let fresh = context_for_job(None);
        assert!(fresh.parent_span_id_hex().is_none());

        let salvaged = context_for_job(Some("not a header"));
        assert!(salvaged.parent_span_id_hex().is_none());
        assert_ne!(salvaged.trace_id(), fresh.trace_id());
    }

    /// The sampling decision travels with the trace: a job enqueued by an
    /// unsampled request must not sample itself back in.
    #[test]
    fn the_sampling_decision_is_inherited() {
        let unsampled =
            TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00")
                .expect("valid");
        assert!(!unsampled.is_sampled());
        assert!(!unsampled.child().is_sampled());
    }

    /// A task-local and not a thread-local: two concurrent jobs on one runtime
    /// thread must not see each other's trace.
    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_tasks_do_not_share_a_context() {
        let first = TraceContext::root();
        let second = TraceContext::root();

        let (a, b) = tokio::join!(
            scope(first, async {
                tokio::task::yield_now().await;
                current()
            }),
            scope(second, async {
                tokio::task::yield_now().await;
                current()
            })
        );

        assert_eq!(a, Some(first));
        assert_eq!(b, Some(second));
        assert!(current().is_none(), "the scope does not leak out");
    }
}
