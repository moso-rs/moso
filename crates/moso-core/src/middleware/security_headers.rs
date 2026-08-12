//! `security_headers` — the headers every API should send, on by default.
//!
//! Defaults that are safe for an API and do not break a browser client:
//!
//! | Header | Default |
//! | --- | --- |
//! | `Strict-Transport-Security` | `max-age=63072000; includeSubDomains` |
//! | `X-Content-Type-Options` | `nosniff` |
//! | `Referrer-Policy` | `strict-origin-when-cross-origin` |
//! | `Content-Security-Policy` | `frame-ancestors 'none'` |
//! | `X-Frame-Options` | `DENY` |
//!
//! The CSP default is deliberately minimal. A full `default-src 'self'` breaks
//! any page that loads anything, so shipping one by default would train people
//! to turn the whole layer off. `frame-ancestors 'none'` is the clickjacking
//! protection, applies to APIs and pages alike, and breaks nothing.
//! `X-Frame-Options: DENY` says the same thing to a browser too old to honour
//! the CSP directive; it costs 24 bytes and cannot conflict with it.
//!
//! There is **no** default `Permissions-Policy`. A deny-all policy is correct
//! for an API and silently breaks any page that asks for a camera or a
//! location, so it is one call away ([`SecurityHeadersConfig::permissions_policy`])
//! rather than a default that teaches people to disable the layer.
//!
//! `preload` is **not** in the default HSTS value. Submitting a domain to the
//! preload list is close to irreversible and is not a framework's decision.
//!
//! Every value is a pre-built `HeaderValue`, so the layer allocates nothing per
//! request: [`SecurityHeadersConfig::headers`] runs once at boot and the
//! service clones refcounted `Bytes` on the way out.

use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{HeaderName, HeaderValue};
use tower::Service;

use crate::router::Route;
use crate::{BoxFuture, Request, Response};

/// The `Referrer-Policy` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReferrerPolicy {
    /// Send the origin cross-origin, the full URL same-origin, nothing on a
    /// downgrade. The default, and the one that leaks least while still
    /// letting analytics work.
    #[default]
    StrictOriginWhenCrossOrigin,
    /// Never send a referrer.
    NoReferrer,
    /// Send the origin only, always.
    Origin,
    /// Same-origin requests only.
    SameOrigin,
    /// Send everything. Almost never right.
    UnsafeUrl,
}

impl ReferrerPolicy {
    /// The header value.
    pub const fn as_str(self) -> &'static str {
        match self {
            ReferrerPolicy::StrictOriginWhenCrossOrigin => "strict-origin-when-cross-origin",
            ReferrerPolicy::NoReferrer => "no-referrer",
            ReferrerPolicy::Origin => "origin",
            ReferrerPolicy::SameOrigin => "same-origin",
            ReferrerPolicy::UnsafeUrl => "unsafe-url",
        }
    }
}

/// The `X-Frame-Options` value shipped by default.
pub const DEFAULT_FRAME_OPTIONS: &str = "DENY";

/// A deny-all `Permissions-Policy`, for an API that serves no pages.
///
/// Not a default — see the module header — but the value most people want when
/// they go looking for one.
pub const DENY_ALL_PERMISSIONS_POLICY: &str = "accelerometer=(), camera=(), geolocation=(), gyroscope=(), magnetometer=(), \
     microphone=(), payment=(), usb=()";

/// How the `security_headers` slot behaves.
#[derive(Debug, Clone)]
pub struct SecurityHeadersConfig {
    /// `Strict-Transport-Security` max-age, in seconds. `None` omits it.
    ///
    /// Omitted automatically in the `dev` profile: a two-year HSTS pin on
    /// `localhost` makes every other local project HTTPS-only.
    pub hsts_max_age: Option<u64>,
    /// Whether HSTS covers subdomains.
    pub hsts_include_subdomains: bool,
    /// Whether to add `preload`. Off, and deliberately hard to turn on.
    pub hsts_preload: bool,
    /// Send `X-Content-Type-Options: nosniff`.
    pub nosniff: bool,
    /// The referrer policy.
    pub referrer_policy: Option<ReferrerPolicy>,
    /// The `Content-Security-Policy` value.
    pub csp: Option<String>,
    /// Extra headers to set on every response.
    ///
    /// The last entry for a given name wins, so overriding a default is
    /// [`SecurityHeadersConfig::header`] with the same name.
    pub extra: Vec<(&'static str, String)>,
}

impl Default for SecurityHeadersConfig {
    fn default() -> Self {
        Self {
            hsts_max_age: Some(63_072_000),
            hsts_include_subdomains: true,
            hsts_preload: false,
            nosniff: true,
            referrer_policy: Some(ReferrerPolicy::default()),
            csp: Some("frame-ancestors 'none'".to_owned()),
            extra: vec![("x-frame-options", DEFAULT_FRAME_OPTIONS.to_owned())],
        }
    }
}

impl SecurityHeadersConfig {
    /// The configuration for a profile. `dev` omits HSTS.
    pub fn for_profile(profile: crate::config::Profile) -> Self {
        let mut config = Self::default();
        if profile == crate::config::Profile::Dev {
            config.hsts_max_age = None;
        }
        config
    }

