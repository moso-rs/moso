//! The correlation id: read it, or make one.
//!
//! Every request gets an id, every response echoes it, every log line carries
//! it and every error document contains it. That chain is what turns "a user
//! reports a 500" into a query, and it costs one header.
//!
//! # What "trust the header" means here
//!
//! A [`RequestCtx`](crate::RequestCtx) carries a `Ulid`, not a string, so an
//! adopted id has to be one. A client-supplied value is adopted when it passes
//! [`is_acceptable`] *and* parses as a ULID; anything else is replaced by a
//! freshly generated id. The alternative — keeping the client's arbitrary
//! string on the wire while the framework used a different id internally —
//! would put two different ids in front of the same operator, which is worse
//! than replacing one.
//!
//! The value written back is always the canonical 26-character spelling, so the
//! header, the log line and the problem document agree character for character.

use std::convert::Infallible;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use http::{HeaderName, HeaderValue};
use tower::Service;
use ulid::{ULID_LEN, Ulid};

use crate::router::Route;
use crate::{BoxFuture, Request, Response};

/// Where the id comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestIdSource {
    /// Trust the client's header when it sent one, generate otherwise.
    ///
    /// The default, and correct behind a load balancer or a service mesh that
    /// already assigns ids. The value is length-limited and character-checked
    /// before use, because it ends up in log lines.
    TrustHeader,
    /// Always generate, ignoring anything the client sent.
    ///
    /// Correct at the edge of a public API: a client-supplied id can be used to
    /// forge or to poison a log search.
    AlwaysGenerate,
}

/// How the `request_id` slot behaves.
#[derive(Debug, Clone)]
pub struct RequestIdConfig {
    /// The header read and written. `x-request-id` by default.
    pub header: &'static str,
    /// Where the id comes from.
    pub source: RequestIdSource,
    /// Whether to echo the id on the response.
    pub echo: bool,
    /// The longest client-supplied id accepted before one is generated instead.
    pub max_len: usize,
}

impl Default for RequestIdConfig {
    fn default() -> Self {
        Self {
            header: crate::REQUEST_ID_HEADER,
            source: RequestIdSource::TrustHeader,
            echo: true,
            max_len: 128,
        }
    }
}

impl RequestIdConfig {
    /// Ignore client-supplied ids.
    pub fn always_generate(mut self) -> Self {
        self.source = RequestIdSource::AlwaysGenerate;
        self
    }

    /// Read and write a different header.
    pub fn header(mut self, header: &'static str) -> Self {
        self.header = header;
        self
    }

    /// Stop echoing the id on responses.
    pub fn no_echo(mut self) -> Self {
        self.echo = false;
        self
    }

    /// A one-line summary for `moso middleware`.
    pub fn summary(&self) -> String {
        let source = match self.source {
            RequestIdSource::TrustHeader => "trust-header",
            RequestIdSource::AlwaysGenerate => "always-generate",
        };
        format!(
            "header={} generator=ulid source={source}{}",
            self.header,
            if self.echo { "" } else { " echo=off" }
        )
    }
}

/// Generate a fresh id.
///
/// ULIDs rather than UUIDs: they sort by creation time, so a log store's index
/// stays dense and a range scan over a period is a range scan rather than a
/// full scan.
pub fn generate() -> Ulid {
    Ulid::generate()
}

/// Whether a client-supplied id is safe to adopt.
///
/// Printable ASCII only, no whitespace, within `max_len`. A rejected id is
/// replaced rather than refused, because failing a request over a malformed
/// diagnostic header would be absurd.
pub fn is_acceptable(value: &str, max_len: usize) -> bool {
    !value.is_empty() && value.len() <= max_len && value.bytes().all(|byte| byte.is_ascii_graphic())
}

