//! Capturing the server's own log lines, per request.
//!
//! # Why this module exists
//!
//! A failing HTTP assertion tells you what came back. It does not tell you what
//! the server was thinking. The difference between a five-second and a
//! fifteen-minute debugging session is whether the failure output contains the
//! `WARN` line the handler emitted just before it returned the 422 you did not
//! expect — so `moso-test` captures the application's `tracing` output and
//! attaches the lines belonging to the failing request to every assertion
//! failure.
//!
//! # How a log line is attributed to a request
//!
//! `tracing` has one global subscriber per process, so a harness cannot install
//! one per [`TestApp`](crate::TestApp). Instead this module installs a single
//! [`Subscriber`] the first time any test app is spawned, and *routes* each
//! event to the right buffer using the span context the event was emitted in:
//!
//! 1. an enclosing span carrying the field `moso_test_app` — written by
//!    [`CaptureLayer`], which wraps the in-process service, and by the span the
//!    harness boots the application inside, so startup logs are attributed too;
//! 2. failing that, an enclosing span carrying `moso_test_request` or
//!    `request_id` — the latter is the field Moso's own trace layer records from
//!    the `x-request-id` header. The harness sets that header on every request
//!    and registers the id before sending, so a log line produced on a
//!    *server task* reached over a real socket still lands in the right buffer.
//!
//! Both mechanisms key on values that are unique per test app, so a hundred
//! test apps running in parallel in one binary do not see each other's logs.
//!
//! # When capture is unavailable
//!
//! If the test binary installed its own global subscriber first —
//! `tracing_subscriber::fmt().init()` in a `#[ctor]`, say — ours cannot be
//! installed. That is not an error: the harness records the fact, every buffer
//! stays empty, and the failure output says so instead of silently omitting the
//! most useful section.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};
// `tracing` re-exports most of `tracing_core`, but not `span::Current`, which
// `Subscriber::current_span` must return.
use tracing_core::span::Current;

/// `tracing`'s verbosity level, re-exported so a test needs one `use`.
pub use tracing::Level;

/// The span field naming the [`TestApp`](crate::TestApp) a log line belongs to.
pub const APP_FIELD: &str = "moso_test_app";

/// The span field naming the request a log line belongs to, written by
/// [`CaptureLayer`].
pub const REQUEST_FIELD: &str = "moso_test_request";

/// The span field Moso's own trace layer records the correlation id under.
pub const MOSO_REQUEST_ID_FIELD: &str = "request_id";

/// How many records one buffer keeps before it starts dropping the oldest.
pub const DEFAULT_LOG_LIMIT: usize = 4096;

/// How many in-flight request ids the harness remembers for attribution.
const CLAIM_LIMIT: usize = 8192;

// ---------------------------------------------------------------------------
// LogRecord
// ---------------------------------------------------------------------------

/// One captured `tracing` event.
#[derive(Clone, Debug)]
pub struct LogRecord {
    /// The level the event was emitted at.
    pub level: Level,
    /// The event's target, usually the emitting module path.
    pub target: String,
    /// The `message` field, or an empty string for an event that has none.
    pub message: String,
    /// Every other field on the event, in declaration order.
    pub fields: Vec<(String, String)>,
    /// The correlation id of the request this line belongs to, when known.
    pub request_id: Option<String>,
    /// The name of the innermost enclosing span.
    pub span: Option<String>,
}

impl LogRecord {
    /// Render the record the way the failure report prints it.
    ///
    /// ```text
    /// WARN  moso::http  rate limit exceeded  remaining=0
    /// ```
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("{:<5} {}", self.level.as_str(), self.target);
        if !self.message.is_empty() {
            let _ = write!(out, "  {}", self.message);
        }
        for (key, value) in &self.fields {
            let _ = write!(out, "  {key}={value}");
        }
        out
    }

    /// Whether `needle` appears in the message, the target or any field value.
    ///
    /// This is what [`LogAssertions::assert_contains`] matches on: an assertion
    /// that only looked at the message would miss `error = "…"`, which is where
    /// `tracing::error!(%error, "…")` puts the interesting half.
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        self.message.contains(needle)
            || self.target.contains(needle)
            || self
                .fields
                .iter()
                .any(|(key, value)| key.contains(needle) || value.contains(needle))
    }
}

