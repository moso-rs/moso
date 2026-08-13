//! The default middleware stack, asserted twice: once as the framework
//! *describes* it, and once as a client *observes* it.
//!
//! The description and the behaviour are two independent sources of truth, and
//! the failure mode this file exists for is the one where they disagree —
//! `moso middleware` prints a stack that is not the stack the process is
//! running.

#![allow(dead_code)]

use std::time::Duration;

use moso::middleware::{MiddlewareStack, Slot};
use moso::prelude::*;
use moso::response::{NoContent, Text};

mod support;
use support::{LogCapture, Reply, get, post_json, send};

// ---------------------------------------------------------------------------
// The application
// ---------------------------------------------------------------------------

/// This application's configuration.
#[derive(Config, Debug, Clone, Default)]
pub struct Cfg {}

/// Answer immediately.
#[endpoint]
async fn ok() -> Result<Text> {
    Ok(Text("ok".to_owned()))
}

/// A body big enough that the compression layer will not skip it.
#[endpoint]
async fn big() -> Result<Json<Vec<String>>> {
    Ok(Json(vec!["the quick brown fox".to_owned(); 400]))
}

/// Take longer than any sane request budget.
#[endpoint]
async fn slow() -> Result<NoContent> {
    moso::deps::tokio::time::sleep(Duration::from_secs(30)).await;
    Ok(NoContent)
}

/// Panic, so the catcher has something to catch.
#[endpoint]
async fn boom() -> Result<NoContent> {
    panic!("a deliberate panic, caught by `catch_panic`");
}

/// Read the whole body, so the limit has to be enforced before we do.
#[endpoint]
async fn swallow(body: moso::extract::Bytes) -> Result<Text> {
    Ok(Text(format!("read {}", body.len())))
}

fn router() -> Router {
    moso::routes! {
        GET  "/ok"      => ok,
        GET  "/big"     => big,
        GET  "/slow"    => slow,
        GET  "/boom"    => boom,
        POST "/swallow" => swallow,
    }
}

/// The application with its stack untouched.
fn app() -> axum::Router<()> {
    App::new(Cfg::default())
        .mount(router())
        .build()
        .expect("builds")
        .into_service()
}

/// The application with one stack edit applied.
fn app_with(edit: impl FnOnce(&mut MiddlewareStack)) -> axum::Router<()> {
    App::new(Cfg::default())
        .mount(router())
        .with_middleware(edit)
        .build()
        .expect("builds")
        .into_service()
}

// ---------------------------------------------------------------------------
// The description
// ---------------------------------------------------------------------------

#[test]
fn the_default_stack_is_in_the_documented_order() {
    let stack = MiddlewareStack::standard();
    let described: Vec<String> = stack
        .describe()
        .into_iter()
        .map(|entry| entry.name.into_owned())
        .collect();

    assert_eq!(
        described,
        [
            "catch_panic",
            "request_id",
            "trace",
            "sensitive_headers",
            "catch_error",
            "request_limits",
            "timeout",
            "body_limit",
            "normalize_path",
            "cors",
            "security_headers",
            "compression",
            "rate_limit",
            "session",
            "metrics",
        ]
        .map(str::to_owned),
        "the stack order is part of the contract: see docs/01-http/16-middleware.md"
    );

    // `describe` lists every slot; `enabled` is what decides whether it runs.
    for slot in Slot::ORDER {
        assert!(
            stack.entry(slot).is_some(),
            "`{slot}` is missing from `describe()`"
        );
    }
}

#[test]
fn the_slots_that_run_by_default_are_the_documented_ones() {
    let stack = MiddlewareStack::standard();

    for slot in [
        Slot::CatchPanic,
        Slot::RequestId,
        Slot::SensitiveHeaders,
        Slot::CatchError,
        Slot::RequestLimits,
        Slot::Timeout,
        Slot::BodyLimit,
        Slot::NormalizePath,
        Slot::SecurityHeaders,
    ] {
        assert!(stack.is_enabled(slot), "`{slot}` should be on by default");
    }

    // Off unless something asks for them: two have no implementation at all,
    // CORS needs an origin list, and metrics needs a recorder.
    for slot in [Slot::RateLimit, Slot::Session, Slot::Cors, Slot::Metrics] {
        assert!(!stack.is_enabled(slot), "`{slot}` should be off by default");
    }

    // The two feature-gated slots say what the build can actually do, rather
    // than claiming a capability that is not compiled in.
    assert_eq!(stack.is_enabled(Slot::Trace), cfg!(feature = "tracing"));
    assert_eq!(
        stack.is_enabled(Slot::Compression),
        cfg!(feature = "compression")
    );
}

