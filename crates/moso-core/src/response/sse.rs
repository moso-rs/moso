//! `Sse<S>` — server-sent events, documented as such.
//!
//! ```
//! use moso::prelude::*;
//! use moso::response::sse::{Event, Sse};
//! use futures_util::{Stream, stream};
//! use std::pin::Pin;
//! use std::time::Duration;
//!
//! /// A stream of events, named concretely so it can appear in a signature.
//! pub type Events = Pin<Box<dyn Stream<Item = Result<Event>> + Send>>;
//!
//! /// Stream progress to the browser.
//! #[endpoint]
//! async fn progress() -> Result<Sse<Events>> {
//!     let events = stream::iter([Ok(Event::data("started")), Ok(Event::data("done"))]);
//!     Ok(Sse::new(Box::pin(events) as Events).keep_alive(Duration::from_secs(15)))
//! }
//! # fn main() {
//! assert_eq!(
//!     String::from_utf8(Event::data("hi").named("tick").to_bytes()).unwrap(),
//!     "event: tick\ndata: hi\n\n",
//! );
//! # }
//! ```
//!
//! The stream type has to be nameable: `#[endpoint]` writes
//! `<ReturnType as Describe>::describe(…)`, and `impl Trait` is not allowed in a
//! path. A boxed stream alias is the shape that compiles. An application that
//! builds its own streams adds `futures-util` to its manifest.
//!
//! # Shutdown
//!
//! An SSE handler outlives a normal request, so it must cooperate with the
//! drain: take an `Inject<Signal>` and stop when it fires. The framework logs a
//! warning naming any route still streaming when the grace period ends, which
//! is how a leaked stream is found rather than guessed at.
//!
//! Compression is skipped for `text/event-stream`: buffering defeats the point
//! of the transport, and a compressed stream that a proxy holds is worse than
//! an uncompressed one that arrives.

use std::time::Duration;

use futures_util::StreamExt;
use moso_openapi::{OperationBuilder, Param, ResponseSpec};
use serde::Serialize;

use crate::Response;
use crate::error::{Error, Result};
use crate::response::{Describe, IntoResponse};

/// One server-sent event.
///
/// Every member is optional except the data, matching the wire format, so a
/// heartbeat comment and a typed event both fit the same struct.
///
/// ```
/// use moso::response::sse::Event;
///
/// // The wire format is the thing being built, so it is worth seeing.
/// assert_eq!(
///     String::from_utf8(Event::data("hello").to_bytes()).unwrap(),
///     "data: hello\n\n",
/// );
///
/// let named = Event::data("tick").named("clock").with_id("7");
/// assert_eq!(
///     String::from_utf8(named.to_bytes()).unwrap(),
///     "event: clock\nid: 7\ndata: tick\n\n",
/// );
///
/// // A JSON payload is one call, and a multi-line body is split into `data:` lines.
/// let json = Event::json(&serde_json::json!({ "n": 1 })).unwrap();
/// assert!(String::from_utf8(json.to_bytes()).unwrap().starts_with("data: {"));
/// ```
///
/// Give events ids when the stream is resumable: a reconnecting client sends the
/// last one back in `Last-Event-ID`, which
/// [`last_event_id`] reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Event {
    /// The `event:` field — the client-side listener name.
    pub name: Option<String>,
    /// The `id:` field, which the client echoes as `Last-Event-ID` on reconnect.
    pub id: Option<String>,
    /// The `retry:` field, in milliseconds.
    pub retry: Option<u64>,
    /// The `data:` field. Multi-line data is split across lines on the wire.
    pub data: String,
    /// A `:` comment line, used for keep-alives.
    pub comment: Option<String>,
}