// ---------------------------------------------------------------------------
// LogBuffer
// ---------------------------------------------------------------------------

/// The per-application ring of captured records.
#[derive(Debug)]
pub(crate) struct LogBuffer {
    records: Mutex<VecDeque<LogRecord>>,
    limit: usize,
}

impl LogBuffer {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            records: Mutex::new(VecDeque::new()),
            limit: limit.max(1),
        }
    }

    fn push(&self, record: LogRecord) {
        let Ok(mut records) = self.records.lock() else {
            return;
        };
        if records.len() >= self.limit {
            records.pop_front();
        }
        records.push_back(record);
    }

    fn snapshot(&self) -> Vec<LogRecord> {
        self.records
            .lock()
            .map(|records| records.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn clear(&self) {
        if let Ok(mut records) = self.records.lock() {
            records.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// The global registry
// ---------------------------------------------------------------------------

/// Everything the subscriber needs in order to route an event.
#[derive(Debug, Default)]
struct Registry {
    /// One buffer per live test app. `Weak`, so a dropped app's buffer is
    /// collected even if `deregister` never ran.
    buffers: Mutex<HashMap<u64, Weak<LogBuffer>>>,
    /// Which app claimed which request id, for the socket transport where the
    /// harness cannot wrap the future the server polls.
    claims: Mutex<(HashMap<String, u64>, VecDeque<String>)>,
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

/// Allocate the next test-app id. Never reused inside one process.
pub(crate) fn next_app_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn register(app: u64, buffer: &Arc<LogBuffer>) {
    if let Ok(mut buffers) = registry().buffers.lock() {
        buffers.retain(|_, weak| weak.strong_count() > 0);
        buffers.insert(app, Arc::downgrade(buffer));
    }
}

pub(crate) fn deregister(app: u64) {
    if let Ok(mut buffers) = registry().buffers.lock() {
        buffers.remove(&app);
    }
    if let Ok(mut claims) = registry().claims.lock() {
        let (map, order) = &mut *claims;
        map.retain(|_, owner| *owner != app);
        order.retain(|id| map.contains_key(id));
    }
}

/// Record that `request_id` belongs to `app`.
///
/// Called by the client immediately before a request goes out, so that a log
/// line produced on a server task — which the harness cannot instrument — can
/// still be attributed. Bounded: the oldest claim is evicted past
/// [`CLAIM_LIMIT`], which is far more than any single test issues.
pub(crate) fn claim_request(app: u64, request_id: &str) {
    let Ok(mut claims) = registry().claims.lock() else {
        return;
    };
    let (map, order) = &mut *claims;
    if map.insert(request_id.to_owned(), app).is_none() {
        order.push_back(request_id.to_owned());
    }
    while order.len() > CLAIM_LIMIT {
        if let Some(oldest) = order.pop_front() {
            map.remove(&oldest);
        }
    }
}

fn owner_of(request_id: &str) -> Option<u64> {
    registry()
        .claims
        .lock()
        .ok()
        .and_then(|claims| claims.0.get(request_id).copied())
}

fn deliver(app: u64, record: LogRecord) {
    let buffer = registry()
        .buffers
        .lock()
        .ok()
        .and_then(|buffers| buffers.get(&app).and_then(Weak::upgrade));
    if let Some(buffer) = buffer {
        buffer.push(record);
    }
}

// ---------------------------------------------------------------------------
// The subscriber
// ---------------------------------------------------------------------------

thread_local! {
    /// The span stack of the current thread, innermost last.
    static STACK: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

fn current_span_id() -> Option<u64> {
    STACK.with(|stack| stack.borrow().last().copied())
}

/// What the subscriber remembers about one open span.
struct SpanState {
    /// How many handles exist. The span is forgotten when this reaches zero.
    refs: usize,
    /// The callsite, needed by [`Subscriber::current_span`].
    meta: &'static Metadata<'static>,
    /// The span this one was created inside, if any.
    parent: Option<u64>,
    /// Its fields, including the ones recorded after creation.
    fields: Vec<(String, String)>,
}

/// A minimal `tracing` subscriber that routes events into per-app buffers.
///
/// Hand-written rather than built on `tracing-subscriber` because the harness
/// needs exactly one behaviour — "put this event in that `Vec`" — and a
/// registry, a filter stack and a formatter would be five dependencies bought
/// to throw away.
struct CaptureSubscriber {
    next: AtomicU64,
    spans: Mutex<HashMap<u64, SpanState>>,
}

impl CaptureSubscriber {
    fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            spans: Mutex::new(HashMap::new()),
        }
    }

    /// The id an event or span should hang off: an explicit parent, else the
    /// innermost open span, else nothing.
    fn contextual_parent(explicit: Option<&Id>, is_root: bool, is_contextual: bool) -> Option<u64> {
        if is_root {
            return None;
        }
        if let Some(id) = explicit {
            return Some(id.into_u64());
        }
        if is_contextual {
            return current_span_id();
        }
        None
    }
}

impl Subscriber for CaptureSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        // Everything: the harness cannot know in advance which target the
        // application logs the interesting line under.
        true
    }

    fn new_span(&self, attributes: &Attributes<'_>) -> Id {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut visitor = FieldVisitor::default();
        attributes.record(&mut visitor);
        let parent = Self::contextual_parent(
            attributes.parent(),
            attributes.is_root(),
            attributes.is_contextual(),
        );
        let state = SpanState {
            refs: 1,
            meta: attributes.metadata(),
            parent,
            fields: visitor.into_fields(),
        };
        if let Ok(mut spans) = self.spans.lock() {
            spans.insert(id, state);
        }
        Id::from_u64(id)
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        let mut visitor = FieldVisitor::default();
        values.record(&mut visitor);
        let recorded = visitor.into_fields();
        if let Ok(mut spans) = self.spans.lock()
            && let Some(state) = spans.get_mut(&span.into_u64())
        {
            for (key, value) in recorded {
                match state.fields.iter_mut().find(|(name, _)| *name == key) {
                    Some(slot) => slot.1 = value,
                    None => state.fields.push((key, value)),
                }
            }
        }
    }

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let mut cursor =
            Self::contextual_parent(event.parent(), event.is_root(), event.is_contextual());

        let mut app = None;
        // The harness's own field and Moso's are tracked separately, and the
        // harness's wins. They normally agree — the harness issues an id the
        // request-id middleware adopts — but an application that rewrites the id
        // mid-stack would otherwise file its log lines under a key the client
        // never saw, which is exactly the silent failure this harness exists to
        // prevent.
        let mut harness_request = None;
        let mut moso_request = None;
        let mut span_name = None;

        if let Ok(spans) = self.spans.lock() {
            // Innermost first, so the nearest `moso_test_app` wins — which
            // matters when a test app's service is nested inside another's.
            let mut hops = 0usize;
            while let Some(id) = cursor {
                let Some(state) = spans.get(&id) else { break };
                if span_name.is_none() {
                    span_name = Some(state.meta.name().to_owned());
                }
                for (key, value) in &state.fields {
                    if value.is_empty() {
                        continue;
                    }
                    if app.is_none() && key == APP_FIELD {
                        app = value.parse::<u64>().ok();
                    }
                    if harness_request.is_none() && key == REQUEST_FIELD {
                        harness_request = Some(value.clone());
                    }
                    if moso_request.is_none() && key == MOSO_REQUEST_ID_FIELD {
                        moso_request = Some(value.clone());
                    }
                }
                cursor = state.parent;
                hops += 1;
                // A malformed parent chain must not wedge a test run.
                if hops > 256 {
                    break;
                }
            }
        }

        let request = harness_request.or(moso_request);
        let app = match app.or_else(|| request.as_deref().and_then(owner_of)) {
            Some(app) => app,
            // Not ours: an event from the test binary itself, or from a crate
            // that logs outside any request. Dropping it keeps the buffers
            // scoped to the application under test.
            None => return,
        };

        let (message, fields) = visitor.split();
        deliver(
            app,
            LogRecord {
                level: *event.metadata().level(),
                target: event.metadata().target().to_owned(),
                message,
                fields,
                request_id: request,
                span: span_name,
            },
        );
    }

    fn enter(&self, span: &Id) {
        STACK.with(|stack| stack.borrow_mut().push(span.into_u64()));
    }

    fn exit(&self, span: &Id) {
        STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            let wanted = span.into_u64();
            if stack.last() == Some(&wanted) {
                stack.pop();
            } else if let Some(index) = stack.iter().rposition(|id| *id == wanted) {
                stack.remove(index);
            }
        });
    }

    fn clone_span(&self, span: &Id) -> Id {
        if let Ok(mut spans) = self.spans.lock()
            && let Some(state) = spans.get_mut(&span.into_u64())
        {
            state.refs += 1;
        }
        span.clone()
    }

    fn try_close(&self, span: Id) -> bool {
        let Ok(mut spans) = self.spans.lock() else {
            return false;
        };
        let id = span.into_u64();
        let Some(state) = spans.get_mut(&id) else {
            return false;
        };
        state.refs = state.refs.saturating_sub(1);
        if state.refs == 0 {
            spans.remove(&id);
            return true;
        }
        false
    }

    fn current_span(&self) -> Current {
        let Some(id) = current_span_id() else {
            return Current::none();
        };
        match self.spans.lock() {
            Ok(spans) => match spans.get(&id) {
                Some(state) => Current::new(Id::from_u64(id), state.meta),
                None => Current::none(),
            },
            Err(_) => Current::none(),
        }
    }
}

