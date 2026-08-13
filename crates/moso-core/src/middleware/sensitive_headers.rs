//! `sensitive_headers` — mark the headers that must never be printed.
//!
//! Marking a [`HeaderValue`](http::HeaderValue) sensitive is not decoration. It
//! changes two things:
//!
//! - `hyper` stops indexing the header in HPACK/QPACK, so an `Authorization`
//!   value never enters a per-connection compression table an attacker can
//!   probe with a CRIME-style oracle;
//! - every `Debug` rendering of the header map prints `Sensitive` instead of
//!   the value, so a header dump in a log or an error page cannot leak one.
//!
//! The list comes from [`REDACTED_HEADERS`], which is the same list the error
//! logger and `Headers::describe` use. One list, three consumers, no drift.
//!
//! This is one of the three slots that is a `tower-http` layer verbatim
//! (`SetSensitiveRequestHeadersLayer` + `SetSensitiveResponseHeadersLayer`):
//! it is exactly the behaviour Moso wants, it changes no body type, and
//! reimplementing it would only add a place to be wrong.
//!
//! [`REDACTED_HEADERS`]: crate::extract::headers::REDACTED_HEADERS

use std::sync::Arc;

use http::HeaderName;
use tower::Layer as _;
use tower_http::sensitive_headers::{
    SetSensitiveRequestHeadersLayer, SetSensitiveResponseHeadersLayer,
};

use crate::extract::headers::REDACTED_HEADERS;
use crate::router::Route;

/// How the `sensitive_headers` slot behaves.
#[derive(Debug, Clone)]
pub struct SensitiveHeadersConfig {
    /// The header names marked sensitive, on the request and the response.
    ///
    /// Defaults to [`REDACTED_HEADERS`]. Names that are not valid header names
    /// are dropped when the layer is built, with a warning, rather than
    /// failing boot: a typo in a redaction list should not take the process
    /// down, and it must not silently widen what is printed either.
    pub names: Vec<String>,
}

impl Default for SensitiveHeadersConfig {
    fn default() -> Self {
        Self {
            names: REDACTED_HEADERS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        }
    }
}

impl SensitiveHeadersConfig {
    /// Also mark `name` sensitive.
    pub fn add(&mut self, name: impl Into<String>) -> &mut Self {
        self.names.push(name.into());
        self
    }

    /// Replace the list wholesale.
    ///
    /// Removing a default is a decision worth making deliberately, which is why
    /// it takes a whole list rather than a `remove`.
    pub fn set(&mut self, names: impl IntoIterator<Item = impl Into<String>>) -> &mut Self {
        self.names = names.into_iter().map(Into::into).collect();
        self
    }

    /// The parsed header names, in order, with the unparseable ones dropped.
    pub fn header_names(&self) -> Vec<HeaderName> {
        self.names
            .iter()
            .filter_map(|name| match HeaderName::try_from(name.as_str()) {
                Ok(name) => Some(name),
                Err(_) => {
                    tracing::warn!(
                        header = %name,
                        "not a valid header name; it will not be marked sensitive"
                    );
                    None
                }
            })
            .collect()
    }

    /// A one-line summary for `moso middleware`.
    pub fn summary(&self) -> String {
        self.names.join(", ")
    }
}

/// Wrap `service` so the configured headers are marked sensitive both ways.
pub fn layer(config: &SensitiveHeadersConfig, service: Route) -> Route {
    let names: Arc<[HeaderName]> = Arc::from(config.header_names());
    let responses =
        SetSensitiveResponseHeadersLayer::from_shared(Arc::clone(&names)).layer(service);
    Route::new(SetSensitiveRequestHeadersLayer::from_shared(names).layer(responses))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Request, Response};
    use std::convert::Infallible;
    use tower::ServiceExt as _;

    #[test]
    fn the_default_list_is_the_shared_one() {
        let config = SensitiveHeadersConfig::default();
        assert_eq!(config.names.len(), REDACTED_HEADERS.len());
        assert!(config.names.iter().any(|name| name == "authorization"));
        assert!(config.summary().contains("cookie"));
    }

    #[test]
    fn an_invalid_name_is_dropped_rather_than_fatal() {
        let mut config = SensitiveHeadersConfig::default();
        config.set(["authorization", "not a header"]);
        let names = config.header_names();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].as_str(), "authorization");
    }

    #[tokio::test]
    async fn the_request_and_the_response_are_both_marked() {
        let inner = Route::new(tower::service_fn(|req: Request| async move {
            // The request header was marked on the way in.
            assert!(
                req.headers()
                    .get(http::header::AUTHORIZATION)
                    .expect("header")
                    .is_sensitive()
            );
            let mut response = Response::new(axum::body::Body::empty());
            response.headers_mut().insert(
                http::header::SET_COOKIE,
                http::HeaderValue::from_static("session=abc"),
            );
            Ok::<_, Infallible>(response)
        }));

        let request = http::Request::builder()
            .header(http::header::AUTHORIZATION, "Bearer hunter2")
            .body(axum::body::Body::empty())
            .expect("request");

        let response = layer(&SensitiveHeadersConfig::default(), inner)
            .oneshot(request)
            .await
            .expect("infallible");

        let cookie = response
            .headers()
            .get(http::header::SET_COOKIE)
            .expect("header");
        assert!(cookie.is_sensitive());
        // …and a header dump of it cannot print the value.
        assert!(!format!("{:?}", response.headers()).contains("session=abc"));
    }
}