impl Event {
    /// An event carrying `data` verbatim.
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            data: data.into(),
            ..Self::default()
        }
    }

    /// An event whose data is `value` serialised as JSON.
    ///
    /// # Errors
    /// A 500 if `value` cannot be serialised, so the failure surfaces where the
    /// event is built rather than as a truncated frame on the wire.
    pub fn json<T: Serialize + ?Sized>(value: &T) -> Result<Self> {
        let data = serde_json::to_string(value).map_err(|error| {
            Error::internal(error).with_detail("a server-sent event could not be serialised")
        })?;
        Ok(Self::data(data))
    }

    /// A keep-alive comment, which clients ignore and proxies do not time out.
    pub fn comment(text: impl Into<String>) -> Self {
        Self {
            comment: Some(text.into()),
            ..Self::default()
        }
    }

    /// Name the event.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the event id.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Suggest a reconnection delay.
    pub fn with_retry(mut self, retry: Duration) -> Self {
        self.retry = Some(retry.as_millis() as u64);
        self
    }

    /// Render as wire bytes, including the terminating blank line.
    ///
    /// A carriage return or newline inside `event`, `id` or a comment would end
    /// the field and let the value inject a frame of its own, so they are
    /// stripped. `data` is *split* on newlines instead, because multi-line data
    /// is legal and the client rejoins the lines with `\n`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = String::with_capacity(self.data.len() + 32);

        if let Some(comment) = &self.comment {
            for line in comment.split(['\r', '\n']) {
                out.push(':');
                out.push_str(line);
                out.push('\n');
            }
        }
        if let Some(name) = &self.name {
            out.push_str("event: ");
            push_single_line(&mut out, name);
            out.push('\n');
        }
        if let Some(id) = &self.id {
            out.push_str("id: ");
            push_single_line(&mut out, id);
            out.push('\n');
        }
        if let Some(retry) = self.retry {
            out.push_str("retry: ");
            out.push_str(&retry.to_string());
            out.push('\n');
        }
        // An empty `data` with a comment is a keep-alive and dispatches
        // nothing; an otherwise-empty event still sends `data:` so the client
        // fires a listener.
        if !self.data.is_empty() || self.comment.is_none() {
            for line in self.data.split('\n') {
                out.push_str("data: ");
                out.push_str(line.strip_suffix('\r').unwrap_or(line));
                out.push('\n');
            }
        }

        out.push('\n');
        out.into_bytes()
    }
}

/// Append `value` with every CR and LF removed.
fn push_single_line(out: &mut String, value: &str) {
    out.extend(value.chars().filter(|c| *c != '\r' && *c != '\n'));
}

/// The request header a client sends when it reconnects to a stream.
///
/// A stream that assigns ids with [`Event::with_id`] should read this and
/// resume from it; a stream that does not will replay from the beginning, which
/// is the bug this constant exists to make findable.
pub const LAST_EVENT_ID_HEADER: &str = "last-event-id";

/// The value of the client's `Last-Event-ID` header, if it sent one.
///
/// [`IntoResponse`] never sees the request, so resumption is the handler's job:
/// take a `HeaderMap`, read this, and start the stream after that id.
///
/// ```
/// use moso::deps::http::HeaderMap;
/// use moso::response::sse::{LAST_EVENT_ID_HEADER, last_event_id};
///
/// let mut headers = HeaderMap::new();
/// assert_eq!(last_event_id(&headers), None);
///
/// headers.insert(LAST_EVENT_ID_HEADER, "42".parse().unwrap());
/// assert_eq!(last_event_id(&headers), Some("42"));
/// ```
///
/// A handler takes a `HeaderMap`, reads this, and starts its stream after that
/// id — `Sse::new(events_after(last_event_id(&headers)))`.
pub fn last_event_id(headers: &http::HeaderMap) -> Option<&str> {
    headers.get(LAST_EVENT_ID_HEADER)?.to_str().ok()
}

