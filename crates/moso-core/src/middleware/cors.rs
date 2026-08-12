//! `cors` — off until you configure it, and never permissive by accident.
//!
//! There is no `CorsConfig::permissive()`. `Access-Control-Allow-Origin: *`
//! combined with credentials is the single most common security misconfiguration
//! in web APIs, and a framework that ships a one-word way to get there is
//! complicit. [`CorsConfig::any_origin`] exists, is documented as
//! credentials-incompatible, and is rejected at boot if credentials are also
//! enabled — which is the actual rule the browser enforces, surfaced early
//! instead of as a console error in someone else's browser.
//!
//! The whole slot requires the `cors` cargo feature, which pulls
//! `tower-http/cors`. The configuration type exists unconditionally so that
//! turning the feature on never changes the shape of an application's
//! `with_middleware` block. Enabling the slot with the feature off is a boot
//! error rather than a silent no-op — see
//! [`MiddlewareStack::validate`](crate::MiddlewareStack::validate).
//!
//! The layer itself is `tower_http::cors::CorsLayer` verbatim. CORS is a long
//! specification with a lot of corners, and the corners are where the security
//! properties live; reimplementing it would be the wrong kind of ambition.

use std::time::Duration;

use crate::error::BootError;
use crate::router::Route;

/// How the `cors` slot behaves.
#[derive(Debug, Clone, Default)]
pub struct CorsConfig {
    /// Allowed origins. Empty means the slot is inert.
    pub origins: Vec<String>,
    /// Whether any origin is allowed. Incompatible with credentials.
    pub any_origin: bool,
    /// Allowed methods. Defaults to the safe set plus the ones the route table
    /// actually uses.
    pub methods: Vec<String>,
    /// Allowed request headers.
    pub headers: Vec<String>,
    /// Response headers a browser script may read.
    ///
    /// `x-request-id` is included by default: a client that cannot read the
    /// correlation id cannot report a useful bug.
    pub expose_headers: Vec<String>,
    /// Whether cookies and `Authorization` are allowed.
    pub credentials: bool,
    /// How long a browser may cache the preflight result.
    pub max_age: Option<Duration>,
}

/// The methods allowed when the configuration names none.
///
/// The safe set plus the four an API actually uses. `Router` cannot tell this
/// layer which methods its table declares — the stack is composed before the
/// table is consulted — so the default is the union rather than a guess.
pub const DEFAULT_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

