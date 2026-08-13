//! Shared scaffolding for the facade's end-to-end tests.
//!
//! Three things every one of these files needs and none of them should own:
//!
//! 1. a way to drive a built application without binding a socket,
//! 2. a way to see what the application *logged* while it did so, and
//! 3. a way to walk a generated OpenAPI document looking for the two defects
//!    that make a document useless to a client generator.
//!
//! Nothing here depends on anything outside `moso` and its own dev-dependencies:
//! the log capture is a hand-written `tracing::Subscriber` rather than
//! `tracing-subscriber`, because the point of these tests is that the shipped
//! crates are enough.

#![allow(dead_code, reason = "each test file uses a different subset")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use moso::deps::tracing;
use tower::ServiceExt as _;

// ---------------------------------------------------------------------------
// Driving a request
// ---------------------------------------------------------------------------

/// Everything a test wants to look at after one request.
#[derive(Debug, Clone)]
pub struct Reply {
    /// The status code, as a number, because that is what assertions read.
    pub status: u16,
    /// Every response header.
    pub headers: axum::http::HeaderMap,
    /// The body, lossily decoded — these tests never assert on binary.
    pub body: String,
    /// The body as raw bytes, for the compression assertions.
    pub bytes: Vec<u8>,
}

impl Reply {
    /// The body parsed as JSON. Panics with the body when it is not JSON, which
    /// is the failure a test actually wants to read.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("expected JSON, got {error}: {}", self.body))
    }

    /// One header as a string, or `""` when it is absent or not ASCII.
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }

    /// Whether the response carries `name`.
    pub fn has_header(&self, name: &str) -> bool {
        self.headers.contains_key(name)
    }
}

/// A `GET` request, with no body.
pub fn get(path: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .expect("a well-formed GET")
}

/// A `POST` with a JSON body and the matching content type.
pub fn post_json(path: &str, body: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_owned()))
        .expect("a well-formed POST")
}

/// Drive one request through a composed service and collect the whole reply.
///
/// `oneshot` consumes the service, so callers hand over a clone; every test in
/// this suite builds its application once per request, which is cheap and keeps
/// the request-scoped state genuinely fresh.
pub async fn send(
    service: axum::Router<()>,
    request: axum::http::Request<axum::body::Body>,
) -> Reply {
    let response = service
        .oneshot(request)
        .await
        .expect("the stack is infallible");
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 8 << 20)
        .await
        .expect("the body fits in 8 MiB")
        .to_vec();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    Reply {
        status,
        headers,
        body,
        bytes,
    }
}

// ---------------------------------------------------------------------------
// Log capture
// ---------------------------------------------------------------------------

/// A `tracing` subscriber that keeps every event and span it is told about.
///
/// Installed per-test with [`LogCapture::install`], which returns a guard: the
/// dispatcher is thread-local, and `#[tokio::test]` drives its tasks on the
/// thread that installed it, so everything the stack logs while serving a
/// request lands here.
#[derive(Clone, Default)]
pub struct LogCapture {
    lines: Arc<Mutex<Vec<String>>>,
}

impl LogCapture {
    /// A fresh, empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Make this buffer the default subscriber until the guard is dropped.
    pub fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(CaptureSubscriber {
            lines: Arc::clone(&self.lines),
            next: AtomicU64::new(1),
        })
    }

    /// Every line recorded so far, in order.
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().expect("not poisoned").clone()
    }

    /// Every line, newline-joined — the shape a `contains` assertion wants.
    pub fn joined(&self) -> String {
        self.lines().join("\n")
    }

    /// Whether any line contains `needle`.
    pub fn contains(&self, needle: &str) -> bool {
        self.lines().iter().any(|line| line.contains(needle))
    }
}

/// The subscriber [`LogCapture::install`] registers.
struct CaptureSubscriber {
    lines: Arc<Mutex<Vec<String>>>,
    next: AtomicU64,
}