/// Turns `tracing`'s typed field values into strings.
#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl FieldVisitor {
    fn put(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = Some(value);
        } else {
            self.fields.push((field.name().to_owned(), value));
        }
    }

    /// Every field, with the message folded back in under its own name.
    fn into_fields(mut self) -> Vec<(String, String)> {
        if let Some(message) = self.message.take() {
            self.fields.push(("message".to_owned(), message));
        }
        self.fields
    }

    fn split(self) -> (String, Vec<(String, String)>) {
        (self.message.unwrap_or_default(), self.fields)
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        // Without this the `Debug` path would quote and escape every string,
        // and `message` would render as `"…"` inside the report.
        self.put(field, value.to_owned());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        self.put(field, format!("{value:?}"));
    }
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// Install the capturing subscriber, once per process.
///
/// Returns whether capture is available: `false` means some other global
/// subscriber was installed first, and every buffer will stay empty.
pub fn install() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED
        .get_or_init(|| tracing::subscriber::set_global_default(CaptureSubscriber::new()).is_ok())
}

// ---------------------------------------------------------------------------
// The tower layer that names the app and the request
// ---------------------------------------------------------------------------

/// Wraps the in-process service in a span carrying the test app's id.
///
/// Sits *outside* Moso's own middleware stack, so it attributes every line the
/// stack itself emits — including the access log and the error renderer, which
/// are exactly the lines a failing assertion wants to show.
#[derive(Clone, Copy, Debug)]
pub struct CaptureLayer {
    app: u64,
}