impl CorsConfig {
    /// Allow a fixed list of origins.
    pub fn allow_origins(origins: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            origins: origins.into_iter().map(Into::into).collect(),
            expose_headers: vec![crate::REQUEST_ID_HEADER.to_owned()],
            ..Self::default()
        }
    }

    /// Allow any origin.
    ///
    /// Legitimate for a genuinely public, unauthenticated API. Combining it
    /// with [`CorsConfig::allow_credentials`] is a **boot error**, because the
    /// browser would reject every such response anyway and the failure would
    /// otherwise surface in a client developer's console rather than in yours.
    pub fn any_origin() -> Self {
        Self {
            any_origin: true,
            expose_headers: vec![crate::REQUEST_ID_HEADER.to_owned()],
            ..Self::default()
        }
    }

    /// Allow cookies and `Authorization` on cross-origin requests.
    pub fn allow_credentials(mut self, allow: bool) -> Self {
        self.credentials = allow;
        self
    }

    /// Restrict the allowed methods.
    pub fn allow_methods(mut self, methods: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.methods = methods.into_iter().map(Into::into).collect();
        self
    }

    /// Restrict the allowed request headers.
    pub fn allow_headers(mut self, headers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.headers = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Let scripts read these response headers.
    pub fn expose_headers(mut self, headers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.expose_headers = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Cache preflight results for `max_age`.
    pub fn max_age(mut self, max_age: Duration) -> Self {
        self.max_age = Some(max_age);
        self
    }

    /// Whether the slot has anything to do.
    pub fn is_configured(&self) -> bool {
        self.any_origin || !self.origins.is_empty()
    }

    /// Check the combinations a browser will reject.
    ///
    /// Returns a boot error for `any_origin` plus credentials, and for an
    /// origin that is not a valid origin — a trailing slash or a path is the
    /// usual mistake, and it silently matches nothing.
    pub fn validate(&self) -> Vec<BootError> {
        let mut errors = Vec::new();

        if self.any_origin && self.credentials {
            errors.push(BootError::Other {
                message: "CORS allows any origin and credentials at the same time".to_owned(),
                notes: vec![
                    "a browser refuses `Access-Control-Allow-Origin: *` on a credentialed \
                     request, so every such response would be discarded client-side"
                        .to_owned(),
                ],
                fix: Some(
                    "CorsConfig::allow_origins([\"https://app.example\"]).allow_credentials(true)"
                        .to_owned(),
                ),
            });
        }

        for origin in &self.origins {
            if let Some(problem) = origin_problem(origin) {
                errors.push(BootError::Other {
                    message: format!("`{origin}` is not a valid CORS origin"),
                    notes: vec![
                        problem.to_owned(),
                        "an `Origin` header is scheme, host and port and nothing else, so a \
                         value with anything more matches no request at all"
                            .to_owned(),
                    ],
                    fix: Some(format!("\"{}\"", suggest_origin(origin))),
                });
            }
        }

        errors
    }

    /// A one-line summary for `moso middleware`.
    pub fn summary(&self) -> String {
        if !self.is_configured() {
            return "not configured".to_owned();
        }
        let mut parts = Vec::new();
        if self.any_origin {
            parts.push("origins=any".to_owned());
        } else {
            parts.push(format!("origins={}", self.origins.len()));
        }
        parts.push(format!("credentials={}", self.credentials));
        if let Some(max_age) = self.max_age {
            parts.push(format!("max_age={}", humantime::format_duration(max_age)));
        }
        parts.join(" ")
    }
}

/// Why `origin` is not a valid origin, or `None` if it is.
fn origin_problem(origin: &str) -> Option<&'static str> {
    let Some((scheme, rest)) = origin.split_once("://") else {
        return Some("an origin needs a scheme: `https://app.example`");
    };
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+')
    {
        return Some("the scheme is not a scheme");
    }
    if rest.is_empty() {
        return Some("there is no host");
    }
    if origin.ends_with('/') {
        return Some("an origin has no trailing slash");
    }
    if rest.contains('/') {
        return Some("an origin has no path");
    }
    if rest.contains('?') || rest.contains('#') {
        return Some("an origin has no query and no fragment");
    }
    None
}

/// The origin the author probably meant.
fn suggest_origin(origin: &str) -> String {
    let trimmed = origin.trim_end_matches('/');
    match trimmed.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
            format!("{scheme}://{host}")
        }
        None => format!(
            "https://{}",
            trimmed.split(['/', '?', '#']).next().unwrap_or(trimmed)
        ),
    }
}

/// Wrap `service` in `tower_http`'s CORS layer.
///
/// An unconfigured [`CorsConfig`] passes the service through untouched, so the
/// slot being present in the stack is never the same as it doing something.
#[cfg(feature = "cors")]
pub fn layer(config: &CorsConfig, service: Route) -> Route {
    use http::{HeaderName, HeaderValue, Method};
    use tower::Layer as _;
    use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer, ExposeHeaders};

    if !config.is_configured() {
        return service;
    }

    let mut cors = CorsLayer::new();

    cors = if config.any_origin {
        cors.allow_origin(AllowOrigin::any())
    } else {
        let origins: Vec<HeaderValue> = config
            .origins
            .iter()
            .filter_map(|origin| HeaderValue::try_from(origin.as_str()).ok())
            .collect();
        cors.allow_origin(AllowOrigin::list(origins))
    };

    let methods: Vec<Method> = if config.methods.is_empty() {
        DEFAULT_METHODS
            .iter()
            .filter_map(|method| Method::try_from(*method).ok())
            .collect()
    } else {
        config
            .methods
            .iter()
            .filter_map(|method| Method::try_from(method.as_str()).ok())
            .collect()
    };
    cors = cors.allow_methods(AllowMethods::list(methods));

    cors = if config.headers.is_empty() {
        // Echo whatever the preflight asked for. Narrower than `Any` in the
        // sense that matters — the response names the headers rather than
        // wildcarding them — and it does not require the author to enumerate
        // every header their own client library sends.
        cors.allow_headers(AllowHeaders::mirror_request())
    } else {
        let headers: Vec<HeaderName> = config
            .headers
            .iter()
            .filter_map(|header| HeaderName::try_from(header.as_str()).ok())
            .collect();
        cors.allow_headers(AllowHeaders::list(headers))
    };

    let exposed: Vec<HeaderName> = config
        .expose_headers
        .iter()
        .filter_map(|header| HeaderName::try_from(header.as_str()).ok())
        .collect();
    if !exposed.is_empty() {
        cors = cors.expose_headers(ExposeHeaders::list(exposed));
    }

    cors = cors.allow_credentials(config.credentials);
    if let Some(max_age) = config.max_age {
        cors = cors.max_age(max_age);
    }

    Route::new(cors.layer(service))
}

