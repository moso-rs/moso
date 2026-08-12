//! `normalize_path` — decide the trailing-slash question once.
//!
//! `/users` and `/users/` are different paths to a router and the same resource
//! to a user. Serving both doubles a cache's work and splits every metric.
//! Redirecting is the correct behaviour for a browser and an annoyance for an
//! API client that does not follow redirects on `POST`, so the default rewrites
//! internally instead: the request is matched as if the slash were not there,
//! and no redirect is sent.
//!
//! # One rule, one function
//!
//! Every policy is expressed through [`normalize`], and the layer only decides
//! what to *do* with the answer — rewrite the URI, or answer 308. `moso check`
//! and the router's conflict report call the same function, so what the layer
//! does and what the tooling predicts cannot drift.
//!
//! `tower_http::normalize_path::NormalizePathLayer` covers two of the four
//! policies (trim and append) and neither the redirect nor the repeated-slash
//! case; splitting the slot across two implementations would put the rule in
//! two places, which is the failure this module exists to avoid.

use std::convert::Infallible;
use std::task::{Context, Poll};

use http::uri::PathAndQuery;
use http::{HeaderValue, StatusCode, Uri};
use tower::Service;

use crate::router::Route;
use crate::{BoxFuture, Request, Response};

/// The trailing-slash policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrailingSlash {
    /// Strip a trailing slash before routing, without a redirect.
    ///
    /// The default. Both spellings work, one route table entry serves them, and
    /// no client has to handle a 308 on a `POST`.
    #[default]
    Trim,
    /// Add a trailing slash before routing, without a redirect.
    ///
    /// For an application whose route table is written with trailing slashes.
    Append,
    /// Redirect to the canonical spelling with a 308.
    ///
    /// Correct for a browser-facing application where the URL in the address
    /// bar matters. A 308 preserves the method, so a `POST` is not silently
    /// downgraded — which is the bug a 301 would introduce.
    Redirect,
    /// Treat the two spellings as different paths.
    Off,
}

/// How the `normalize_path` slot behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NormalizePathConfig {
    /// The trailing-slash policy.
    pub trailing_slash: TrailingSlash,
    /// Whether to collapse repeated slashes: `/a//b` becomes `/a/b`.
    ///
    /// Off by default. A doubled slash is usually a client bug worth surfacing
    /// as a 404, and silently repairing it hides a broken URL template.
    pub collapse_slashes: bool,
}

impl NormalizePathConfig {
    /// The policy that redirects to the canonical spelling.
    pub fn redirect() -> Self {
        Self {
            trailing_slash: TrailingSlash::Redirect,
            collapse_slashes: false,
        }
    }

    /// Disable normalisation entirely.
    pub fn off() -> Self {
        Self {
            trailing_slash: TrailingSlash::Off,
            collapse_slashes: false,
        }
    }

    /// The canonical spelling of `path` under this configuration, or `None`
    /// when it is already canonical.
    pub fn canonical(&self, path: &str) -> Option<String> {
        let collapsed = if self.collapse_slashes {
            collapse(path)
        } else {
            None
        };
        let candidate = collapsed.as_deref().unwrap_or(path);
        match normalize(candidate, self.trailing_slash) {
            Some(normalised) => Some(normalised),
            None => collapsed,
        }
    }

    /// A one-line summary for `moso middleware`.
    pub fn summary(&self) -> String {
        let policy = match self.trailing_slash {
            TrailingSlash::Trim => "trim_trailing_slash",
            TrailingSlash::Append => "append_trailing_slash",
            TrailingSlash::Redirect => "redirect_to_canonical",
            TrailingSlash::Off => "off",
        };
        if self.collapse_slashes {
            format!("{policy} collapse_slashes")
        } else {
            policy.to_owned()
        }
    }
}

/// Apply the policy to a path, returning `None` when it is already canonical.
///
/// The root path is never rewritten: trimming `/` would produce an empty path,
/// which matches nothing.
pub fn normalize(path: &str, policy: TrailingSlash) -> Option<String> {
    match policy {
        TrailingSlash::Off => None,
        TrailingSlash::Trim | TrailingSlash::Redirect => {
            let trimmed = path.trim_end_matches('/');
            // `""` from a path of `/` — or of `///`, which is the same request
            // written by a broken URL template.
            let trimmed = if trimmed.is_empty() { "/" } else { trimmed };
            (trimmed != path).then(|| trimmed.to_owned())
        }
        TrailingSlash::Append => (!path.ends_with('/')).then(|| format!("{path}/")),
    }
}

/// Collapse repeated slashes, returning `None` when there are none.
pub fn collapse(path: &str) -> Option<String> {
    if !path.contains("//") {
        return None;
    }
    let mut out = String::with_capacity(path.len());
    let mut previous_was_slash = false;
    for character in path.chars() {
        if character == '/' {
            if previous_was_slash {
                continue;
            }
            previous_was_slash = true;
        } else {
            previous_was_slash = false;
        }
        out.push(character);
    }
    Some(out)
}

/// Wrap `service` in the trailing-slash policy.
pub fn layer(config: &NormalizePathConfig, service: Route) -> Route {
    Route::new(NormalizePath {
        inner: service,
        config: *config,
    })
}

/// The service [`layer`] builds.
#[derive(Clone)]
struct NormalizePath {
    inner: Route,
    config: NormalizePathConfig,
}