impl CaptureLayer {
    /// Attribute everything under this service to test app `app`.
    #[must_use]
    pub fn new(app: u64) -> Self {
        Self { app }
    }
}

impl<S> tower::Layer<S> for CaptureLayer {
    type Service = CaptureService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CaptureService {
            inner,
            app: self.app,
        }
    }
}

/// The service [`CaptureLayer`] produces.
#[derive(Clone)]
pub struct CaptureService<S> {
    inner: S,
    app: u64,
}

impl<S> core::fmt::Debug for CaptureService<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CaptureService")
            .field("app", &self.app)
            .finish_non_exhaustive()
    }
}

impl<S, B> tower::Service<http::Request<B>> for CaptureService<S>
where
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = tracing::instrument::Instrumented<S::Future>;

    fn poll_ready(
        &mut self,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let request_id = request
            .headers()
            .get(moso::REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let span = tracing::info_span!(
            target: "moso_test",
            "moso_test::request",
            moso_test_app = self.app,
            moso_test_request = %request_id,
        );
        // Entered around `call` as well, because a `tower` service is allowed
        // to do work — and to log — before it returns its future.
        let future = {
            let _entered = span.enter();
            self.inner.call(request)
        };
        tracing::Instrument::instrument(future, span)
    }
}

// ---------------------------------------------------------------------------
// LogAssertions
// ---------------------------------------------------------------------------