/// Pass `service` through: the `cors` feature is off, so there is no layer.
///
/// Reaching here means the slot was enabled without the feature, which
/// [`MiddlewareStack::validate`](crate::MiddlewareStack::validate) reports as a
/// boot error. The pass-through exists so that `build_unchecked` still runs.
#[cfg(not(feature = "cors"))]
pub fn layer(config: &CorsConfig, service: Route) -> Route {
    let _ = config;
    service
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_is_inert_until_configured() {
        assert!(!CorsConfig::default().is_configured());
        assert!(CorsConfig::any_origin().is_configured());
        assert!(CorsConfig::allow_origins(["https://a.example"]).is_configured());
    }

    #[test]
    fn the_request_id_is_exposed_by_default() {
        let config = CorsConfig::allow_origins(["https://a.example"]);
        assert!(
            config
                .expose_headers
                .iter()
                .any(|header| header == "x-request-id")
        );
    }

    #[test]
    fn any_origin_with_credentials_is_a_boot_error() {
        let errors = CorsConfig::any_origin().allow_credentials(true).validate();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].headline().contains("any origin and credentials"));
    }

    #[test]
    fn any_origin_without_credentials_is_fine() {
        assert!(CorsConfig::any_origin().validate().is_empty());
    }

    #[test]
    fn a_good_origin_passes() {
        assert!(
            CorsConfig::allow_origins(["https://a.example", "http://localhost:3000"])
                .validate()
                .is_empty()
        );
    }

    #[test]
    fn the_usual_origin_mistakes_are_caught_with_a_fix() {
        for (bad, fixed) in [
            ("https://a.example/", "https://a.example"),
            ("https://a.example/app", "https://a.example"),
            ("a.example", "https://a.example"),
        ] {
            let errors = CorsConfig::allow_origins([bad]).validate();
            assert_eq!(errors.len(), 1, "{bad}");
            assert!(errors[0].headline().contains(bad), "{bad}");
            assert_eq!(suggest_origin(bad), fixed);
        }
    }

    #[test]
    fn the_summary_says_what_is_allowed() {
        assert_eq!(CorsConfig::default().summary(), "not configured");
        assert_eq!(
            CorsConfig::allow_origins(["https://a.example"])
                .allow_credentials(true)
                .max_age(Duration::from_secs(600))
                .summary(),
            "origins=1 credentials=true max_age=10m"
        );
        assert!(CorsConfig::any_origin().summary().contains("origins=any"));
    }

    #[cfg(feature = "cors")]
    #[tokio::test]
    async fn a_configured_layer_answers_a_preflight() {
        use crate::{Request, Response};
        use std::convert::Infallible;
        use tower::ServiceExt as _;

        let inner = Route::new(tower::service_fn(|_req: Request| async {
            Ok::<_, Infallible>(Response::new(axum::body::Body::empty()))
        }));
        let service = layer(&CorsConfig::allow_origins(["https://a.example"]), inner);

        let request = http::Request::builder()
            .method(http::Method::OPTIONS)
            .header(http::header::ORIGIN, "https://a.example")
            .header("access-control-request-method", "POST")
            .body(axum::body::Body::empty())
            .expect("request");

        let response = service.oneshot(request).await.expect("infallible");
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://a.example")
        );
    }

    #[cfg(feature = "cors")]
    #[tokio::test]
    async fn an_unconfigured_layer_is_a_pass_through() {
        use crate::{Request, Response};
        use std::convert::Infallible;
        use tower::ServiceExt as _;

        let inner = Route::new(tower::service_fn(|_req: Request| async {
            Ok::<_, Infallible>(Response::new(axum::body::Body::empty()))
        }));
        let response = layer(&CorsConfig::default(), inner)
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );
    }
}