/// A one-shot cell carrying the correlation id back out to `catch_panic`.
///
/// `catch_panic` sits *outside* this layer, so when a panic unwinds past it the
/// id it needs for the 500 has already been dropped along with the request. The
/// outer layer therefore puts one of these in the request extensions on the way
/// in and reads it on the way out. Empty when a panic happened before this
/// layer ran, which is exactly when there was no id.
#[derive(Clone, Debug, Default)]
pub(crate) struct RequestIdSlot(Arc<OnceLock<String>>);

impl RequestIdSlot {
    /// Record the id. The first writer wins; there is only ever one.
    pub(crate) fn set(&self, id: &str) {
        let _ = self.0.set(id.to_owned());
    }

    /// The id, if this request got one.
    pub(crate) fn get(&self) -> Option<String> {
        self.0.get().cloned()
    }
}

/// Wrap `service` in the correlation-id layer.
pub fn layer(config: &RequestIdConfig, service: Route) -> Route {
    // An invalid header name is a programming error, not a request error, and
    // failing every request over it would be a strange way to say so. The fall
    // back is the documented default, and it is reported once.
    let header = HeaderName::try_from(config.header).unwrap_or_else(|_| {
        tracing::warn!(
            header = config.header,
            "not a valid header name; falling back to `x-request-id`"
        );
        HeaderName::from_static(crate::REQUEST_ID_HEADER)
    });

    Route::new(RequestIdMiddleware {
        inner: service,
        header,
        source: config.source,
        echo: config.echo,
        max_len: config.max_len,
    })
}

/// The service [`layer`] builds.
#[derive(Clone)]
struct RequestIdMiddleware {
    inner: Route,
    header: HeaderName,
    source: RequestIdSource,
    echo: bool,
    max_len: usize,
}

impl RequestIdMiddleware {
    /// The id for this request: the client's when it is usable, ours otherwise.
    fn id_for(&self, req: &Request) -> Ulid {
        if self.source == RequestIdSource::AlwaysGenerate {
            return generate();
        }
        req.headers()
            .get(&self.header)
            .and_then(|value| value.to_str().ok())
            .filter(|value| is_acceptable(value, self.max_len))
            .and_then(|value| Ulid::from_string(value).ok())
            .unwrap_or_else(generate)
    }
}

