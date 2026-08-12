---
title: Observability
description: One span and one log line per request, a correlation id that reaches every output, a metrics hook with a cardinality cap, and OTLP trace export behind one feature flag.
order: 35
status: shipped
---

Moso instruments the request path for you: one `tracing` span per request, exactly one log line per
request whatever the outcome, a correlation id on the request, the response, the log line and the
problem document, and a metrics seam that cannot explode your time series. The framework emits
through `tracing`, and it can install the subscriber that renders those events for you, from your
`TracingConfig`, behind the `subscriber` cargo feature. You can still install your own instead, and
the framework steps out of the way.

Installing the subscriber yourself is fully supported and covered below. The one thing that still
surprises people is that a Moso application with **no** subscriber installed at all, neither the
framework's nor your own, produces no output and looks broken.

> [!NOTE]
> Trace export and the database, cache and job spans ship behind the `otel` cargo feature, off by
> default; see [exporting traces](#exporting-traces-with-otlp). Moso exports *traces* over OTLP;
> metrics stay behind the `MetricsRecorder` seam and the process-wide counters, which you scrape from
> your own `/metrics` endpoint. See [metrics](#metrics).

## Install a subscriber

With the `subscriber` feature on, the framework installs one for you from `TracingConfig`.
`AppBuilder::tracing_config` wires it: `level` becomes an `EnvFilter`, and `format` selects the
`pretty`, `json` or `compact` formatter. The install happens at serve time and hands back a
`TracingGuard` that flushes on shutdown, so with the `otel` feature you do not lose the last spans of
a draining process.

```rust title="src/lib.rs"
use moso::http_config::TracingConfig;

App::new(config)
    .tracing_config(TracingConfig {
        level: "info".to_owned(),
        format: "json".to_owned(),
        ..TracingConfig::default()
    })
    .mount(routes::router())
```

To take full control instead, add `tracing-subscriber` to the application, not to Moso. The framework
detects that a global subscriber already exists and does not fight it.

```toml title="Cargo.toml"
[dependencies]
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
```

Then install it as the one global subscriber, before anything else runs.

```rust title="src/main.rs"
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> moso::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    my_app::build()?.serve().await
}
```

`with_current_span(true)` is what puts the request span's fields (the correlation id, the method, the
path) onto every event emitted underneath it. Without it you get the log lines but not the
correlation, which is most of the value.

For local work, drop `.json()` and use the default human-readable formatter. Nothing else changes.

> [!NOTE]
> When the framework installs the subscriber, the `TracingGuard` it returns flushes the OTLP exporter
> on shutdown, so the `otel` feature does not drop the last spans of a draining process. When you
> install your own subscriber, Moso does not own it and cannot flush it: the last thing written
> through it is `shutdown complete` from `moso::app`, and if that line reaches your collector and the
> ones before it do not, the flush is the part you have not wired.

## What the framework instruments for you

Four slots in the [default middleware stack](./middleware.md) do the work, in this order, outermost
first.

| Slot | What it contributes |
| --- | --- |
| `catch_panic` | Turns a panic into a 500 problem, logs it at `ERROR`, increments `moso_panics_total` |
| `request_id` | Reads or generates the correlation id, echoes it on the response |
| `trace` | Opens the span every later event inherits, records `status` and `duration_ms` at the end |
| `sensitive_headers` | Marks `authorization`, `cookie` and friends redacted, in both directions |
| `catch_error` | Emits the one log line per request, at a level chosen from the outcome |
| `metrics` | Records one sample per request, off until you give it a recorder |

`trace` needs the `tracing` cargo feature, which is on by default for the `moso` facade. With it off,
the slot starts disabled rather than enabled and inert.

### The request span

One span, named `http.request`, on target `moso::http`, opened at `INFO`.

```text
http.request{method=POST route=/api/v1/users/{id} path=/api/v1/users/0192f
             request_id=01J8XG7K3RQZ4B0N2Y6M9C5V1T status=201 duration_ms=12.4}
```

`route` is the matched **pattern** and `path` is the concrete path, which is the split that lets you
group by endpoint and still see the request. A request that matches no route (a 404, or anything
served by a `Router::mount_axum` mount, whose patterns Moso cannot see) records `<unmatched>`.

`status` and `duration_ms` are recorded when the inner stack returns, so they are on the span but not
on the events emitted inside it. `user_agent` is a field that exists and is empty by default: it is
long, high cardinality, and rarely what a production question turns on.

The span deliberately does not log on entry or on exit. A layer that did would double every line in
the system and make "one log line per request" impossible to hold. `catch_error` is the only thing
that emits.

Adjust it on the builder:

```rust title="src/lib.rs"
use moso::deps::tracing::Level;
use moso::{App, Slot};

let app = App::new(config).with_middleware(|stack| {
    stack.trace(|trace| {
        // Behind a mesh that already traces, keep the span for local debugging
        // and stop paying for it in production.
        trace.level(Level::DEBUG).with_user_agent();
    });
});
```

### The one log line

`catch_error` emits exactly one event per request, on target `moso::http`, with the message
`request` and these fields: `status`, `method`, `route`, `path`, `duration_ms`, `request_id`,
`error`, `headers`.

The level comes from the outcome, not from a global setting.

| Status | Level | Why |
| --- | --- | --- |
| 5xx | `ERROR` | With the error's full source chain in `error` |
| 401, 403, 409, 410, 423, 429 | `WARN` | Worth noticing in aggregate |
| 404, 422 and every other 4xx | `DEBUG` | Routine; at `INFO` they drown everything else |
| 2xx, 3xx | `INFO` | The access line |

So an access log is a filter on level and target, not a second layer. `moso::http` at `INFO` and
above is your access log; `moso::http` at `ERROR` is your incident feed.

Redaction is structural, never a regex over the body. The line carries the error's title, detail and
source chain and never its field errors, because a validation message can quote the value it
rejected and a value is exactly what must not be logged. Headers reach the line only when you ask,
and then only on a 5xx, and then only after every name in `REDACTED_HEADERS` has been replaced with
`[redacted]`:

```rust title="src/lib.rs"
let app = App::new(config).with_middleware(|stack| {
    stack.catch_error(|catch| {
        catch.log_headers = true;
    });
});
```

The redaction list is `authorization`, `proxy-authorization`, `cookie`, `set-cookie`, `x-api-key`,
`x-auth-token`, `x-csrf-token`. One list, used by the log line, by `sensitive_headers` and by the
`Headers` extractor, so the three cannot drift apart.

### Silenced paths

`/healthz`, `/readyz`, `/docs` and `/openapi.json` are kept out of the access log and out of the
metrics. Infrastructure polls them every few seconds and nobody is debugging them, so at one line
each they would be the bulk of a quiet service's log volume. Add your own by prefix:

```rust title="src/lib.rs"
let app = App::new(config).with_middleware(|stack| {
    stack.silence("/internal/probe");
});
```

Silencing is by path prefix and applies to both the log line and the metrics sample.

## Correlating a log line with a request

Every request gets a ULID. The chain is:

1. `request_id` reads `x-request-id`, adopts it when it is printable ASCII within 128 bytes **and**
   parses as a ULID, generates a fresh one otherwise.
2. The header is rewritten on the request, so the handler sees the same value.
3. The id goes into the request extensions, which is where `RequestCtx` reads it.
4. `trace` records it as a span field, so every event underneath carries it.
5. `catch_error` puts it on the log line.
6. Every RFC 9457 problem document carries it as `request_id`.
7. The response echoes it back on `x-request-id`.

ULIDs rather than UUIDs because they sort by creation time, so a log store's index stays dense and a
range scan over a period is a range scan rather than a full scan.

Read it in a handler:

```rust title="src/routes/orders.rs"
use moso::extract::RequestId;
use moso::prelude::*;

/// Place an order.
#[endpoint]
async fn place(RequestId(id): RequestId) -> Result<NoContent> {
    tracing::info!(order.id = %id, "accepted");
    Ok(NoContent)
}
```

You do not need to attach the id yourself: the event is emitted inside the request span, which
already carries it. The example above is for when you want it in the message body of something else,
like a receipt or an outbound header.

At the edge of a public API, stop trusting the client's header. A client-supplied id can be used to
forge a log entry or to poison a log search:

```rust title="src/lib.rs"
use moso::middleware::RequestIdSource;

let app = App::new(config).with_middleware(|stack| {
    stack.request_id(|id| {
        id.source = RequestIdSource::AlwaysGenerate;
    });
});
```

Behind a load balancer or a mesh that already assigns ids, leave it on `TrustHeader`, which is the
default.

### The trace id

`catch_error` reads an inbound W3C `traceparent` header and, when it is well formed, puts its
32-character trace id into the problem document as `trace_id`. With the `otel` feature on, a
well-formed `traceparent` does more: the request span is opened as a **child** of the remote context,
so the request continues the caller's trace rather than starting a fresh one. Without `otel` the
header is correlation-only, exactly as described here: the trace id lands in the problem document and
nothing joins the spans. An all-zero identifier is rejected either way, as the specification requires.

## Metrics

Moso does not depend on a metrics facade. The `metrics` slot takes a `MetricsRecorder`, which has one
method, and an exporter is a small adapter rather than an integration.

```rust title="src/telemetry.rs"
use moso::middleware::{MetricsRecorder, RequestSample};

/// Forwards request samples to whatever exporter this build uses.
pub struct Exporter;

impl MetricsRecorder for Exporter {
    fn record(&self, sample: &RequestSample<'_>) {
        // `record` runs on the request's own task, so it must not block.
        // An exporter that needs I/O pushes onto a channel here.
        metrics::counter!(
            "moso_http_requests_total",
            "route" => sample.route.to_owned(),
            "status" => sample.status.as_u16().to_string(),
        )
        .increment(1);
    }
}
```

Register it, which enables the slot:

```rust title="src/lib.rs"
use std::sync::Arc;

let app = App::new(config).with_middleware(|stack| {
    stack.metrics(Arc::new(crate::telemetry::Exporter));
});
```

A `RequestSample` carries `method`, `route`, `status`, `duration` and `in_flight`. It is borrowed,
not owned, so a recorder that only increments a counter allocates nothing.

### The cardinality guard

`route` is a route **pattern** and never a raw path. Labelling with the raw path is the classic
cardinality explosion, and it is a production incident rather than a tidiness matter. Even a pattern
can run away, so the layer caps the number of distinct route labels it will ever emit at
`MetricsConfig::max_routes` (2000 by default), folds everything past the cap into `<other>`, and warns
about it exactly once.

Two label values are reserved and worth knowing:

| Value | Meaning |
| --- | --- |
| `<unmatched>` | The request matched no pattern Moso knows: a 404, a `mount_axum` mount, a static file mount, or a path a redirecting `normalize_path` answered with a 308 before routing |
| `<other>` | The cardinality cap was reached; this is the fold-everything-else bucket |

A recorder comparing against `<unmatched>` is asking "did this bypass the route table". Everything
that lands there folds into **one** series, whatever the path was, which is what keeps 404 traffic
from becoming a cardinality incident of its own.

The `route` field on the span, on the log line and on the metric sample is one value resolved once
per request, so the three cannot disagree with each other or with the router. The mechanism, and the
exact list of what does and does not get a pattern, is in
[middleware](./middleware.md#the-stack-runs-outside-routing-and-still-knows-the-route).

### Counters the framework keeps itself

Four process-wide counters exist without a recorder, backed by atomics, for a `/metrics` endpoint you
write yourself.

| Function | Metric name | Meaning |
| --- | --- | --- |
| `middleware::metrics::requests_total()` | `moso_http_requests_total` | Requests completed through the metrics slot |
| `middleware::metrics::in_flight()` | `moso_http_in_flight` | Requests in the metrics slot right now |
| `middleware::catch_error::failed_requests_total()` | `moso_requests_failed_total` | Requests answered with a 4xx or 5xx |
| `middleware::catch_panic::panics_total()` | `moso_panics_total` | Panics caught, the signal to alert on |

The first two only move when the `metrics` slot is enabled. The last two always move.

Beyond those four, `moso_core::middleware::metrics::counter(name)` and `gauge(name)` are a
process-wide sink any battery can push a named counter or gauge into without a recorder wired. It is
how the batteries report their own internals: `moso_kv_errors_total` and `moso_kv_operations_total`
come from `moso-kv`, and `moso_authz_audit_dropped` from the authorisation audit path, all through
this one sink. A `/metrics` endpoint you write yourself reads them back alongside the four above.

### Background job metrics

With the `jobs` feature, `moso::jobs::metrics::snapshot()` renders the whole job registry as
Prometheus text, which a worker pod can serve directly.

```rust title="src/routes/metrics.rs"
use moso::prelude::*;
use moso::response::Text;

/// Prometheus scrape endpoint for the worker.
#[endpoint]
async fn metrics() -> Result<Text> {
    Ok(Text(moso::jobs::metrics::snapshot()))
}
```

The registry covers `moso_jobs_enqueued_total`, `moso_jobs_duration_seconds`,
`moso_jobs_queue_depth`, `moso_jobs_latency_seconds`, `moso_jobs_retries_total`, `moso_jobs_dlq_total`,
plus `moso_jobs_backpressure_active` and `moso_jobs_outbox_lag_seconds`. Every label comes from a
registered job's wire name or a declared queue, both bounded by your source code; the one unbounded
input, a retry reason, is mapped onto a closed set of five.

## Tracing a job back to the request that queued it

`moso::jobs::trace` owns W3C trace propagation for the job pipeline: the identifiers, the header
format and the parentage. It does not own an exporter.

A `TraceContext` is the 55 bytes of a `traceparent`. `trace::scope` puts one in a task-local for the
duration of a future, `EnqueueBuilder` reads it and writes it onto the queue row, and the worker reads
it back, makes a **child** of it, and runs the job inside that.

```rust title="src/routes/signup.rs"
use moso::jobs::trace::{self, TraceContext};

// One seam: wrap the work, and every enqueue underneath carries the trace.
let outcome = trace::scope(TraceContext::root(), async {
    jobs.enqueue(SendWelcome { user_id }).await
})
.await;
```

The worker opens a span on target `moso::jobs` named `job` carrying `job`, `queue`, `id`, `attempt`,
`worker`, `trace_id`, `span_id` and `parent_span_id`, so a failed welcome email and the signup that
caused it are one query rather than two unrelated log lines an hour apart.

A row with no `traceparent`, or a malformed one, starts a fresh trace rather than joining an unrelated
one. An all-zero identifier is rejected, as the specification requires, because a zero trace id joins
every request in the system into one trace.

Blocking work is instrumented too: `moso::task::blocking` opens a `moso.blocking` span carrying
`otel.kind = "internal"` and how many permits were queued, so time spent on the bounded blocking pool
is attributable instead of being a gap in the timeline.

## Asserting on logs in tests

`moso-test` captures the application's `tracing` output and attributes each line to the request that
produced it, using the `request_id` span field. A failing assertion prints the lines belonging to the
failing request underneath it.

```rust title="tests/api.rs"
app.client()
    .post("/users")
    .json(&serde_json::json!({ "username": "ada", "email": "a@b.example" }))
    .send()
    .await;

app.logs().assert_no_errors();
```

`LogAssertions` also has `assert_contains(level, needle)`, `assert_contains_at_least`,
`assert_none_containing`, `for_request(request_id)`, `records()` and `dump()`. `assert_no_errors` is a
good last line for every test: an endpoint that returns the right status while logging a stack trace
is still broken.

If your test binary installed its own global subscriber first, capture cannot be installed. That is
not an error: `is_capturing()` returns `false`, every buffer stays empty, and the failure output says
so rather than silently omitting the most useful section. See [testing](./testing.md).

## Exporting traces with OTLP

The `otel` cargo feature turns the framework's own spans into an OpenTelemetry trace stream. It pulls
`opentelemetry-otlp`, which is a feature-gated workspace dependency, and exports over **gRPC**
(`grpc-tonic`) with no TLS, so it adds no OpenSSL to your build. The feature is off by default, so a
build that does not ask for it pays for none of it.

`TracingConfig` drives it, and its three OTLP fields are read: `otlp_endpoint` is the collector to
send to, `service_name` labels the stream, and `sample_ratio` is the head sampling rate.
`AppBuilder::tracing_config` wires the whole thing, the exporter is installed at serve time, and the
`TracingGuard` flushes it on shutdown.

```rust title="src/lib.rs"
use moso::http_config::TracingConfig;

App::new(config)
    .tracing_config(TracingConfig {
        otlp_endpoint: Some("http://otel-collector:4317".to_owned()),
        service_name: "blog-api".to_owned(),
        sample_ratio: 0.1,
        ..TracingConfig::default()
    })
    .mount(routes::router())
```

Once it is on, three spans join the request span in the trace, none of which cost anything without a
subscriber that records them:

- The ORM opens a `db.query` span carrying the **parameterised** SQL only. Bound values are never
  recorded, because a value is exactly what must not leak into a trace.
- `moso-kv` opens a `kv.op` span for each cache operation.
- `moso-jobs` opens a job-execution span and propagates the W3C trace context across
  `enqueue -> execute`, so a job runs inside a child of the request that queued it. This is the same
  propagation the [job section above](#tracing-a-job-back-to-the-request-that-queued-it) describes,
  and with `otel` on it reaches your collector rather than only your logs.

A well-formed inbound `traceparent` makes the request span a child of the caller's, as described under
[the trace id](#the-trace-id).

## Failure modes

**No output at all.** No subscriber is installed. See [install a subscriber](#install-a-subscriber).

**Log lines with no `request_id`.** Either your subscriber is not rendering span fields (set
`with_current_span(true)` on the JSON formatter) or the event was emitted outside a request, from a
startup hook or a worker.

**A panic 500 with no `x-request-id` header.** Expected. `catch_panic` is the outermost slot, so a
response it produces never travels back through `request_id` and cannot pick up the header. The id
still reaches the problem document body through a one-shot cell the inner layer fills, so the response
is correlatable even though the header is absent.

**Three log lines for one failure.** Something in your code is logging errors. `Error` is a value;
`catch_error` is the event. Return the error and let the boundary log it exactly once. An error logged
at its construction site, again where it is wrapped, and again at the boundary looks like three
incidents.

**A metrics backend falling over.** Check whether you built a label from a raw path or an error
message. Moso's own labels are bounded; a recorder that adds `"path" => sample` fields of its own is
not.

**The stack refuses to boot** with a message about ordering. `catch_error` must run inside `trace` so
the error log carries the span, and outside `timeout` so an expiry renders as a problem document.
`metrics` must be innermost. These are checked at boot because every consequence is subtle enough to
survive review.

## See also

- [Middleware](./middleware.md) for the slot model and how to reorder or replace any of these layers.
- [Errors](./errors.md) for the problem document that carries `request_id` and `trace_id`.
- [Health and shutdown](./health-and-shutdown.md) for the probes that are excluded from this output.
- [Background jobs](./jobs.md) for the worker side of trace propagation.
- [Testing](./testing.md) for the log capture harness.
- [Configuration](./configuration.md) for the `http` and `tracing` sections.
