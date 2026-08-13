---
title: Server sent events and realtime
description: Stream server-sent events from a handler with Sse, keep quiet connections open through proxies, resume with Last-Event-ID, cooperate with shutdown, and see what WebSocket support exists.
order: 33
status: shipped
---

Moso ships one push transport: server-sent events. `Sse<S>` turns a stream of `Event` values into a
`text/event-stream` response with the three headers that make it work behind a real proxy, documents
itself in OpenAPI including the resumption header, and turns a mid-stream failure into a terminal
event rather than a connection that simply stops.

Moso is SSE-first, and WebSockets are handled by re-exposing Axum. The `ws` cargo feature turns on
axum's own `WebSocketUpgrade` directly rather than wrapping it in a framework type. The
[section at the end](#websockets) shows how you mount a socket route and what that hands you.

| Piece | Where | State |
| --- | --- | --- |
| `Sse<S>`, `Event`, `last_event_id` | `moso::response::sse` | shipped |
| Cross-process fan-out (`Bus`, `Topic`, `Presence`) | `moso_kv::bus` | shipped |
| Shutdown cooperation (`Signal`, `Drain`) | `moso::shutdown` | shipped |
| A framework WebSocket type, auth before upgrade, message caps, ping/pong | axum via the `ws` feature | re-exposed |

## The smallest stream

```rust
use moso::prelude::*;
use moso::response::sse::{Event, Sse};
use futures_util::{Stream, stream};
use std::pin::Pin;
use std::time::Duration;

/// A stream of events, named concretely so it can appear in a signature.
pub type Events = Pin<Box<dyn Stream<Item = Result<Event>> + Send>>;

/// Stream progress to the browser.
#[endpoint]
async fn progress() -> Result<Sse<Events>> {
    let events = stream::iter([Ok(Event::data("started")), Ok(Event::data("done"))]);
    Ok(Sse::new(Box::pin(events) as Events).keep_alive(Duration::from_secs(15)))
}
```

The type alias is not decoration. `#[endpoint]` writes `<ReturnType as Describe>::describe(..)`, and
`impl Trait` is not allowed in a path, so the stream type has to be nameable. A boxed alias is the
shape that compiles. An application that builds its own streams adds `futures-util` to its manifest;
Moso does not re-export it.

On the browser side that is an ordinary `EventSource`:

```javascript
const source = new EventSource("/progress");
source.addEventListener("message", (e) => console.log(e.data));
source.addEventListener("error", (e) => console.error(JSON.parse(e.data)));
```

## Building events

`Event` mirrors the wire format, so every field is optional except the data.

| Constructor or builder | Wire effect |
| --- | --- |
| `Event::data(text)` | `data: text` |
| `Event::json(&value)` | `data: {…}`, serialised. Errors as a 500 where the event is built |
| `Event::comment(text)` | `:text`, which dispatches no listener |
| `.named(name)` | `event: name`, the client-side listener name |
| `.with_id(id)` | `id: id`, which the client echoes back on reconnect |
| `.with_retry(duration)` | `retry: milliseconds`, a reconnection hint |
| `.to_bytes()` | The frame, including the terminating blank line |

The wire format is worth seeing, since it is what you are actually building:

```rust
assert_eq!(
    String::from_utf8(Event::data("hello").to_bytes()).unwrap(),
    "data: hello\n\n",
);

let named = Event::data("tick").named("clock").with_id("7");
assert_eq!(
    String::from_utf8(named.to_bytes()).unwrap(),
    "event: clock\nid: 7\ndata: tick\n\n",
);
```

Multi-line data becomes one `data:` line per line, because that is legal and the client rejoins
them with `\n`. Carriage returns and line feeds are **stripped** from `event`, `id` and a comment
instead, because a newline there would end the field and let the value write a frame of its own.
A user-supplied event name cannot inject an event.

## What the response sets

`Sse` writes three headers and nothing else touches the body.

| Header | Value | Why |
| --- | --- | --- |
| `Content-Type` | `text/event-stream` | The transport |
| `Cache-Control` | `no-cache` | A cached event stream is not an event stream |
| `X-Accel-Buffering` | `no` | nginx buffers proxied responses by default, holding every event until the buffer fills. This is the documented opt-out |

Compression is skipped for `text/event-stream`. Buffering defeats the point of the transport, and a
compressed stream a proxy is holding is worse than an uncompressed one that arrives.

`Sse::describe` documents a 200 whose content type is `text/event-stream` and an optional
`Last-Event-ID` header parameter, so the operation appears correctly in the
[OpenAPI document](./openapi.md) with no annotation.

## Keeping a quiet connection alive

A stream that says nothing for a minute gets closed by whatever sits in front of it, and the browser
reconnects in a loop. `.keep_alive(interval)` sends a comment line whenever the stream has been
silent for that long.

```rust
Ok(Sse::new(stream).keep_alive(Duration::from_secs(15)))
```

Fifteen seconds is the usual choice, comfortably under the sixty-second idle timeout most proxies
default to. A comment (`:keep-alive`) dispatches no listener, so the client sees nothing.

Keep-alive is **off unless you ask for it**. A stream that produces an event every second does not
need it. A notification feed that may be silent all afternoon does.

The timer is a `tokio::time::timeout` around the next poll of your stream, so the interval is
measured from the last thing sent rather than on a fixed schedule. A busy stream sends no comments
at all.

## Resuming a stream

An `EventSource` that reconnects sends the last id it saw back in `Last-Event-ID`. Read it and start
after that point.

```rust
use moso::prelude::*;
use moso::deps::http::HeaderMap;
use moso::response::sse::{Event, Sse, last_event_id};

/// Resume a feed from where the client left off.
#[endpoint]
async fn feed(headers: HeaderMap) -> Result<Sse<Events>> {
    let after = last_event_id(&headers);
    let events = events_after(after);
    Ok(Sse::new(events).keep_alive(std::time::Duration::from_secs(15)))
}
```

Resumption is the handler's job, not the response's: `IntoResponse` never sees the request. If you
assign ids with `.with_id(..)` and do not read `last_event_id`, a reconnecting client replays your
stream from the beginning. That is the bug `LAST_EVENT_ID_HEADER` exists to make findable.

> [!IMPORTANT]
> Resumption needs a log you can seek into. The pub/sub bus has no replay: `KvBus` reports
> `replay: false`, and a Redis or PostgreSQL subscriber receives only what is published while it is
> subscribed. If you need gap-free delivery across a reconnect, the events have to come out of a
> table or a stream you own, with the bus used only as the wake-up.

## Failures in the middle of a stream

Once the status line has gone out, a handler cannot change it. An `Err` in the stream therefore
becomes a terminal event named `error` carrying the same `type`, `title` and `status` an RFC 9457
problem would, and the stream ends. The detail of a 5xx is suppressed, exactly as it is
[everywhere else](./errors.md); a client-safe detail (a 4xx) does travel.

```text
data: one

event: error
data: {"type":"about:blank","title":"Internal Server Error","status":500}

```

Listen for it on the client with `addEventListener("error", ..)`. Nothing after the error is sent,
so a stream that reports a failure has finished.

`Event::json` failing to serialise is the other in-stream failure, and it surfaces where the event
is built rather than as a truncated frame on the wire.

## Backpressure

The response body pulls one frame at a time. `Sse` polls your stream when the transport asks for the
next chunk, so a client that reads slowly slows the producer down rather than accumulating frames in
memory. There is no queue inside `Sse` and no buffer you can configure, because there is nothing to
configure.

That means the buffering decision belongs to whatever feeds the stream, and the choice matters:

- **A channel you own.** A bounded `tokio::sync::mpsc` gives you real backpressure: the producer
  waits. That is right when the producer is a request handler and wrong when it is a shared event
  loop that must not block on one slow reader.
- **The in-process bus.** `LocalBus` is a `tokio::sync::broadcast` with a per-subscriber buffer of
  256 messages, adjustable with `.buffer(n)`. A subscriber that falls more than the buffer behind
  **misses messages** and gets a `WARN` naming the channel and the count. The stream continues,
  because a dropped notification is better than a disconnected socket.
- **`KvBus` over Redis or PostgreSQL.** Same shape, plus whatever the backend drops. There is no
  replay, so a slow subscriber loses events and cannot ask for them again.

If losing events is not acceptable, do not stream them straight from the bus. Publish an identifier,
have the handler read the authoritative rows, and send those.

`TopicStream` makes one related decision worth knowing: a message that fails to decode is **skipped
and counted**, not returned as an error, because one bad publish from an older deployment must not
end a subscriber's stream and disconnect every socket behind it. `TopicStream::skipped()` reports
the count, and it belongs on a dashboard.

## Shutdown

An SSE handler outlives a normal request. `Sse` does nothing about the drain by itself, so a stream
that ignores shutdown holds a deploy open for the whole grace period and is then dropped
mid-response.

Take `Inject<Signal>` and stop when it fires, and take a named guard from the `Drain` so the drain
knows what it is waiting for.

```rust title="src/routes/events.rs"
use moso::prelude::*;
use moso::response::sse::{Event, Sse};
use moso::shutdown::Drain;
use moso::Signal;
use futures_util::{Stream, StreamExt};
use std::pin::Pin;
use std::time::Duration;

/// A stream of events, named concretely so it can appear in a signature.
pub type Events = Pin<Box<dyn Stream<Item = Result<Event>> + Send>>;

/// Stream notifications until the process is asked to stop.
#[endpoint]
async fn notifications(
    Inject(drain): Inject<Drain>,
    Inject(signal): Inject<Signal>,
) -> Result<Sse<Events>> {
    let guard = drain.guard("GET /notifications");
    let source = ticks().map(|tick| Ok(Event::json(&tick)?));
    let events = source.take_until(async move {
        signal.recv().await;
        drop(guard);
    });
    Ok(Sse::new(Box::pin(events) as Events).keep_alive(Duration::from_secs(15)))
}
```

`signal.recv()` returns immediately if shutdown has already begun, so a stream that opens during the
drain does not wait out the grace. `signal.is_shutting_down()` answers the same question without
awaiting, for a loop that checks between iterations.

Whatever guards are still held when the grace expires get named in one `WARN` line:

```text
WARN moso::app: the drain did not finish inside the grace period; these are still open.
     A long-lived handler must select on `Inject<Signal>` and close.
     grace=25s outstanding=3 still_open="GET /notifications (x2), nightly-export"
```

Without that line the symptom is "deploys take 25 seconds" and nobody knows why. See
[health and shutdown](./health-and-shutdown.md) for the two drain stages and the grace budget.

## Fanning out across processes

A socket is held by whichever instance the load balancer picked, and the event that should reach it
usually happens on another. `moso_kv::Bus` is the piece that closes that gap: typed `Topic`s over an
in-process broadcast, Redis pub/sub, or PostgreSQL `LISTEN`/`NOTIFY`.

```rust
use moso_kv::Topic;
use serde::{Deserialize, Serialize};

/// What arrives in a user's notification feed.
#[derive(Serialize, Deserialize)]
pub struct Notification {
    /// What to show.
    pub text: String,
}

/// One user's notification feed.
pub struct UserNotifications(pub u64);

impl Topic for UserNotifications {
    type Message = Notification;
    const NAME: &'static str = "notifications";

    fn instance(&self) -> std::borrow::Cow<'_, str> {
        self.0.to_string().into()
    }
}
```

`bus.subscribe(&UserNotifications(user_id))` gives a `TopicStream<UserNotifications>` of decoded
messages, which you map into `Event::json(&message)?` and hand to `Sse`. `bus.publish(&topic, &msg)`
sends one from anywhere, including a background job.

The one flag that decides whether any of this works in production is
`Bus::capabilities().cross_process`. `LocalBus` reports `false` and can never see another process,
by design. A test that only checks that publishing succeeded passes locally and fails on two pods;
assert on the capability instead.

`Presence` tracks who is connected: `join`, `heartbeat`, `leave`, `members` and `count`, keyed by
channel with a TTL, so a crashed instance's members expire rather than lingering. Reach it with
`Bus::presence()`, which returns `Option<&dyn Presence>`.

The bus, its backends, its failure modes and the full `Presence` surface are covered in
[rate limiting and locks](./rate-limiting.md), which is where the `Kv` handle it rides on is
documented.

## WebSockets

WebSockets are Axum's job in a Moso app, by design. To be precise about the seam:

- `moso-core` declares `ws = ["axum/ws"]` and the `moso` facade forwards it as
  `ws = ["moso-core/ws"]`.
- The feature is a straight pass-through: turning it on turns on axum's own `ws` feature, which is
  what you build the socket on.
- Moso adds no wrapper of its own over `WebSocketUpgrade`, no typed `WebSocket<ClientMsg, ServerMsg>`
  and no OpenAPI extension for a socket route, so a socket stays plain Axum.

What you can do today is use axum directly and mount it. `Router::mount_axum` takes an
`axum::Router<()>` and puts it under a prefix, and everything mounted that way is outside the
OpenAPI document by construction.

```toml title="Cargo.toml"
[dependencies]
moso = { version = "0.1", features = ["ws"] }
```

```rust title="src/routes/socket.rs"
use moso::deps::axum;
use moso::Router;

/// Mount a plain Axum WebSocket route under `/ws`.
pub fn routes() -> Router {
    let sockets = axum::Router::new().route("/", axum::routing::any(handler));
    Router::new().mount_axum("/ws", sockets)
}
```

Everything the design documents describe for WebSockets lives on the Axum side of the seam, so it is
yours to wire in the handler:

| Concern | Where you handle it |
| --- | --- |
| Authentication before the upgrade | Run your own extractor in the axum handler before calling `on_upgrade` |
| A message size cap | Configure it on axum's `WebSocketUpgrade` |
| Ping/pong with a dead-peer timeout | Write the timer yourself |
| Per-connection rate limiting | Call `Kv::rate_limit` per message. See [rate limiting](./rate-limiting.md) |
| Graceful close on shutdown | Take a `Signal` and a `Drain` guard as the SSE example above does |
| Connection metrics | Instrument the handler yourself |
| A typed message pair | Serialise and deserialise by hand |
| Documentation of the route | Nothing; a mounted route is outside the document |

Reach for SSE first. It works through proxies that do not understand an upgrade, reconnects
automatically with resumption built into the client, and needs no infrastructure you do not already
have. A WebSocket earns its complexity only when the client genuinely needs to send a high rate of
messages back.

## Failure modes and gotchas

- **A stream with no keep-alive behind a proxy.** The symptom is a reconnect loop every sixty
  seconds with no error anywhere. Add `.keep_alive(..)`.
- **nginx buffering.** `X-Accel-Buffering: no` is set for you. A different proxy may need its own
  opt-out, and the symptom is identical: events arrive in bursts when a buffer fills.
- **`impl Stream` in the return type.** It does not compile, because `#[endpoint]` names the return
  type in a path. Use a boxed alias.
- **Assigning ids without reading `Last-Event-ID`.** Every reconnect replays the whole stream.
- **A handler that never selects on `Signal`.** It holds the deploy open for the full grace and is
  then dropped. Take the guard so the warning names it.
- **A 5xx detail does not reach the client.** By design. Correlate with the request id in your logs.
- **`LocalBus` never crosses a process.** `cross_process` is `false` and it is not a bug.
- **A subscriber that falls behind loses messages.** The gap is logged, not returned. There is no
  replay to recover it from.
- **Events are not batched.** One `Event` is one frame. A stream producing thousands of events per
  second per connection is a design to reconsider, not a flag to set.

## See also

- [Responses](./responses.md) for the other response types and what each contributes to the document.
- [Health and shutdown](./health-and-shutdown.md) for `Signal`, `Drain` and the grace budget.
- [Rate limiting and locks](./rate-limiting.md) for the bus, presence and the `Kv` handle.
- [Errors](./errors.md) for what the `error` event's payload means.
- [OpenAPI](./openapi.md) for how a streaming operation is documented.
