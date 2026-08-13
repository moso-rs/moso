//! The rest of the macro surface, exercised against the real runtime.
//!
//! `end_to_end.rs` covers `#[endpoint]`, `routes!`, `ep!`, `#[derive(Schema)]`
//! and `#[derive(Config)]`. This file covers the four that file does not touch —
//! `#[middleware]`, `#[derive(Constrained)]`, `#[derive(Responder)]`,
//! `#[derive(Dependency)]`, `#[derive(Error)]` — plus the extractors and
//! response types a real application reaches for.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use moso::extract::Headers;
use moso::prelude::*;
use moso::response::NoContent;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// `#[derive(Constrained)]`
// ---------------------------------------------------------------------------

/// An order number, which cannot exist in an invalid state.
#[derive(Constrained, Debug, Clone, PartialEq)]
#[constrained(inner = String, pattern = r"^ORD-\d{4}$", format = "order-number")]
pub struct OrderNumber(String);

#[test]
fn a_constrained_newtype_rejects_a_bad_value() {
    assert!(OrderNumber::new("ORD-1234".to_owned()).is_ok());
    assert!(OrderNumber::new("nope".to_owned()).is_err());
}

#[test]
fn a_constrained_newtype_deserialises_through_its_constructor() {
    let good: OrderNumber = serde_json::from_str(r#""ORD-4321""#).expect("valid");
    assert_eq!(good.as_ref(), "ORD-4321");

    let bad = serde_json::from_str::<OrderNumber>(r#""nope""#);
    assert!(bad.is_err(), "the constructor must reject it");
}

#[test]
fn a_constrained_newtype_documents_its_constraint() {
    let mut generator = moso::schema::SchemaGenerator::default();
    let json = serde_json::to_string(&OrderNumber::json_schema(&mut generator)).expect("json");
    assert!(json.contains("pattern"), "{json}");
    assert!(json.contains("order-number"), "{json}");
}

// ---------------------------------------------------------------------------
// `#[derive(Error)]`
// ---------------------------------------------------------------------------

/// The failures this application's domain can produce.
#[derive(Debug, moso::Error)]
pub enum ShopError {
    /// Not enough stock to satisfy the order.
    #[error(status = 409, detail = "Only {available} left in stock")]
    OutOfStock {
        /// How many remain.
        available: u32,
    },
    /// The basket does not exist.
    #[error(status = 404)]
    NoSuchBasket,
}

#[test]
fn a_derived_error_maps_onto_a_problem_document() {
    let problem: Error = ShopError::OutOfStock { available: 2 }.into();
    let response = problem.into_response();
    assert_eq!(response.status(), 409);

    let problem: Error = ShopError::NoSuchBasket.into();
    assert_eq!(problem.into_response().status(), 404);
}

#[test]
fn a_derived_error_interpolates_its_detail() {
    let rendered = ShopError::OutOfStock { available: 2 }.to_string();
    assert!(rendered.contains('2'), "{rendered}");
}

/// One variant per unique response status the taxonomy carries.
///
/// `moso-macros` mirrors `ErrorKind` by hand in its `STATUS_KINDS` table because
/// a proc-macro crate depends on no runtime Moso crate and cannot name the enum.
/// This enum is the seam where the two homes are cross-checked: it stops
/// compiling the moment the derive is asked for a status the macro's table has
/// no kind for, and [`every_response_kind_is_spellable_by_the_derive`] proves
/// its statuses are exactly `ErrorKind::RESPONSE_KINDS`. `504` appears once
/// (`GatewayTimeout`); `ErrorKind::Timeout` shares that status and needs no
/// separate variant.
#[derive(Debug, moso::Error)]
enum EveryResponseKind {
    /// 400.
    #[error(status = 400)]
    BadRequest,
    /// 401.
    #[error(status = 401)]
    Unauthenticated,
    /// 403.
    #[error(status = 403)]
    Forbidden,
    /// 404.
    #[error(status = 404)]
    NotFound,
    /// 405.
    #[error(status = 405)]
    MethodNotAllowed,
    /// 406.
    #[error(status = 406)]
    NotAcceptable,
    /// 409.
    #[error(status = 409)]
    Conflict,
    /// 410.
    #[error(status = 410)]
    Gone,
    /// 412.
    #[error(status = 412)]
    PreconditionFailed,
    /// 413.
    #[error(status = 413)]
    PayloadTooLarge,
    /// 414.
    #[error(status = 414)]
    UriTooLong,
    /// 415.
    #[error(status = 415)]
    UnsupportedMedia,
    /// 416.
    #[error(status = 416)]
    RangeNotSatisfiable,
    /// 422.
    #[error(status = 422)]
    Validation,
    /// 423.
    #[error(status = 423)]
    Locked,
    /// 429.
    #[error(status = 429)]
    TooManyRequests,
    /// 431 — the kind the hand-mirrored table used to lack.
    #[error(status = 431)]
    HeaderFieldsTooLarge,
    /// 500.
    #[error(status = 500)]
    Internal,
    /// 501.
    #[error(status = 501)]
    NotImplemented,
    /// 502.
    #[error(status = 502)]
    BadGateway,
    /// 503.
    #[error(status = 503)]
    Unavailable,
    /// 504.
    #[error(status = 504)]
    GatewayTimeout,
}

#[test]
fn every_response_kind_is_spellable_by_the_derive() {
    use std::collections::BTreeSet;

    // The statuses the derive was actually asked to spell, read back from the
    // generated `VARIANTS` table rather than restated here.
    let spelled: BTreeSet<u16> = EveryResponseKind::variants()
        .iter()
        .map(|(_, status, _, _)| *status)
        .collect();

    // The statuses the taxonomy carries, straight from its one home.
    let taxonomy: BTreeSet<u16> = moso::ErrorKind::RESPONSE_KINDS
        .iter()
        .map(|kind| kind.status().as_u16())
        .collect();

    assert_eq!(
        spelled, taxonomy,
        "`#[derive(Error)]` and `ErrorKind` diverged; add the missing status to \
         moso-macros' STATUS_KINDS and a variant to `EveryResponseKind`",
    );
}

#[test]
fn the_four_thirty_one_kind_reaches_a_problem_document() {
    let problem: Error = EveryResponseKind::HeaderFieldsTooLarge.into();
    assert_eq!(problem.status(), 431);
}

// ---------------------------------------------------------------------------
// `#[derive(Responder)]`
// ---------------------------------------------------------------------------

/// A body serialised with a status the handler does not have to name.
#[derive(Schema, Responder, Debug, Clone)]
#[responder(status = 201)]
pub struct BasketCreated {
    /// The new basket's identifier.
    pub id: u64,
}

#[test]
fn a_responder_uses_the_status_it_declares() {
    let response = BasketCreated { id: 7 }.into_response();
    assert_eq!(response.status(), 201);
}

// ---------------------------------------------------------------------------
// `#[derive(Dependency)]`
// ---------------------------------------------------------------------------

/// The caller, resolved once per request.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    /// Whether the caller may act as an administrator.
    pub admin: bool,
}

impl moso::Dependency for CurrentUser {
    const PROVIDER_REQ: &'static [moso::ProviderReq] = &[];

    async fn resolve(ctx: &RequestCtx) -> Result<Self> {
        Ok(CurrentUser {
            admin: ctx.extension::<bool>().unwrap_or_default(),
        })
    }
}

/// A dependency that counts its own resolutions, to prove per-request
/// memoisation.
///
/// Deliberately a type of its own, reached by exactly one route: a counter on
/// `CurrentUser` would be incremented by every other test in this binary, which
/// run concurrently in the same process.
#[derive(Debug, Clone)]
pub struct Counted(usize);

/// How many times [`Counted`] was resolved.
static RESOLVES: AtomicUsize = AtomicUsize::new(0);

impl moso::Dependency for Counted {
    const PROVIDER_REQ: &'static [moso::ProviderReq] = &[];

    async fn resolve(_ctx: &RequestCtx) -> Result<Self> {
        Ok(Counted(RESOLVES.fetch_add(1, Ordering::SeqCst)))
    }
}

/// An administrator, which is a `CurrentUser` that passed a check.
///
/// `check` names a field on the value `from` resolved to; the derive binds it
/// to `this`, so `"admin"` becomes `this.admin`.
#[derive(Dependency, Debug, Clone)]
#[depends(from = CurrentUser, check = "admin", error = "admin required")]
pub struct AdminUser(pub CurrentUser);

// ---------------------------------------------------------------------------
// `#[middleware]`
// ---------------------------------------------------------------------------

/// Stamp every response with a header, to prove the layer runs.
#[moso::middleware]
async fn stamp(req: moso::Request, next: moso::middleware::Next) -> Result<moso::Response> {
    let mut response = next.run(req).await;
    response
        .headers_mut()
        .insert("x-stamped", "yes".parse().expect("valid header"));
    Ok(response)
}

/// A middleware that extracts before it runs — the shape that needs
/// `middleware_ctx`.
#[moso::middleware]
async fn count_injections(
    Inject(counter): Inject<Counter>,
    req: moso::Request,
    next: moso::middleware::Next,
) -> Result<moso::Response> {
    counter.hits.fetch_add(1, Ordering::SeqCst);
    Ok(next.run(req).await)
}

/// A shared counter, provided at boot.
#[derive(Debug, Default)]
pub struct Counter {
    /// How many requests the injecting middleware saw.
    pub hits: AtomicUsize,
}

// ---------------------------------------------------------------------------
// Endpoints exercising the extractors
// ---------------------------------------------------------------------------

/// Search, reading a typed query string.
#[derive(Schema, Debug, Clone, Default)]
pub struct Search {
    /// Free-text term.
    pub q: Option<String>,
    /// Page size.
    #[schema(range = 1..=100)]
    pub limit: Option<u32>,
}

/// Run a search.
#[endpoint]
async fn search(Query(query): Query<Search>) -> Result<Json<Search>> {
    Ok(Json(query))
}

/// Echo one header back.
#[endpoint]
async fn echo_header(Headers(headers): Headers<Echo>) -> Result<Json<Echo>> {
    Ok(Json(headers))
}

/// The headers this API reads.
#[derive(Schema, Debug, Clone)]
pub struct Echo {
    /// Correlates a client's retries.
    pub x_trace: String,
}

/// A page of results.
#[endpoint]
async fn page() -> Result<Page<u32>> {
    Ok(Page::from_items(vec![1, 2, 3], 10, |item| {
        Cursor::from_bytes(item.to_string().into_bytes())
    }))
}

/// Anything only an administrator may do.
#[endpoint]
async fn admin_only(Depends(admin): Depends<AdminUser>) -> Result<NoContent> {
    assert!(admin.0.admin);
    Ok(NoContent)
}

/// Resolve the same dependency twice, to prove the per-request cache.
#[endpoint]
async fn twice(Depends(a): Depends<Counted>, Depends(b): Depends<Counted>) -> Result<Json<bool>> {
    Ok(Json(a.0 == b.0))
}

/// Return the domain error, so the derived mapping is exercised over the wire.
#[endpoint]
async fn out_of_stock() -> Result<NoContent> {
    Err(ShopError::OutOfStock { available: 2 }.into())
}

fn router() -> Router {
    moso::routes! {
        GET "/search"      => search,
        GET "/echo"        => echo_header,
        GET "/page"        => page,
        GET "/admin"       => admin_only,
        GET "/twice"       => twice,
        GET "/out-of-stock" => out_of_stock,
    }
}

/// This application's configuration.
#[derive(Config, Debug, Clone, Default)]
pub struct Cfg {}

fn app() -> axum::Router<()> {
    App::new(Cfg::default())
        .provide(Counter::default())
        .mount(router().layer(StampLayer::new()))
        .build()
        .expect("builds")
        .into_service()
}

async fn send(
    request: axum::http::Request<axum::body::Body>,
) -> (u16, axum::http::HeaderMap, String) {
    let response = app().oneshot(request).await.expect("infallible");
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn get(path: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_typed_query_string_is_extracted() {
    let (status, _, body) = send(get("/search?q=shoes&limit=10")).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("shoes"), "{body}");
    assert!(body.contains("10"), "{body}");
}

#[tokio::test]
async fn a_query_constraint_is_enforced() {
    let (status, _, body) = send(get("/search?limit=1000")).await;
    assert_eq!(status, 422, "{body}");
    assert!(body.contains("limit"), "{body}");
}

#[tokio::test]
async fn headers_are_extracted_by_field_name() {
    let request = axum::http::Request::builder()
        .uri("/echo")
        .header("x-trace", "abc")
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, _, body) = send(request).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("abc"), "{body}");
}