impl Service<Request> for RequestIdMiddleware {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);

        let id = self.id_for(&req);
        // 26 bytes on the stack rather than a `String`: this runs on every
        // request and the value is copied into the header anyway.
        let mut buffer = [0_u8; ULID_LEN];
        let text: &str = id.array_to_str(&mut buffer);
        let value = HeaderValue::from_str(text).unwrap_or_else(|_| HeaderValue::from_static(""));

        if let Some(slot) = req.extensions().get::<RequestIdSlot>() {
            slot.set(text);
        }
        req.headers_mut().insert(self.header.clone(), value.clone());
        // `RequestCtx::new` reads this first, so the context, the header and
        // the log line cannot disagree.
        req.extensions_mut().insert(id);

        let header = self.header.clone();
        let echo = self.echo;
        Box::pin(async move {
            let mut response = inner.call(req).await?;
            if echo {
                response.headers_mut().insert(header, value);
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt as _;

    fn echo_route() -> Route {
        Route::new(tower::service_fn(|req: Request| async move {
            let mut response = Response::new(axum::body::Body::empty());
            // Prove the *request* was rewritten, not just the response.
            if let Some(value) = req.headers().get(crate::REQUEST_ID_HEADER) {
                response
                    .headers_mut()
                    .insert("x-seen-by-handler", value.clone());
            }
            if let Some(id) = req.extensions().get::<Ulid>() {
                response.headers_mut().insert(
                    "x-seen-ulid",
                    HeaderValue::from_str(&id.to_string()).expect("ascii"),
                );
            }
            Ok::<_, Infallible>(response)
        }))
    }

    async fn send(config: &RequestIdConfig, incoming: Option<&str>) -> http::HeaderMap {
        let mut request = Request::new(axum::body::Body::empty());
        if let Some(incoming) = incoming {
            request.headers_mut().insert(
                HeaderName::from_static(crate::REQUEST_ID_HEADER),
                HeaderValue::from_str(incoming).expect("ascii"),
            );
        }
        layer(config, echo_route())
            .oneshot(request)
            .await
            .expect("infallible")
            .headers()
            .clone()
    }

    #[test]
    fn defaults_trust_the_header_and_echo_it() {
        let config = RequestIdConfig::default();
        assert_eq!(config.source, RequestIdSource::TrustHeader);
        assert!(config.echo);
        assert_eq!(config.header, "x-request-id");
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(generate(), generate());
    }

    #[test]
    fn the_summary_names_the_header_and_the_generator() {
        assert_eq!(
            RequestIdConfig::default().summary(),
            "header=x-request-id generator=ulid source=trust-header"
        );
        assert!(
            RequestIdConfig::default()
                .no_echo()
                .summary()
                .contains("echo=off")
        );
    }

    // ── is_acceptable ────────────────────────────────────────────────────

    #[test]
    fn an_acceptable_id_is_short_printable_and_unspaced() {
        assert!(is_acceptable("01J8XG7K3RQZ4B0N2Y6M9C5V1T", 128));
        assert!(!is_acceptable("", 128));
        assert!(!is_acceptable("has space", 128));
        assert!(!is_acceptable("tab\there", 128));
        assert!(!is_acceptable("new\nline", 128));
        assert!(!is_acceptable("café", 128));
        assert!(!is_acceptable(&"a".repeat(129), 128));
        assert!(is_acceptable(&"a".repeat(128), 128));
    }

    // ── the layer ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_client_ulid_is_adopted_end_to_end() {
        let id = "01J8XG7K3RQZ4B0N2Y6M9C5V1T";
        let headers = send(&RequestIdConfig::default(), Some(id)).await;
        assert_eq!(headers[crate::REQUEST_ID_HEADER], id);
        assert_eq!(headers["x-seen-by-handler"], id);
        assert_eq!(headers["x-seen-ulid"], id);
    }

    #[tokio::test]
    async fn an_unusable_client_id_is_replaced_rather_than_refused() {
        for bad in ["not a ulid", "", "x".repeat(400).as_str()] {
            let headers = send(&RequestIdConfig::default(), Some(bad)).await;
            let assigned = headers[crate::REQUEST_ID_HEADER]
                .to_str()
                .expect("ascii")
                .to_owned();
            assert_ne!(assigned, bad);
            assert!(Ulid::from_string(&assigned).is_ok(), "{assigned}");
        }
    }

    #[tokio::test]
    async fn an_absent_header_is_generated() {
        let headers = send(&RequestIdConfig::default(), None).await;
        let assigned = headers[crate::REQUEST_ID_HEADER].to_str().expect("ascii");
        assert!(Ulid::from_string(assigned).is_ok());
    }

    #[tokio::test]
    async fn always_generate_ignores_the_client() {
        let id = "01J8XG7K3RQZ4B0N2Y6M9C5V1T";
        let headers = send(&RequestIdConfig::default().always_generate(), Some(id)).await;
        assert_ne!(headers[crate::REQUEST_ID_HEADER], id);
    }

    #[tokio::test]
    async fn no_echo_keeps_the_id_off_the_response() {
        let headers = send(&RequestIdConfig::default().no_echo(), None).await;
        assert!(!headers.contains_key(crate::REQUEST_ID_HEADER));
        // …but the handler still saw one.
        assert!(headers.contains_key("x-seen-by-handler"));
    }

    #[tokio::test]
    async fn the_panic_slot_is_filled_when_one_is_present() {
        let slot = RequestIdSlot::default();
        let mut request = Request::new(axum::body::Body::empty());
        request.extensions_mut().insert(slot.clone());

        layer(&RequestIdConfig::default(), echo_route())
            .oneshot(request)
            .await
            .expect("infallible");

        let recorded = slot.get().expect("the layer records the id");
        assert!(Ulid::from_string(&recorded).is_ok());
    }
}