#[test]
fn the_default_stack_passes_its_own_ordering_checks() {
    assert!(
        MiddlewareStack::standard().validate().is_empty(),
        "the shipped default must not be a boot error"
    );
}

#[test]
fn the_rendered_stack_matches_what_is_enabled() {
    let stack = MiddlewareStack::standard();
    let rendered = stack.render();
    assert!(rendered.starts_with("GLOBAL\n"), "{rendered}");

    for entry in stack.describe() {
        let mentioned = rendered.contains(entry.name.as_ref());
        assert_eq!(
            mentioned,
            entry.enabled,
            "`{}` is {} in the stack but {} in the rendering:\n{rendered}",
            entry.name,
            if entry.enabled { "on" } else { "off" },
            if mentioned { "printed" } else { "absent" },
        );
    }
}

#[test]
fn an_inserted_layer_lands_where_it_was_asked_to() {
    let mut stack = MiddlewareStack::standard();
    stack.insert_after(Slot::Trace, "tenant", tower::layer::util::Identity::new());
    let names: Vec<String> = stack
        .describe()
        .into_iter()
        .map(|entry| entry.name.into_owned())
        .collect();

    let trace = names
        .iter()
        .position(|name| name == "trace")
        .expect("trace");
    let tenant = names
        .iter()
        .position(|name| name == "tenant")
        .expect("tenant");
    assert_eq!(tenant, trace + 1, "{names:?}");
}

// ---------------------------------------------------------------------------
// The behaviour
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_response_carries_a_correlation_id() {
    let reply = send(app(), get("/ok")).await;
    assert_eq!(reply.status, 200, "{}", reply.body);

    let id = reply.header(moso::REQUEST_ID_HEADER);
    assert_eq!(id.len(), 26, "a ULID, not {id:?}");
    assert!(
        id.chars().all(|c| c.is_ascii_alphanumeric()),
        "a ULID, not {id:?}"
    );
}

#[tokio::test]
async fn a_client_supplied_correlation_id_is_echoed_back() {
    // A real ULID, so the middleware accepts it rather than minting its own.
    let supplied = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let request = axum::http::Request::builder()
        .uri("/ok")
        .header(moso::REQUEST_ID_HEADER, supplied)
        .body(axum::body::Body::empty())
        .expect("request");

    let reply = send(app(), request).await;
    assert_eq!(reply.header(moso::REQUEST_ID_HEADER), supplied);
}

#[tokio::test]
async fn the_security_headers_are_on_every_response() {
    let reply = send(app(), get("/ok")).await;

    assert_eq!(reply.header("x-content-type-options"), "nosniff");
    assert_eq!(reply.header("x-frame-options"), "DENY");
    assert!(
        !reply.header("referrer-policy").is_empty(),
        "a referrer policy is part of the default posture"
    );
}

#[tokio::test]
async fn a_slow_handler_becomes_a_504_problem() {
    let service = app_with(|stack| {
        stack.timeout(Duration::from_millis(20));
    });
    let reply = send(service, get("/slow")).await;

    assert_eq!(reply.status, 504, "{}", reply.body);
    assert!(
        reply
            .header("content-type")
            .starts_with("application/problem+json"),
        "{:?}",
        reply.header("content-type")
    );
    let problem = reply.json();
    assert_eq!(problem["status"], 504);
    // The timeout renders through `catch_error`, which is *outside* `timeout`:
    // if the order were reversed this would be a bare 504 with no document.
    assert!(problem["title"].is_string(), "{}", reply.body);
}