/// A `text/event-stream` response over a stream of [`Event`]s.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::sse::{Event, Sse};
/// use futures_util::{Stream, stream};
/// use std::pin::Pin;
/// use std::time::Duration;
///
/// /// A stream of events, named concretely so it can appear in a signature.
/// pub type Events = Pin<Box<dyn Stream<Item = Result<Event>> + Send>>;
///
/// /// Stream progress to the browser.
/// #[endpoint]
/// async fn progress() -> Result<Sse<Events>> {
///     let events = stream::iter([Ok(Event::data("started")), Ok(Event::data("done"))]);
///     Ok(Sse::new(Box::pin(events) as Events).keep_alive(Duration::from_secs(15)))
/// }
/// # fn main() { assert_eq!(Router::new().get("/progress", moso::ep!(progress)).len(), 1); }
/// ```
///
/// An `Err` in the stream becomes a terminal `error` event carrying the problem's
/// `type`, `title` and `status` — never the detail of a 5xx.
///
/// A stream outlives its request, so a long-lived one should select on
/// `Inject<Signal>` and close when shutdown starts.
pub struct Sse<S> {
    stream: S,
    keep_alive: Option<Duration>,
}

impl<S> Sse<S> {
    /// Stream `stream` as server-sent events.
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            keep_alive: None,
        }
    }

    /// Send a comment line every `interval` while the stream is idle.
    ///
    /// Without one, a proxy with a 60-second idle timeout closes a quiet stream
    /// and the client reconnects in a loop. 15 seconds is the usual choice.
    pub fn keep_alive(mut self, interval: Duration) -> Self {
        self.keep_alive = Some(interval);
        self
    }

    /// The underlying stream.
    pub fn into_inner(self) -> S {
        self.stream
    }
}

impl<S> core::fmt::Debug for Sse<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sse")
            .field("keep_alive", &self.keep_alive)
            .finish_non_exhaustive()
    }
}

/// The event name an in-stream failure is reported under.
///
/// A stream that has already sent its headers cannot change its status code, so
/// a failure mid-stream becomes a named event a client can listen for rather
/// than a connection that simply stops. `addEventListener("error", …)` on the
/// browser side is how it is observed.
pub const ERROR_EVENT: &str = "error";

/// What a keep-alive comment says. The text is ignored by every client; it is
/// there so a packet capture is readable.
const KEEP_ALIVE_COMMENT: &str = "keep-alive";

/// The stream's state while it is being turned into a body.
struct SseState<S> {
    stream: core::pin::Pin<Box<S>>,
    keep_alive: Option<Duration>,
    done: bool,
}

impl<S> IntoResponse for Sse<S>
where
    S: futures_util::Stream<Item = Result<Event>> + Send + 'static,
{
    fn into_response(self) -> Response {
        let state = SseState {
            stream: Box::pin(self.stream),
            keep_alive: self.keep_alive,
            done: false,
        };

        let frames = futures_util::stream::unfold(state, |mut state| async move {
            if state.done {
                return None;
            }
            let next = match state.keep_alive {
                // A quiet stream still has to say something, or the proxy in
                // front of it closes the connection on its idle timeout and the
                // client reconnects in a loop.
                Some(interval) => match tokio::time::timeout(interval, state.stream.next()).await {
                    Ok(next) => next,
                    Err(_elapsed) => {
                        let frame = Event::comment(KEEP_ALIVE_COMMENT).to_bytes();
                        return Some((frame_ok(frame), state));
                    }
                },
                None => state.stream.next().await,
            };

            match next {
                Some(Ok(event)) => Some((frame_ok(event.to_bytes()), state)),
                Some(Err(error)) => {
                    // The status line is long gone, so the failure travels as a
                    // final event and the stream ends.
                    state.done = true;
                    Some((frame_ok(error_frame(&error)), state))
                }
                None => None,
            }
        });

        let mut response = Response::new(axum::body::Body::from_stream(frames));
        for (name, value) in [
            (http::header::CONTENT_TYPE, "text/event-stream"),
            // A cached event stream is not an event stream.
            (http::header::CACHE_CONTROL, "no-cache"),
            // nginx buffers proxied responses by default, which holds every
            // event until the buffer fills. This is the documented opt-out.
            (http::HeaderName::from_static("x-accel-buffering"), "no"),
        ] {
            response
                .headers_mut()
                .insert(name, http::HeaderValue::from_static(value));
        }
        response
    }
}