#[tokio::test]
async fn a_page_serialises_its_envelope() {
    let (status, _, body) = send(get("/page")).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains('1') && body.contains('3'), "{body}");
}

#[tokio::test]
async fn a_middleware_layer_runs() {
    let (status, headers, body) = send(get("/page")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        headers.get("x-stamped").map(|v| v.to_str().unwrap()),
        Some("yes"),
        "the `#[middleware]` layer did not run"
    );
}

#[tokio::test]
async fn a_failing_dependency_check_is_a_403() {
    let (status, _, body) = send(get("/admin")).await;
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("admin required"), "{body}");
}

#[tokio::test]
async fn a_dependency_is_resolved_once_per_request() {
    let before = RESOLVES.load(Ordering::SeqCst);
    let (status, _, body) = send(get("/twice")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        RESOLVES.load(Ordering::SeqCst) - before,
        1,
        "`Depends<T>` must memoise within one request"
    );
    // Both parameters saw the same value, which is the memoisation the counter
    // is only circumstantial evidence for.
    assert_eq!(body, "true", "the two resolutions disagreed");
}

#[tokio::test]
async fn a_derived_error_reaches_the_wire_with_its_status() {
    let (status, _, body) = send(get("/out-of-stock")).await;
    assert_eq!(status, 409, "{body}");
}

#[tokio::test]
async fn an_injecting_middleware_gets_its_provider() {
    let counter = Arc::new(Counter::default());
    let service = App::new(Cfg::default())
        .provide_arc(Arc::clone(&counter))
        .mount(router().layer(CountInjectionsLayer::new()))
        .build()
        .expect("builds")
        .into_service();

    let response = service.oneshot(get("/page")).await.expect("infallible");
    assert_eq!(response.status(), 200);
    assert_eq!(
        counter.hits.load(Ordering::SeqCst),
        1,
        "`middleware_ctx` did not hand the layer a working provider map"
    );
}