impl CaptureSubscriber {
    fn push(&self, line: String) {
        if let Ok(mut lines) = self.lines.lock() {
            lines.push(line);
        }
    }
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn register_callsite(
        &self,
        _metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        // `sometimes`, not `always`: the interest cache is global and these
        // subscribers are per-test, so a cached "always" from one test must not
        // decide anything for the next one.
        tracing::subscriber::Interest::sometimes()
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut visitor = Recorder::default();
        span.record(&mut visitor);
        self.push(format!(
            "SPAN {} target={} {}",
            span.metadata().name(),
            span.metadata().target(),
            visitor.0
        ));
        tracing::span::Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut visitor = Recorder::default();
        values.record(&mut visitor);
        self.push(format!("RECORD span={} {}", span.into_u64(), visitor.0));
    }

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = Recorder::default();
        event.record(&mut visitor);
        self.push(format!(
            "{} {} {}",
            event.metadata().level(),
            event.metadata().target(),
            visitor.0
        ));
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Folds every field of an event or span into one string.
///
/// `Debug` for every value, deliberately: the canary test is asking whether a
/// secret can reach a log by *any* formatting route, and `Debug` is the one a
/// careless `?field` sigil would take.
#[derive(Default)]
struct Recorder(String);

impl tracing::field::Visit for Recorder {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn core::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(&format!("{}={:?}", field.name(), value));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(&format!("{}={}", field.name(), value));
    }
}

// ---------------------------------------------------------------------------
// OpenAPI document checks
// ---------------------------------------------------------------------------

/// Every `$ref` in `document` that does not resolve, with the pointer at which
/// it was found.
///
/// A dangling `$ref` is the defect that survives every other check: the
/// document serialises, the UI renders, and the generated client fails to
/// compile at the customer's desk.
pub fn dangling_refs(document: &serde_json::Value) -> Vec<String> {
    let mut found = Vec::new();
    collect_refs(document, String::new(), &mut found);
    found
        .into_iter()
        .filter(|(_, target)| resolve_ref(document, target).is_none())
        .map(|(at, target)| format!("{at} -> {target}"))
        .collect()
}

/// Walk every node, recording `(pointer, target)` for each `$ref` string.
fn collect_refs(node: &serde_json::Value, at: String, out: &mut Vec<(String, String)>) {
    match node {
        serde_json::Value::Object(members) => {
            for (key, value) in members {
                if key == "$ref"
                    && let Some(target) = value.as_str()
                {
                    out.push((at.clone(), target.to_owned()));
                    continue;
                }
                collect_refs(value, format!("{at}/{}", escape(key)), out);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                collect_refs(value, format!("{at}/{index}"), out);
            }
        }
        _ => {}
    }
}

/// Resolve a local `#/...` JSON Pointer against `document`.
///
/// A non-local `$ref` (one naming another document) is treated as resolvable,
/// because this check cannot follow it and reporting it would be a false
/// positive.
fn resolve_ref<'a>(document: &'a serde_json::Value, target: &str) -> Option<&'a serde_json::Value> {
    let Some(pointer) = target.strip_prefix('#') else {
        return Some(document);
    };
    document.pointer(pointer)
}

/// Escape a JSON Pointer token, per RFC 6901.
fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Every `operationId` that appears more than once, with its count.
pub fn duplicate_operation_ids(document: &serde_json::Value) -> Vec<(String, usize)> {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let Some(paths) = document.get("paths").and_then(|paths| paths.as_object()) else {
        return Vec::new();
    };
    for item in paths.values() {
        let Some(item) = item.as_object() else {
            continue;
        };
        for (method, operation) in item {
            if !is_http_method(method) {
                continue;
            }
            if let Some(id) = operation.get("operationId").and_then(|id| id.as_str()) {
                *counts.entry(id.to_owned()).or_default() += 1;
            }
        }
    }
    counts.into_iter().filter(|(_, count)| *count > 1).collect()
}

/// Every operation in the document, as `(METHOD, path, operation)`.
pub fn operations(
    document: &serde_json::Value,
) -> Vec<(String, String, &serde_json::Map<String, serde_json::Value>)> {
    let mut out = Vec::new();
    let Some(paths) = document.get("paths").and_then(|paths| paths.as_object()) else {
        return out;
    };
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        for (method, operation) in item {
            if !is_http_method(method) {
                continue;
            }
            if let Some(operation) = operation.as_object() {
                out.push((method.to_uppercase(), path.clone(), operation));
            }
        }
    }
    out
}

/// Whether a path-item member is an operation rather than metadata.
fn is_http_method(key: &str) -> bool {
    matches!(
        key,
        "get" | "put" | "post" | "delete" | "options" | "head" | "patch" | "trace"
    )
}

// ---------------------------------------------------------------------------
// Problem-document helpers
// ---------------------------------------------------------------------------

/// The `(pointer, code)` pairs of an RFC 9457 body's `errors` member.
///
/// Sorted, so an assertion does not depend on field order.
pub fn field_errors(problem: &serde_json::Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = problem["errors"]
        .as_array()
        .map(|errors| {
            errors
                .iter()
                .map(|error| {
                    (
                        error["pointer"].as_str().unwrap_or_default().to_owned(),
                        error["code"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}