/// Assertions over the log lines one [`TestApp`](crate::TestApp) produced.
///
/// ```
/// use moso_test::prelude::*;
/// # /// A user, as the API accepts one.
/// # #[derive(moso::Schema)] pub struct CreateUser {
/// #     /// Public handle.
/// #     #[schema(len = 3..=32)] pub username: String,
/// #     /// Contact address.
/// #     pub email: moso::schema::Email }
/// # /// A user, as the API returns one.
/// # #[derive(moso::Schema)] pub struct UserOut {
/// #     /// Stable identifier.
/// #     pub id: u64,
/// #     /// Public handle.
/// #     pub username: String }
/// # /// Everything this application reads from its environment.
/// # #[derive(moso::Config, Clone, Debug)] pub struct AppConfig {
/// #     /// Service name.
/// #     #[config(default = "users")] pub name: String }
/// # /// Create a user.
/// # #[moso::endpoint]
/// # async fn create(moso::extract::Json(body): moso::extract::Json<CreateUser>)
/// #     -> moso::Result<moso::response::Created<UserOut>>
/// # {
/// #     Ok(moso::response::Created::at(
/// #         "/users/1",
/// #         UserOut { id: 1, username: body.username },
/// #     ))
/// # }
/// # /// The composition root every Moso application exposes.
/// # fn app() -> moso::AppBuilder {
/// #     moso::App::new(AppConfig { name: "users".to_owned() })
/// #         .mount(moso::routes! { POST "/users" => create })
/// # }
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso::Result<()> {
/// # let app = TestApp::builder().app(app()).spawn().await?;
/// app.client().post("/users")
///     .json(&serde_json::json!({ "username": "ada", "email": "a@b.example" }))
///     .send().await;
///
/// app.logs().assert_no_errors();
/// # Ok(())
/// # }
/// ```
///
/// Every assertion prints the whole captured buffer on failure, because "the
/// line you expected is not there" is only actionable next to the lines that
/// are.
#[derive(Clone, Debug)]
pub struct LogAssertions {
    buffer: Arc<LogBuffer>,
    capturing: bool,
}

impl LogAssertions {
    pub(crate) fn new(buffer: Arc<LogBuffer>, capturing: bool) -> Self {
        Self { buffer, capturing }
    }

    /// Whether log capture is actually running.
    ///
    /// `false` when another global `tracing` subscriber was installed before
    /// the first [`TestApp`](crate::TestApp) was spawned.
    #[must_use]
    pub fn is_capturing(&self) -> bool {
        self.capturing
    }

    /// Every record captured so far, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<LogRecord> {
        self.buffer.snapshot()
    }

    /// The records emitted while serving the request with this correlation id.
    #[must_use]
    pub fn for_request(&self, request_id: &str) -> Vec<LogRecord> {
        self.buffer
            .snapshot()
            .into_iter()
            .filter(|record| record.request_id.as_deref() == Some(request_id))
            .collect()
    }

    /// How many records are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buffer
            .records
            .lock()
            .map(|records| records.len())
            .unwrap_or(0)
    }

    /// Whether nothing was captured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forget everything captured so far.
    ///
    /// Useful between the arrange and act phases of a long test, so
    /// [`assert_no_errors`](Self::assert_no_errors) only judges the part under
    /// test.
    pub fn clear(&self) {
        self.buffer.clear();
    }

    /// The whole buffer, rendered the way the failure report prints it.
    #[must_use]
    pub fn dump(&self) -> String {
        render_records(&self.records())
    }

    /// Assert that some record at exactly `level` contains `needle`.
    ///
    /// The level is matched exactly rather than "at least", because a test that
    /// means "somebody warned about this" should not be satisfied by an `ERROR`
    /// that happens to share a word. Use
    /// [`assert_contains_at_least`](Self::assert_contains_at_least) for the
    /// other reading.
    pub fn assert_contains(&self, level: Level, needle: &str) -> &Self {
        if self
            .records()
            .iter()
            .any(|record| record.level == level && record.contains(needle))
        {
            return self;
        }
        self.fail(&format!(
            "expected a {level} log line containing {needle:?}"
        ));
    }

    /// Assert that some record at `level` **or more severe** contains `needle`.
    pub fn assert_contains_at_least(&self, level: Level, needle: &str) -> &Self {
        if self
            .records()
            .iter()
            .any(|record| record.level <= level && record.contains(needle))
        {
            return self;
        }
        self.fail(&format!(
            "expected a log line at {level} or more severe containing {needle:?}"
        ));
    }

    /// Assert that **no** record at `level` contains `needle`.
    pub fn assert_none_containing(&self, level: Level, needle: &str) -> &Self {
        if !self
            .records()
            .iter()
            .any(|record| record.level == level && record.contains(needle))
        {
            return self;
        }
        self.fail(&format!(
            "expected no {level} log line containing {needle:?}, but one was emitted"
        ));
    }

    /// Assert that the application logged nothing at `ERROR`.
    ///
    /// A good last line for every test: an endpoint that returns the right
    /// status while logging a stack trace is still broken.
    pub fn assert_no_errors(&self) -> &Self {
        let errors: Vec<LogRecord> = self
            .records()
            .into_iter()
            .filter(|record| record.level == Level::ERROR)
            .collect();
        if errors.is_empty() {
            return self;
        }
        self.fail(&format!(
            "expected no ERROR log lines, found {}",
            errors.len()
        ));
    }

    /// Panic with the assertion and the whole buffer underneath it.
    fn fail(&self, headline: &str) -> ! {
        let mut out = String::new();
        out.push_str(&crate::report::rule("moso-test: log assertion failed"));
        let _ = writeln!(out, "  {headline}");
        out.push('\n');
        if self.capturing {
            out.push_str(&crate::report::section(
                &format!("captured log lines ({})", self.len()),
                &self.dump(),
            ));
        } else {
            out.push_str(&crate::report::section(
                "captured log lines",
                "log capture is unavailable: another global `tracing` subscriber\n\
                 was installed before the first TestApp was spawned.",
            ));
        }
        out.push_str(&crate::report::rule_end());
        panic!("{out}");
    }
}