    /// Set the `Content-Security-Policy`.
    pub fn csp(&mut self, policy: impl Into<String>) -> &mut Self {
        self.csp = Some(policy.into());
        self
    }

    /// Remove the `Content-Security-Policy`.
    pub fn no_csp(&mut self) -> &mut Self {
        self.csp = None;
        self
    }

    /// Set the HSTS max-age.
    pub fn hsts(&mut self, max_age: std::time::Duration) -> &mut Self {
        self.hsts_max_age = Some(max_age.as_secs());
        self
    }

    /// Remove HSTS.
    pub fn no_hsts(&mut self) -> &mut Self {
        self.hsts_max_age = None;
        self
    }

    /// Set the referrer policy.
    pub fn referrer_policy(&mut self, policy: ReferrerPolicy) -> &mut Self {
        self.referrer_policy = Some(policy);
        self
    }

    /// Set `X-Frame-Options`. `DENY` by default.
    pub fn frame_options(&mut self, value: impl Into<String>) -> &mut Self {
        self.header("x-frame-options", value)
    }

    /// Set a `Permissions-Policy`.
    ///
    /// [`DENY_ALL_PERMISSIONS_POLICY`] is the value an API wants.
    pub fn permissions_policy(&mut self, value: impl Into<String>) -> &mut Self {
        self.header("permissions-policy", value)
    }

    /// Set an arbitrary header on every response.
    pub fn header(&mut self, name: &'static str, value: impl Into<String>) -> &mut Self {
        self.extra.push((name, value.into()));
        self
    }

    /// The headers this configuration produces, built once at boot.
    ///
    /// A member that cannot be rendered as a valid header value is dropped with
    /// a warning rather than failing boot or panicking per request: a bad CSP
    /// string is a typo, and refusing to start over one would be a worse
    /// failure than sending one header fewer and saying so.
    pub fn headers(&self) -> Vec<(HeaderName, HeaderValue)> {
        let mut headers = Vec::with_capacity(5 + self.extra.len());

        if let Some(max_age) = self.hsts_max_age {
            let mut value = format!("max-age={max_age}");
            if self.hsts_include_subdomains {
                value.push_str("; includeSubDomains");
            }
            if self.hsts_preload {
                value.push_str("; preload");
            }
            push(&mut headers, "strict-transport-security", &value);
        }
        if self.nosniff {
            push(&mut headers, "x-content-type-options", "nosniff");
        }
        if let Some(policy) = self.referrer_policy {
            push(&mut headers, "referrer-policy", policy.as_str());
        }
        if let Some(csp) = &self.csp {
            push(&mut headers, "content-security-policy", csp);
        }
        for (name, value) in &self.extra {
            push(&mut headers, name, value);
        }
        headers
    }

    /// A one-line summary for `moso middleware`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(max_age) = self.hsts_max_age {
            parts.push(format!("hsts={max_age}"));
        }
        if self.nosniff {
            parts.push("nosniff".to_owned());
        }
        if let Some(csp) = &self.csp {
            parts.push(format!("csp=\"{csp}\""));
        }
        if !self.extra.is_empty() {
            parts.push(format!("extra={}", self.extra.len()));
        }
        parts.join(" ")
    }
}

/// Append one header, dropping it with a warning if it cannot be represented.
fn push(headers: &mut Vec<(HeaderName, HeaderValue)>, name: &str, value: &str) {
    match (HeaderName::try_from(name), HeaderValue::try_from(value)) {
        (Ok(name), Ok(value)) => headers.push((name, value)),
        _ => tracing::warn!(
            header = %name,
            "not a valid header; it will not be sent by `security_headers`"
        ),
    }
}

/// Wrap `service` so every response carries the security headers.
pub fn layer(config: &SecurityHeadersConfig, service: Route) -> Route {
    Route::new(SecurityHeaders {
        inner: service,
        headers: Arc::from(config.headers()),
    })
}

/// The service [`layer`] builds.
#[derive(Clone)]
struct SecurityHeaders {
    inner: Route,
    headers: Arc<[(HeaderName, HeaderValue)]>,
}