impl Service<Request> for NormalizePath {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);

        if let Some(canonical) = self.config.canonical(req.uri().path())
            && let Some(uri) = with_path(req.uri(), &canonical)
        {
            if self.config.trailing_slash == TrailingSlash::Redirect {
                let response = redirect(&uri);
                return Box::pin(async move { Ok(response) });
            }
            *req.uri_mut() = uri;
        }

        Box::pin(async move { inner.call(req).await })
    }
}

/// `uri` with its path replaced, keeping the query.
///
/// `None` when the result is not a valid URI, in which case the request is
/// passed through untouched — a normalisation that cannot be expressed is not
/// worth failing a request over.
fn with_path(uri: &Uri, path: &str) -> Option<Uri> {
    let path_and_query = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(PathAndQuery::try_from(path_and_query).ok()?);
    Uri::from_parts(parts).ok()
}

/// A 308 to `uri`, which preserves the method.
fn redirect(uri: &Uri) -> Response {
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::PERMANENT_REDIRECT;
    // A path built from a valid `Uri` is always a valid header value; the
    // fallback exists so this function cannot panic on the error path.
    if let Ok(location) = HeaderValue::try_from(
        uri.path_and_query()
            .map_or_else(|| uri.path().to_owned(), ToString::to_string),
    ) {
        response
            .headers_mut()
            .insert(http::header::LOCATION, location);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt as _;

    #[test]
    fn the_default_trims_without_redirecting() {
        let config = NormalizePathConfig::default();
        assert_eq!(config.trailing_slash, TrailingSlash::Trim);
        assert!(!config.collapse_slashes);
        assert_eq!(config.summary(), "trim_trailing_slash");
    }

    #[test]
    fn trimming_leaves_the_root_alone() {
        assert_eq!(
            normalize("/users/", TrailingSlash::Trim).as_deref(),
            Some("/users")
        );
        assert_eq!(normalize("/users", TrailingSlash::Trim), None);
        assert_eq!(normalize("/", TrailingSlash::Trim), None);
        // Several slashes are one slash's worth of mistake.
        assert_eq!(
            normalize("/users///", TrailingSlash::Trim).as_deref(),
            Some("/users")
        );
        assert_eq!(normalize("///", TrailingSlash::Trim).as_deref(), Some("/"));
    }

    #[test]
    fn appending_is_the_mirror_image() {
        assert_eq!(
            normalize("/users", TrailingSlash::Append).as_deref(),
            Some("/users/")
        );
        assert_eq!(normalize("/users/", TrailingSlash::Append), None);
        assert_eq!(normalize("/", TrailingSlash::Append), None);
    }

    #[test]
    fn off_never_rewrites() {
        assert_eq!(normalize("/users/", TrailingSlash::Off), None);
        assert_eq!(normalize("/a//b", TrailingSlash::Off), None);
    }

    #[test]
    fn redirect_canonicalises_the_same_way_trim_does() {
        assert_eq!(
            normalize("/users/", TrailingSlash::Redirect).as_deref(),
            Some("/users")
        );
    }

    #[test]
    fn collapsing_is_opt_in_and_idempotent() {
        assert_eq!(collapse("/a//b").as_deref(), Some("/a/b"));
        assert_eq!(collapse("/a///b//c").as_deref(), Some("/a/b/c"));
        assert_eq!(collapse("/a/b"), None);

        let config = NormalizePathConfig {
            trailing_slash: TrailingSlash::Trim,
            collapse_slashes: true,
        };
        assert_eq!(config.canonical("/a//b/").as_deref(), Some("/a/b"));
        assert_eq!(config.canonical("/a//b").as_deref(), Some("/a/b"));
        assert_eq!(config.canonical("/a/b"), None);
    }

    /// Echoes the path the inner service was actually asked for.
    fn echo_path() -> Route {
        Route::new(tower::service_fn(|req: Request| async move {
            let path = req
                .uri()
                .path_and_query()
                .map_or_else(|| req.uri().path().to_owned(), ToString::to_string);
            Ok::<_, Infallible>(Response::new(axum::body::Body::from(path)))
        }))
    }

    async fn seen(config: NormalizePathConfig, uri: &str) -> (StatusCode, String, Option<String>) {
        use http_body_util::BodyExt as _;

        let request = http::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .expect("request");
        let response = layer(&config, echo_path())
            .oneshot(request)
            .await
            .expect("infallible");

        let status = response.status();
        let location = response
            .headers()
            .get(http::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .expect("body")
                .to_bytes()
                .to_vec(),
        )
        .expect("utf-8");
        (status, body, location)
    }

    #[tokio::test]
    async fn trimming_rewrites_the_request_and_keeps_the_query() {
        let (status, path, location) = seen(NormalizePathConfig::default(), "/users/?page=2").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(path, "/users?page=2");
        assert!(location.is_none());
    }

    #[tokio::test]
    async fn redirecting_answers_308_with_a_location() {
        let (status, _, location) = seen(NormalizePathConfig::redirect(), "/users/?page=2").await;
        assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
        assert_eq!(location.as_deref(), Some("/users?page=2"));
    }

    #[tokio::test]
    async fn a_canonical_path_is_passed_straight_through() {
        let (status, path, _) = seen(NormalizePathConfig::redirect(), "/users").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(path, "/users");
    }

    #[tokio::test]
    async fn off_leaves_both_spellings_alone() {
        let (_, path, _) = seen(NormalizePathConfig::off(), "/users/").await;
        assert_eq!(path, "/users/");
    }
}