/// Render a slice of records, one per line, or say that there were none.
pub(crate) fn render_records(records: &[LogRecord]) -> String {
    if records.is_empty() {
        return "(none)".to_owned();
    }
    records
        .iter()
        .map(LogRecord::render)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(level: Level, message: &str) -> LogRecord {
        LogRecord {
            level,
            target: "moso::http".to_owned(),
            message: message.to_owned(),
            fields: Vec::new(),
            request_id: None,
            span: None,
        }
    }

    #[test]
    fn a_record_matches_on_message_target_and_fields() {
        let mut rec = record(Level::WARN, "rate limit exceeded");
        rec.fields.push(("remaining".to_owned(), "0".to_owned()));
        assert!(rec.contains("rate limit"));
        assert!(rec.contains("moso::http"));
        assert!(rec.contains("remaining"));
        assert!(!rec.contains("nowhere"));
    }

    #[test]
    fn rendering_puts_the_level_first_and_the_fields_last() {
        let mut rec = record(Level::INFO, "served");
        rec.fields.push(("status".to_owned(), "200".to_owned()));
        assert_eq!(rec.render(), "INFO  moso::http  served  status=200");
    }

    #[test]
    fn the_buffer_drops_the_oldest_record_past_its_limit() {
        let buffer = LogBuffer::new(2);
        buffer.push(record(Level::INFO, "one"));
        buffer.push(record(Level::INFO, "two"));
        buffer.push(record(Level::INFO, "three"));
        let kept: Vec<String> = buffer
            .snapshot()
            .into_iter()
            .map(|record| record.message)
            .collect();
        assert_eq!(kept, ["two", "three"]);
    }

    #[test]
    fn a_claim_is_evicted_when_its_app_goes_away() {
        let app = next_app_id();
        claim_request(app, "01J8-test-claim");
        assert_eq!(owner_of("01J8-test-claim"), Some(app));
        deregister(app);
        assert_eq!(owner_of("01J8-test-claim"), None);
    }

    #[test]
    fn delivery_reaches_the_registered_buffer_and_only_that_one() {
        let mine = Arc::new(LogBuffer::new(8));
        let theirs = Arc::new(LogBuffer::new(8));
        let a = next_app_id();
        let b = next_app_id();
        register(a, &mine);
        register(b, &theirs);

        deliver(a, record(Level::INFO, "for a"));

        assert_eq!(mine.snapshot().len(), 1);
        assert_eq!(theirs.snapshot().len(), 0);

        deregister(a);
        deregister(b);
    }

    #[test]
    fn a_dropped_buffer_is_pruned_rather_than_leaked() {
        let app = next_app_id();
        {
            let buffer = Arc::new(LogBuffer::new(8));
            register(app, &buffer);
        }
        // The `Weak` is dead; delivering must not panic and must not resurrect.
        deliver(app, record(Level::INFO, "orphan"));
        deregister(app);
    }

    #[test]
    fn the_field_visitor_keeps_message_separate_from_the_rest() {
        let visitor = FieldVisitor {
            message: Some("served".to_owned()),
            fields: vec![("status".to_owned(), "200".to_owned())],
        };
        let (message, fields) = visitor.split();
        assert_eq!(message, "served");
        assert_eq!(fields, [("status".to_owned(), "200".to_owned())]);
    }

    #[test]
    fn into_fields_folds_the_message_back_in() {
        let visitor = FieldVisitor {
            message: Some("m".to_owned()),
            fields: vec![("a".to_owned(), "1".to_owned())],
        };
        let fields = visitor.into_fields();
        assert_eq!(fields.len(), 2);
        assert!(fields.contains(&("message".to_owned(), "m".to_owned())));
    }

    #[test]
    fn rendering_an_empty_buffer_says_so() {
        assert_eq!(render_records(&[]), "(none)");
    }

    #[test]
    fn the_span_stack_pops_out_of_order_entries() {
        let subscriber = CaptureSubscriber::new();
        subscriber.enter(&Id::from_u64(1));
        subscriber.enter(&Id::from_u64(2));
        subscriber.exit(&Id::from_u64(1));
        assert_eq!(current_span_id(), Some(2));
        subscriber.exit(&Id::from_u64(2));
        assert_eq!(current_span_id(), None);
    }
}
