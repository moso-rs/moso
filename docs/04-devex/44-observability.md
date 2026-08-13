# 44 - Observability

> 🟡 **Status: partially implemented.** Built: the request span with a *pattern*-based `route` field,
> request-id generation and propagation, sensitive-header redaction, the `metrics` slot with a
> cardinality cap and a dyn-compatible `MetricsRecorder`, health checks, `/healthz` and `/readyz`.
> ⛔ Not built: OpenTelemetry export (`opentelemetry-otlp` is not a dependency), W3C `traceparent`
> propagation, `db.*` span fields (no ORM), and request→job→outbound trace propagation (no jobs, no
> instrumented HTTP client).

## Position

Observability is not a battery you enable; it is a property of the framework. Every Moso app emits
correlated traces, structured logs, and useful metrics **by default**, with zero configuration, and
the defaults are safe (no PII, no secrets, bounded cardinality).

## Tracing

Built on `tracing` + `tracing-subscriber`, with OpenTelemetry export via `opentelemetry-otlp`.

### The request span

Every request opens a span with these fields, populated as they become known:

```
http.request  method=POST route=/api/v1/users/{id} path=/api/v1/users/0192f…
              status=201 duration_ms=12.4
              request_id=01J8XG7K3RQZ4B0N2Y6M9C5V1T
              trace_id=4bf92f3577b34da6a3ce929d0e0e4736 span_id=00f067aa0ba902b7
              user_id=usr_123 tenant=acme
              db.queries=3 db.duration_ms=8.1
              client_ip=203.0.113.4 user_agent="…"
```

- `route` is the **pattern**, not the concrete path - this is what keeps metric cardinality bounded
  and is the single most common observability mistake.
- W3C `traceparent` is propagated in and out. An incoming trace context is continued, not replaced.
- `user_id`/`tenant` are attached by the auth/tenant dependencies when they resolve.
- Nested spans: each SQL statement, each outbound HTTP call, each job enqueue, each cache
  operation, each template render.

### Sampling

Head-based ratio sampling by default (`tracing.sample_ratio`, default 1.0 in dev, 0.05 in
production), with two overrides that matter:
- **Always sample errors.** A 5xx is always traced, regardless of the ratio.
- **Always sample slow requests.** Above `tracing.slow_ms` (default 1000).

Achieved with a deferred-decision sampler: the span is recorded into a bounded buffer and the export
decision is made at close. This is what makes low sampling ratios useful rather than infuriating.

## Logging

```
# dev: human, colourised, one line per event, with the span context
14:32:11.482  INFO  http  201 POST /api/v1/users  12.4ms  3q  user=usr_123

# production: JSON, one object per event
{"ts":"2026-07-29T14:32:11.482Z","level":"info","target":"moso::http","msg":"request",
 "method":"POST","route":"/api/v1/users","status":201,"duration_ms":12.4,
 "request_id":"01J8…","trace_id":"4bf9…","user_id":"usr_123","db_queries":3}
```

Rules:
- **One log line per request**, at completion. Not one on entry and one on exit.
- Format is chosen by profile; `log.format = json|pretty|compact` overrides.
- `RUST_LOG` works as expected; `log.level` in config is the fallback.
- **Redaction is structural, not a regex.** `#[schema(secret)]` fields, configured headers
  (`authorization`, `cookie`, `set-cookie`, `x-api-key`, `proxy-authorization`), and configured
  query parameters (`token`, `code`, `signature`) never reach a log sink. A test greps the entire
  test-suite log output for canary secrets.
- **PII policy:** request bodies are never logged by default. `log.bodies = "on_error"` logs them
  only for 5xx, with secret fields redacted, capped at 4 KB. `log.bodies = "always"` exists for
  dev and warns at boot in production.
- Rate limiting on repeated identical warnings (a failing dependency should not produce 10k
  lines/s), with a summary line: `… 4,312 similar messages suppressed in the last 60s`.

## Metrics

Prometheus by default (`/metrics`, on a **separate port** so it is not publicly exposed), OTLP
optional.

### Framework metrics (always present)