impl Service<Request> for SecurityHeaders {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);
        let headers = Arc::clone(&self.headers);

        Box::pin(async move {
            let mut response = inner.call(req).await?;
            let map = response.headers_mut();
            for (name, value) in headers.iter() {
                // `insert`, not `append`: a handler that set its own CSP for
                // one response should not end up sending two conflicting ones.
                // A deliberate per-response override therefore has to happen
                // outside this layer, which is what `Router::layer` is for.
                map.insert(name.clone(), value.clone());
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;
    use tower::ServiceExt as _;

    fn map(config: &SecurityHeadersConfig) -> std::collections::HashMap<String, String> {
        config
            .headers()
            .into_iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().expect("ascii").to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn defaults_are_safe_for_an_api() {
        let config = SecurityHeadersConfig::default();
        assert_eq!(config.hsts_max_age, Some(63_072_000));
        assert!(config.hsts_include_subdomains);
        assert!(!config.hsts_preload);
        assert!(config.nosniff);
        assert_eq!(config.csp.as_deref(), Some("frame-ancestors 'none'"));
    }

    #[test]
    fn referrer_policy_renders_the_spelling_browsers_expect() {
        assert_eq!(
            ReferrerPolicy::default().as_str(),
            "strict-origin-when-cross-origin"
        );
    }

    #[test]
    fn the_default_headers_are_the_documented_table() {
        let headers = map(&SecurityHeadersConfig::default());
        assert_eq!(
            headers["strict-transport-security"],
            "max-age=63072000; includeSubDomains"
        );
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(
            headers["referrer-policy"],
            "strict-origin-when-cross-origin"
        );
        assert_eq!(headers["content-security-policy"], "frame-ancestors 'none'");
        assert_eq!(headers["x-frame-options"], "DENY");
        // No `Permissions-Policy` by default: it breaks pages.
        assert!(!headers.contains_key("permissions-policy"));
    }

    #[test]
    fn dev_omits_hsts_and_keeps_everything_else() {
        let dev = SecurityHeadersConfig::for_profile(Profile::Dev);
        assert!(dev.hsts_max_age.is_none());
        assert!(!map(&dev).contains_key("strict-transport-security"));
        assert_eq!(map(&dev)["x-content-type-options"], "nosniff");

        for profile in [Profile::Test, Profile::Production] {
            let config = SecurityHeadersConfig::for_profile(profile);
            assert_eq!(config.hsts_max_age, Some(63_072_000));
        }
    }

    #[test]
    fn preload_and_subdomains_are_rendered_in_order() {
        let mut config = SecurityHeadersConfig {
            hsts_preload: true,
            ..SecurityHeadersConfig::default()
        };
        assert_eq!(
            map(&config)["strict-transport-security"],
            "max-age=63072000; includeSubDomains; preload"
        );

        config.hsts_include_subdomains = false;
        assert_eq!(
            map(&config)["strict-transport-security"],
            "max-age=63072000; preload"
        );
    }

    #[test]
    fn turning_a_header_off_removes_it() {
        let mut config = SecurityHeadersConfig::default();
        config.no_csp().no_hsts();
        config.nosniff = false;
        config.referrer_policy = None;
        let headers = map(&config);
        assert!(!headers.contains_key("content-security-policy"));
        assert!(!headers.contains_key("strict-transport-security"));
        assert!(!headers.contains_key("x-content-type-options"));
        assert!(!headers.contains_key("referrer-policy"));
        // The extras survive, because they were asked for by name.
        assert_eq!(headers["x-frame-options"], "DENY");
    }

    #[test]
    fn an_extra_header_overrides_a_default_of_the_same_name() {
        let mut config = SecurityHeadersConfig::default();
        config.frame_options("SAMEORIGIN");
        config.permissions_policy(DENY_ALL_PERMISSIONS_POLICY);

        // Both entries are present, in order, and the layer's `insert` makes
        // the last one win.
        let rendered = config.headers();
        let last = rendered
            .iter()
            .rfind(|(name, _)| name.as_str() == "x-frame-options")
            .expect("entry");
        assert_eq!(last.1, "SAMEORIGIN");
        assert!(map(&config)["permissions-policy"].contains("camera=()"));
    }

    #[test]
    fn an_unrepresentable_value_is_dropped_rather_than_fatal() {
        let mut config = SecurityHeadersConfig::default();
        config.csp("bad\nvalue");
        assert!(!map(&config).contains_key("content-security-policy"));
    }

    #[test]
    fn the_summary_names_the_visible_settings() {
        let summary = SecurityHeadersConfig::default().summary();
        assert!(summary.contains("hsts=63072000"));
        assert!(summary.contains("csp=\"frame-ancestors 'none'\""));
    }

    #[tokio::test]
    async fn every_response_carries_the_headers_exactly_once() {
        let inner = Route::new(tower::service_fn(|_req: Request| async {
            let mut response = Response::new(axum::body::Body::empty());
            // A handler that set its own value does not get two.
            response
                .headers_mut()
                .insert("x-frame-options", HeaderValue::from_static("ALLOWALL"));
            Ok::<_, Infallible>(response)
        }));

        let response = layer(&SecurityHeadersConfig::default(), inner)
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");

        assert_eq!(
            response.headers().get_all("x-frame-options").iter().count(),
            1
        );
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    }
}