#[tokio::test]
async fn an_oversized_declared_body_is_a_413_before_it_is_read() {
    // The declaration is a gigabyte; the bytes actually sent are none. If the
    // limit were enforced by counting as it reads, this request would be
    // accepted — and a real client would then get to allocate the gigabyte.
    // Nothing here can allocate it: the request never carried it.
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/swallow")
        .header("content-type", "application/octet-stream")
        .header("content-length", "1073741824")
        .body(axum::body::Body::empty())
        .expect("request");

    let reply = send(
        app_with(|stack| {
            stack.body_limit(1024);
        }),
        request,
    )
    .await;

    assert_eq!(reply.status, 413, "{}", reply.body);
    let problem = reply.json();
    assert_eq!(problem["status"], 413);
    assert_eq!(
        problem["max_bytes"], 1024,
        "the client cannot discover the limit any other way: {}",
        reply.body
    );
    assert!(
        !reply.body.contains("read "),
        "the handler answered, so the guard let a gigabyte through: {}",
        reply.body
    );
}

#[tokio::test]
async fn a_body_that_lies_about_its_length_is_still_capped() {
    // No `content-length` at all: the pre-read guard cannot fire, so the cap
    // has to be enforced while reading. The extractor's read is limited, so the
    // handler never sees more than the cap.
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/swallow")
        .header("content-type", "application/octet-stream")
        .header("transfer-encoding", "chunked")
        .body(axum::body::Body::from(vec![b'x'; 64 * 1024]))
        .expect("request");

    let reply = send(
        app_with(|stack| {
            stack.body_limit(1024);
        }),
        request,
    )
    .await;

    assert_eq!(
        reply.status, 413,
        "an undeclared oversize body must not reach the handler intact: {}",
        reply.body
    );
}

#[tokio::test]
async fn a_panicking_handler_is_a_500_problem_and_the_service_survives() {
    let logs = LogCapture::new();
    let guard = logs.install();

    // One service instance, two requests: the second proves the first did not
    // poison anything the stack keeps between calls.
    let service = app();
    let panicked = send(service.clone(), get("/boom")).await;
    let after = send(service, get("/ok")).await;

    drop(guard);

    assert_eq!(panicked.status, 500, "{}", panicked.body);
    let problem = panicked.json();
    assert_eq!(problem["status"], 500);
    assert!(
        problem["request_id"].is_string(),
        "the panic document still has to be correlatable: {}",
        panicked.body
    );

    assert_eq!(after.status, 200, "{}", after.body);
    assert_eq!(after.body, "ok");

    assert!(
        logs.contains("handler panicked"),
        "the panic must be logged, not just answered:\n{}",
        logs.joined()
    );
}

#[tokio::test]
async fn the_panic_document_proves_catch_panic_runs_outside_request_id() {
    let reply = send(app(), get("/boom")).await;

    // `catch_panic` is the outermost layer, so the response never travels back
    // through `request_id` and cannot pick up the header there. The id reaches
    // the document through the slot `catch_panic` planted on the way in — which
    // only works in this order.
    assert_eq!(reply.status, 500);
    assert!(
        !reply.has_header(moso::REQUEST_ID_HEADER),
        "a response produced outside `request_id` cannot carry its header"
    );
    assert!(reply.json()["request_id"].is_string(), "{}", reply.body);
}

#[tokio::test]
async fn the_profile_decides_whether_a_panic_message_is_disclosed() {
    fn service(profile: moso::config::Profile) -> axum::Router<()> {
        App::new(Cfg::default())
            .mount(router())
            .profile(profile)
            .build()
            .expect("builds")
            .into_service()
    }

    let production = send(service(moso::config::Profile::Production), get("/boom")).await;
    assert_eq!(production.status, 500);
    assert!(
        production.json()["detail"].is_null(),
        "a deployed instance must not narrate its panics: {}",
        production.body
    );

    let development = send(service(moso::config::Profile::Dev), get("/boom")).await;
    assert_eq!(development.status, 500);
    assert!(
        development.body.contains("a deliberate panic"),
        "`dev` renders the panic message, which is the whole point of `dev`: {}",
        development.body
    );
}