/// One frame, in the shape `Body::from_stream` wants.
fn frame_ok(bytes: Vec<u8>) -> core::result::Result<bytes::Bytes, core::convert::Infallible> {
    Ok(bytes::Bytes::from(bytes))
}

/// Render an error as a terminal `error` event.
///
/// Carries the same `type`/`title`/`status` an RFC 9457 problem would, minus
/// the detail of a 5xx: a stream is no place to start disclosing internals that
/// the error path is careful about everywhere else.
fn error_frame(error: &Error) -> Vec<u8> {
    let mut payload = serde_json::Map::new();
    payload.insert("type".into(), error.type_uri().into());
    payload.insert("title".into(), error.title().into());
    payload.insert("status".into(), error.status().as_u16().into());
    if let (Some(detail), true) = (error.detail(), error.kind().detail_is_client_safe()) {
        payload.insert("detail".into(), detail.into());
    }
    Event::data(serde_json::Value::Object(payload).to_string())
        .named(ERROR_EVENT)
        .to_bytes()
}

impl<S> Describe for Sse<S> {
    fn describe(op: &mut OperationBuilder) {
        op.parameter(
            Param::header(LAST_EVENT_ID_HEADER)
                .required(false)
                .schema_of::<String>()
                .description(
                    "The id of the last event the client received. Sent automatically by \
                     `EventSource` when it reconnects, so the stream can resume rather than \
                     replay.",
                ),
        );
        op.response(
            200,
            ResponseSpec::sse("A stream of server-sent events, open until either side closes it."),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::tests::described;
    use http_body_util::BodyExt;
    use moso_openapi::ParameterLocation;

    fn wire(event: &Event) -> String {
        String::from_utf8(event.to_bytes()).expect("events are UTF-8")
    }

    async fn body_of(response: Response) -> String {
        String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .expect("body")
                .to_bytes()
                .to_vec(),
        )
        .expect("UTF-8")
    }

    #[test]
    fn a_bare_event_is_one_data_line_and_a_blank() {
        assert_eq!(wire(&Event::data("hello")), "data: hello\n\n");
    }

    #[test]
    fn every_field_is_emitted_in_wire_order() {
        let event = Event::data("hello")
            .named("tick")
            .with_id("42")
            .with_retry(Duration::from_millis(2500));
        assert_eq!(
            wire(&event),
            "event: tick\nid: 42\nretry: 2500\ndata: hello\n\n"
        );
    }

    #[test]
    fn multi_line_data_becomes_one_data_line_each() {
        assert_eq!(
            wire(&Event::data("one\ntwo\nthree")),
            "data: one\ndata: two\ndata: three\n\n"
        );
        // CRLF input produces LF-terminated field lines, not stray carriage
        // returns inside them.
        assert_eq!(wire(&Event::data("one\r\ntwo")), "data: one\ndata: two\n\n");
        // An explicitly empty data field still dispatches an event.
        assert_eq!(wire(&Event::data("")), "data: \n\n");
    }

    #[test]
    fn a_comment_is_a_keep_alive_that_dispatches_nothing() {
        assert_eq!(wire(&Event::comment("keep-alive")), ":keep-alive\n\n");
        assert!(!wire(&Event::comment("x")).contains("data:"));
    }

    #[test]
    fn a_field_value_cannot_inject_a_frame() {
        // A newline in `event` or `id` would otherwise end the field and let
        // the value write its own `data:` line.
        let event = Event::data("x")
            .named("tick\n\ndata: injected")
            .with_id("1\nevent: spoof");
        assert_eq!(
            wire(&event),
            "event: tickdata: injected\nid: 1event: spoof\ndata: x\n\n"
        );
        assert_eq!(wire(&event).matches("\n\n").count(), 1, "exactly one frame");
    }

    #[test]
    fn json_events_carry_the_serialised_value() {
        let event = Event::json(&serde_json::json!({"a": 1})).expect("serialises");
        assert_eq!(wire(&event), "data: {\"a\":1}\n\n");
    }

    #[tokio::test]
    async fn a_stream_is_framed_as_text_event_stream() {
        let events = futures_util::stream::iter(vec![
            Ok(Event::data("one")),
            Ok(Event::data("two").named("tick")),
        ]);
        let response = Sse::new(events).into_response();

        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-cache")
        );
        assert_eq!(
            response
                .headers()
                .get("x-accel-buffering")
                .and_then(|v| v.to_str().ok()),
            Some("no")
        );
        assert_eq!(
            body_of(response).await,
            "data: one\n\nevent: tick\ndata: two\n\n"
        );
    }