| Metric | Type | Labels |
| --- | --- | --- |
| `moso_http_requests_total` | counter | `method`, `route`, `status` |
| `moso_http_request_duration_seconds` | histogram | `method`, `route` |
| `moso_http_request_size_bytes` / `response_size_bytes` | histogram | `route` |
| `moso_http_in_flight` | gauge | - |
| `moso_panics_total` | counter | `route` |
| `moso_db_query_duration_seconds` | histogram | `operation`, `entity` |
| `moso_db_pool_connections` | gauge | `state` (idle/active/waiting) |
| `moso_db_statements_per_request` | histogram | `route` |
| `moso_db_transaction_retries_total` | counter | `reason` |
| `moso_kv_operations_total` / `_errors_total` | counter | `op`, `namespace` |
| `moso_cache_hits_total` / `_misses_total` | counter | `namespace` |
| `moso_jobs_*` | see `03-batteries/32` | `job`, `queue` |
| `moso_auth_attempts_total` | counter | `method`, `outcome` |
| `moso_authz_denials_total` | counter | `permission` |
| `moso_build_info` | gauge=1 | `version`, `commit`, `rustc` |

**Cardinality guard:** the metrics layer refuses labels not in a declared allowlist and caps
distinct label-value combinations per metric (default 2000), logging once when the cap is hit rather
than exploding the backend. Blowing up a Prometheus with unbounded labels is a real and expensive
incident; the framework should not enable it.

### Application metrics

```rust
// example
moso::metrics::counter!("orders_placed_total", 1, "channel" => order.channel);
moso::metrics::histogram!("order_value_eur", order.total.as_f64());
```
Thin wrappers over the `metrics` crate facade, so any exporter works.

## Health and readiness

Specified in `01-http/10-app-lifecycle.md`. Summary: `/healthz` (liveness, never touches
dependencies), `/readyz` (runs registered checks, 2 s budget, returns 503 immediately on shutdown).
Both are excluded from access logs and metrics.

## Profiling

- `moso dev --profile` and a `/debug/pprof` endpoint (dev-only, and behind
  `Perm::AdminAccess` when the admin is present) exposing `pprof`-format CPU profiles and
  allocation profiles via `jemalloc`/`dhat` when the feature is enabled.
- `tokio-console` support behind the `console` feature, with a one-line enablement in
  `moso doctor --fix-config`. Diagnosing a stuck async task without it is miserable.
- A `/debug/tasks` page in dev listing blocked tasks and their spans.

## Error tracking

An `ErrorReporter` trait with a Sentry-compatible implementation shipped as
`moso-observability/sentry`. Every 5xx and every panic is reported with the request context, the
span, the user (id only, never PII), the release version, and breadcrumbs from the request's spans.
Rate-limited and deduplicated by error fingerprint.

## Configuration

```toml
[tracing]
enabled = true
format = "json"            # json | pretty | compact
level = "info"
sample_ratio = 0.05
always_sample_errors = true
slow_ms = 1000
otlp_endpoint = "http://otel-collector:4317"
service_name = "shop"          # defaults to the app name
resource_attributes = { "deployment.environment" = "production" }

[metrics]
enabled = true
port = 9090                # separate listener
path = "/metrics"

[log]
bodies = "on_error"        # never | on_error | always
redact_headers = ["authorization", "cookie", "x-api-key"]
redact_query = ["token", "code", "signature"]
```

## What the developer sees in dev

`moso dev` prints a compact request line with the timing breakdown and query count, and the dev
error page shows the request's full span tree:

```
POST /api/v1/checkout  201  84.2ms
├─ authz.check                    0.4ms
├─ db.transaction                72.1ms
│  ├─ SELECT products …           2.1ms   (12 rows)
│  ├─ INSERT orders …             1.8ms
│  └─ payment.charge             67.9ms   ← the actual cost
└─ jobs.enqueue SendReceipt       1.1ms
```

Making the slow span visible in the terminal without configuring any tooling is a large,
inexpensive win.

## Acceptance criteria (WP-27)

1. A request with an incoming `traceparent` continues the trace; the outbound HTTP client and the
   enqueued job propagate it (asserted on span parentage across three hops).
2. Metric labels use route patterns; hitting `/users/1` and `/users/2` produces one series.
3. Cardinality guard caps series and logs once; verified with 5000 distinct label values.
4. No secret, cookie, or `#[schema(secret)]` value appears anywhere in the test suite's log output
   (canary grep over the full run).
5. `/metrics` is not reachable on the main HTTP port.
6. Error sampling: with `sample_ratio = 0.0`, a 500 is still exported.
7. Observability overhead: < 5% throughput cost with tracing + metrics at default settings,
   measured in `examples/bench`.
8. `tokio-console` and `/debug/pprof` work in dev and are absent in release builds.