#[tokio::test]
async fn an_error_is_logged_inside_its_span() {
    let logs = LogCapture::new();
    let guard = logs.install();
    let reply = send(app(), get("/boom")).await;
    drop(guard);

    assert_eq!(reply.status, 500);
    let lines = logs.lines();
    let span = lines.iter().position(|line| line.starts_with("SPAN "));
    let error = lines.iter().position(|line| line.contains("panicked"));

    if let (Some(span), Some(error)) = (span, error) {
        assert!(
            span < error,
            "`catch_error`/`catch_panic` must log inside the `trace` span:\n{}",
            logs.joined()
        );
    }
}

#[tokio::test]
async fn a_response_is_not_compressed_unless_it_is_asked_for() {
    let request = axum::http::Request::builder()
        .uri("/big")
        .body(axum::body::Body::empty())
        .expect("request");
    let reply = send(app(), request).await;

    assert_eq!(reply.status, 200);
    assert!(
        !reply.has_header("content-encoding"),
        "a client that sent no `accept-encoding` must get plain bytes"
    );
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn a_large_response_is_compressed_when_the_client_accepts_it() {
    let request = axum::http::Request::builder()
        .uri("/big")
        .header("accept-encoding", "gzip")
        .body(axum::body::Body::empty())
        .expect("request");
    let reply = send(app(), request).await;

    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("content-encoding"), "gzip");
    assert!(
        reply.bytes.len() < 400 * 20,
        "400 copies of one phrase should compress hard, not to {} bytes",
        reply.bytes.len()
    );
    assert!(
        reply
            .header("vary")
            .to_ascii_lowercase()
            .contains("accept-encoding"),
        "a compressed response must vary on the negotiation header"
    );
}

#[cfg(not(feature = "compression"))]
#[tokio::test]
async fn without_the_feature_nothing_claims_to_compress() {
    let request = axum::http::Request::builder()
        .uri("/big")
        .header("accept-encoding", "gzip, br")
        .body(axum::body::Body::empty())
        .expect("request");
    let reply = send(app(), request).await;

    assert_eq!(reply.status, 200);
    assert!(
        !reply.has_header("content-encoding"),
        "the slot is disabled without its codecs, so nothing may claim an encoding"
    );
}

#[tokio::test]
async fn a_trailing_slash_reaches_the_same_route() {
    let reply = send(app(), get("/ok/")).await;
    assert_eq!(
        reply.status, 200,
        "`normalize_path` should have trimmed the slash: {}",
        reply.body
    );
}

#[tokio::test]
async fn a_disabled_slot_stops_running() {
    let service = app_with(|stack| {
        stack.disable(Slot::SecurityHeaders);
    });
    let reply = send(service, get("/ok")).await;

    assert_eq!(reply.status, 200);
    assert!(
        !reply.has_header("x-content-type-options"),
        "a disabled slot must actually stop running"
    );
}

#[tokio::test]
async fn a_custom_layer_appended_to_the_stack_runs() {
    /// Stamps a header on the way out, so its presence proves it ran.
    #[derive(Clone)]
    struct Stamp;

    impl<S> tower::Layer<S> for Stamp {
        type Service = Stamped<S>;

        fn layer(&self, inner: S) -> Self::Service {
            Stamped(inner)
        }
    }

    #[derive(Clone)]
    struct Stamped<S>(S);

    impl<S> tower::Service<moso::Request> for Stamped<S>
    where
        S: tower::Service<moso::Request, Response = moso::Response> + Clone + Send + 'static,
        S::Future: Send + 'static,
    {
        type Response = moso::Response;
        type Error = S::Error;
        type Future = moso::BoxFuture<'static, core::result::Result<moso::Response, S::Error>>;

        fn poll_ready(
            &mut self,
            cx: &mut core::task::Context<'_>,
        ) -> core::task::Poll<core::result::Result<(), S::Error>> {
            self.0.poll_ready(cx)
        }

        fn call(&mut self, req: moso::Request) -> Self::Future {
            let ready = self.0.clone();
            let mut inner = core::mem::replace(&mut self.0, ready);
            Box::pin(async move {
                let mut response = inner.call(req).await?;
                response
                    .headers_mut()
                    .insert("x-stamped", axum::http::HeaderValue::from_static("yes"));
                Ok(response)
            })
        }
    }

    let service = app_with(|stack| {
        stack.append("stamp", Stamp);
    });
    let reply = send(service, get("/ok")).await;

    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("x-stamped"), "yes");
}