    #[tokio::test]
    async fn a_failure_mid_stream_becomes_a_final_error_event() {
        let events = futures_util::stream::iter(vec![
            Ok(Event::data("one")),
            Err(crate::Error::internal_msg("the database went away")),
            Ok(Event::data("never sent")),
        ]);
        let body = body_of(Sse::new(events).into_response()).await;

        assert!(body.starts_with("data: one\n\n"), "{body}");
        assert!(body.contains("event: error\n"), "{body}");
        assert!(body.contains("\"status\":500"), "{body}");
        // A 5xx detail is no more disclosable here than anywhere else.
        assert!(!body.contains("the database went away"), "{body}");
        assert!(!body.contains("never sent"), "the stream ends: {body}");
    }

    #[tokio::test]
    async fn a_client_safe_detail_does_reach_the_error_event() {
        let events =
            futures_util::stream::iter(vec![Err(crate::Error::bad_request("unknown channel"))]);
        let body = body_of(Sse::new(events).into_response()).await;
        assert!(body.contains("unknown channel"), "{body}");
        assert!(body.contains("\"status\":400"), "{body}");
    }

    #[tokio::test]
    async fn a_quiet_stream_gets_keep_alive_comments() {
        // A stream that says nothing for a while, then yields once and ends.
        // The exact number of comments depends on scheduling, so the assertion
        // is on the shape rather than the count.
        let events = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(120)).await;
            Ok(Event::data("finally"))
        });
        let body = body_of(
            Sse::new(events)
                .keep_alive(Duration::from_millis(10))
                .into_response(),
        )
        .await;

        assert!(body.starts_with(":keep-alive\n\n"), "{body}");
        assert!(body.ends_with("data: finally\n\n"), "{body}");
        assert!(!body.contains("event:"), "a comment dispatches nothing");
    }

    #[tokio::test]
    async fn keep_alive_is_off_unless_asked_for() {
        let events = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok(Event::data("finally"))
        });
        let body = body_of(Sse::new(events).into_response()).await;
        assert_eq!(body, "data: finally\n\n");
    }

    #[test]
    fn last_event_id_is_read_from_the_request() {
        let mut headers = http::HeaderMap::new();
        assert_eq!(last_event_id(&headers), None);
        headers.insert(LAST_EVENT_ID_HEADER, http::HeaderValue::from_static("42"));
        assert_eq!(last_event_id(&headers), Some("42"));
    }

    #[test]
    fn sse_documents_the_stream_and_the_resumption_header() {
        let op = described::<Sse<futures_util::stream::Empty<Result<Event>>>>();

        let response = op.response(200).expect("200 documented");
        assert!(response.content.contains_key("text/event-stream"));

        let parameter = op
            .parameter(ParameterLocation::Header, LAST_EVENT_ID_HEADER)
            .expect("`Last-Event-ID` documented");
        assert!(!parameter.required);
    }

    #[test]
    fn into_inner_and_debug_do_not_touch_the_stream() {
        let sse = Sse::new(futures_util::stream::empty::<Result<Event>>())
            .keep_alive(Duration::from_secs(15));
        assert!(format!("{sse:?}").contains("keep_alive"));
        let _ = sse.into_inner();
    }
}