// ---------------------------------------------------------------------------
// The stack and the HTTP configuration must agree
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_http_configuration_reaches_the_stack() {
    // `http.timeout` and `http.body_max` are configuration, not stack edits.
    // If they do not reach the middleware, a deployment's `[http]` section is
    // silently inert — which is worse than it not existing.
    let service = App::new(Cfg::default())
        .mount(router())
        .http_config(moso::http_config::HttpConfig {
            timeout: Duration::from_millis(20),
            body_max: 1024,
            ..moso::http_config::HttpConfig::default()
        })
        .build()
        .expect("builds")
        .into_service();

    let timed_out = send(service.clone(), get("/slow")).await;
    assert_eq!(
        timed_out.status, 504,
        "`http.timeout` must configure the `timeout` slot: {}",
        timed_out.body
    );

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/swallow")
        .header("content-type", "application/octet-stream")
        .header("content-length", "65536")
        .body(axum::body::Body::from(vec![b'x'; 65536]))
        .expect("request");
    let too_big = send(service, request).await;
    assert_eq!(
        too_big.status, 413,
        "`http.body_max` must configure the `body_limit` slot: {}",
        too_big.body
    );
}

#[tokio::test]
async fn an_explicit_stack_edit_beats_the_configuration() {
    // `with_middleware` runs on the builder and `configure` runs inside
    // `build()`, so the derived value is written *after* the explicit one. The
    // explicit one still has to win, because that is the order it reads in.
    let service = App::new(Cfg::default())
        .mount(router())
        .with_middleware(|stack| {
            stack.body_limit(8);
            stack.timeout(Duration::from_millis(20));
        })
        .http_config(moso::http_config::HttpConfig {
            body_max: 4 * 1024 * 1024,
            timeout: Duration::from_secs(300),
            ..moso::http_config::HttpConfig::default()
        })
        .build()
        .expect("builds")
        .into_service();

    let too_big = send(
        service.clone(),
        post_json("/swallow", &format!("\"{}\"", "x".repeat(2048))),
    )
    .await;
    assert_eq!(
        too_big.status, 413,
        "the explicit `body_limit` must win over `http.body_max`: {}",
        too_big.body
    );
    assert_eq!(too_big.json()["max_bytes"], 8, "{}", too_big.body);

    let timed_out = send(service, get("/slow")).await;
    assert_eq!(
        timed_out.status, 504,
        "the explicit `timeout` must win over `http.timeout`: {}",
        timed_out.body
    );
}

#[tokio::test]
async fn a_500_discloses_nothing_by_default_and_everything_when_asked() {
    /// Fail with a detail no client should see.
    #[endpoint]
    async fn explode() -> Result<NoContent> {
        Err(Error::internal_msg(
            "connection to postgres://user:hunter2@db:5432 refused",
        ))
    }

    fn service(expose: bool) -> axum::Router<()> {
        App::new(Cfg::default())
            .mount(moso::routes! { GET "/explode" => explode })
            .http_config(moso::http_config::HttpConfig {
                expose_internal_errors: expose,
                ..moso::http_config::HttpConfig::default()
            })
            .build()
            .expect("builds")
            .into_service()
    }

    let hidden: Reply = send(service(false), get("/explode")).await;
    assert_eq!(hidden.status, 500, "{}", hidden.body);
    assert!(
        !hidden.body.contains("hunter2"),
        "5xx detail must be suppressed: {}",
        hidden.body
    );

    let shown = send(service(true), get("/explode")).await;
    assert_eq!(shown.status, 500, "{}", shown.body);
    assert!(
        shown.body.contains("hunter2"),
        "`http.expose_internal_errors` must actually expose: {}",
        shown.body
    );
}
