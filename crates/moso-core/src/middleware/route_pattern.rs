//! `route_pattern` — answer "which route is this?" before routing runs.
//!
//! Three things in the stack want the matched route **pattern** and not the raw
//! path: [`Slot::Trace`]'s `route` span field, [`Slot::Metrics`]'s `route`
//! label, and [`Slot::Timeout`]'s exemption list. Axum publishes the pattern as
//! a request extension *during* routing, and the composed stack is installed
//! outside routing, so for a long time all three read `<unmatched>` — one dead
//! span field, one useless metric label, and a `timeout_exempt` list that never
//! matched anything.
//!
//! # Why the stack is not simply moved inside routing
//!
//! The obvious repair is to install the composed service with
//! `axum::Router::layer`, which applies it *inside* routing. It does not work,
//! and the reason is the slot order itself:
//!
//! ```text
//! outermost ─┐  catch_panic
//!            │  request_id
//!            │  trace            ← wants the pattern
//!            │  …
//!            │  timeout          ← wants the pattern
//!            │  …
//!            │  normalize_path   ← REWRITES THE URI, so it is inert unless it
//!            │                     runs BEFORE matching: `/users/` has already
//!            │                     failed to match `/users` by then
//!            │  …
//! innermost ─┘  metrics          ← wants the pattern
//! ```
//!
//! [`Slot::NormalizePath`] has to run before matching or it does nothing at all,
//! and [`Slot::Timeout`] and [`Slot::Trace`] sit *outside* it, so neither can be
//! moved inside routing without reordering the stack. Moving the whole stack in
//! would break normalisation; moving three slots in would break the ordering
//! rules [`MiddlewareStack::validate`] enforces.
//!
//! # What happens instead
//!
//! The route table is known at boot, so the pattern can be resolved at the very
//! outside of the stack and published as a [`ResolvedRoute`] extension. One
//! lookup per request, and [`Trace`], [`Timeout`], [`CatchError`] and
//! [`Metrics`] all read the same answer, through the one crate-internal
//! `matched_route` helper that prefers Axum's own `MatchedPath` whenever a layer
//! is inside routing and can see it.
//!
//! # Why `matchit` and not a hand-written matcher
//!
//! Axum routes by inserting every registered path into a [`matchit::Router`] and
//! calling `at(uri.path())`. [`RoutePatterns`] does exactly that, with the same
//! crate at the same version, over the same strings, in the same registration
//! order. Anything else — a segment-wise comparison, a regex — would disagree
//! with the router in precisely the backtracking cases that make routing
//! interesting (`/users/me` against `/users/{id}`), and a *wrong* `route` label
//! is worse than a coarse one: it attributes latency to the wrong endpoint and
//! it can hand an attacker a timeout exemption.
//!
//! # What deliberately does not resolve
//!
//! | Request | Resolves to | Why |
//! | --- | --- | --- |
//! | A registered Moso route | its pattern | the point of the module |
//! | A path matching nothing | `<unmatched>` | one bounded series, never the path |
//! | A [`Router::mount_axum`] route | `<unmatched>` | Moso cannot see those patterns, and they are absent from the OpenAPI document for the same reason |
//! | A [`Router::static_files`] mount | `<unmatched>` | same: a mounted service, not a route table |
//! | A request a redirecting normaliser will answer with a 308 | `<unmatched>` | it never reaches the route table |
//!
//! Never the raw path, under any of them. A label built from a path is the
//! cardinality explosion the `route` label exists to prevent.
//!
//! # This layer is outside `catch_panic`
//!
//! It wraps the whole composed stack, because [`Slot::Trace`] is near the
//! outside of it and has to see the extension. That is only safe because it
//! cannot panic: it calls [`NormalizePathConfig::canonical`],
//! [`matchit::Router::at`] and `Extensions::insert`, all total, with no
//! indexing, no `unwrap` and no arithmetic.
//!
//! [`Slot::Trace`]: crate::middleware::Slot::Trace
//! [`Slot::Metrics`]: crate::middleware::Slot::Metrics
//! [`Slot::Timeout`]: crate::middleware::Slot::Timeout
//! [`Slot::NormalizePath`]: crate::middleware::Slot::NormalizePath
//! [`MiddlewareStack::validate`]: crate::middleware::MiddlewareStack::validate
//! [`Trace`]: crate::middleware::trace
//! [`Timeout`]: crate::middleware::timeout
//! [`CatchError`]: crate::middleware::catch_error
//! [`Metrics`]: crate::middleware::metrics
//! [`Router::mount_axum`]: crate::Router::mount_axum
//! [`Router::static_files`]: crate::Router::static_files

use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::Service;

use crate::middleware::normalize_path::{NormalizePathConfig, TrailingSlash};
use crate::router::{Route, Router};
use crate::{BoxFuture, Request, Response};

// ---------------------------------------------------------------------------
// ResolvedRoute
// ---------------------------------------------------------------------------

/// The route pattern the stack resolved for this request.
///
/// A request extension, inserted by [`layer`] at the outside of the composed
/// stack and read back by every slot that wants the pattern. It is always one of
/// the patterns the application registered — never a raw path, and never a value
/// a client can influence — which is what keeps it usable as a metric label and
/// as a timeout exemption key.
///
/// There is deliberately no public constructor. Forging one would forge a
/// `route` label and, worse, a timeout exemption; the only thing that inserts
/// one is the layer that resolved it against the boot-frozen route table.
/// Inside routing Axum's own `MatchedPath` wins, so this never overrides the
/// router's own answer.
#[derive(Debug, Clone)]
pub struct ResolvedRoute(Arc<str>);

impl ResolvedRoute {
    /// The pattern, `/users/{id}`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// RoutePatterns
// ---------------------------------------------------------------------------

/// The route table, reduced to the one question the stack asks of it.
///
/// Built once at boot from the paths the application registered, and queried
/// once per request. Patterns are stored behind an [`Arc<str>`](std::sync::Arc)
/// so that publishing one costs a refcount bump rather than an allocation.
///
/// ```
/// use moso_core::middleware::RoutePatterns;
///
/// let patterns = RoutePatterns::new(["/users", "/users/{id}", "/files/{*rest}"]);
///
/// // The pattern, never the path — that is the whole contract.
/// assert_eq!(patterns.resolve("/users/42"), Some("/users/{id}"));
/// assert_eq!(patterns.resolve("/files/a/b.txt"), Some("/files/{*rest}"));
/// assert_eq!(patterns.resolve("/nothing/here"), None);
/// ```
///
/// # Conflicting patterns
///
/// Two paths that `matchit` cannot tell apart — `/users/{id}` and
/// `/users/{user_id}` — are a boot error reported by
/// [`Router::conflicts`](crate::Router::conflicts), and
/// [`Router::into_axum`](crate::Router::into_axum) resolves them
/// first-registration-wins rather than panicking. This does the same, by
/// inserting in registration order and dropping a pattern the table already
/// covers, so the label it reports is the path Axum actually registered.
pub struct RoutePatterns {
    /// The same radix tree Axum routes with, over the same strings.
    table: matchit::Router<Arc<str>>,
    /// How many patterns the table actually accepted.
    count: usize,
    /// The trailing-slash policy that will run before routing, if any.
    normalize: Option<NormalizePathConfig>,
}

impl Default for RoutePatterns {
    fn default() -> Self {
        Self::new(core::iter::empty::<&str>())
    }
}

impl core::fmt::Debug for RoutePatterns {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RoutePatterns")
            .field("patterns", &self.count)
            .field("normalize", &self.normalize)
            .finish_non_exhaustive()
    }
}

impl RoutePatterns {
    /// Build a table from route patterns, in registration order.
    ///
    /// A pattern the table cannot accept — one that collides with an earlier
    /// one, or one `matchit` rejects outright — is dropped rather than reported.
    /// Both cases are already boot errors raised where they can name both source
    /// locations ([`Router::conflicts`](crate::Router::conflicts) and
    /// `validate_path`), and the consequence here is only that those requests
    /// resolve to `<unmatched>` — coarse, never wrong.
    ///
    /// ```
    /// use moso_core::middleware::RoutePatterns;
    ///
    /// // `matchit` cannot tell these apart, so the first registration wins,
    /// // exactly as `Router::into_axum` resolves it.
    /// let patterns = RoutePatterns::new(["/users/{id}", "/users/{user_id}"]);
    ///
    /// assert_eq!(patterns.len(), 1);
    /// assert_eq!(patterns.resolve("/users/42"), Some("/users/{id}"));
    /// ```
    #[must_use]
    pub fn new<I>(patterns: I) -> Self
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut table = matchit::Router::new();
        let mut count = 0_usize;
        for pattern in patterns {
            let pattern = pattern.as_ref();
            let value: Arc<str> = Arc::from(pattern);
            if table.insert(pattern, value).is_ok() {
                count += 1;
            }
        }
        Self {
            table,
            count,
            normalize: None,
        }
    }

    /// Build a table from every route a [`Router`] registered.
    ///
    /// Only the route *table* — a [`Router::mount_axum`](crate::Router::mount_axum)
    /// mount and a [`Router::static_files`](crate::Router::static_files) mount
    /// contribute nothing, because Moso does not know their patterns. Requests
    /// they serve resolve to `<unmatched>`, which is the same honesty that keeps
    /// them out of the OpenAPI document.
    ///
    /// ```
    /// use moso_core::Router;
    /// use moso_core::middleware::RoutePatterns;
    /// # /// Answer with nothing at all.
    /// # async fn show() -> &'static str { "ok" }
    /// let router = Router::new().get("/users/{id}", show);
    /// let patterns = RoutePatterns::from_router(&router);
    ///
    /// assert_eq!(patterns.resolve("/users/42"), Some("/users/{id}"));
    /// ```
    #[must_use]
    pub fn from_router(router: &Router) -> Self {
        Self::new(router.entries().iter().map(|entry| entry.path.as_str()))
    }

    /// Resolve as if `config` had already rewritten the path.
    ///
    /// [`Slot::NormalizePath`](crate::middleware::Slot::NormalizePath) runs
    /// inside this layer, so without this a request for `/users/` would be
    /// resolved against the *unnormalised* path and miss, while the router —
    /// which sees the rewritten `/users` — matches. The two would disagree about
    /// the same request, which is the one outcome worse than not resolving at
    /// all.
    ///
    /// Both this and the layer itself decide through
    /// [`NormalizePathConfig::canonical`], so the prediction and the rewrite
    /// cannot drift.
    ///
    /// ```
    /// use moso_core::middleware::{NormalizePathConfig, RoutePatterns};
    ///
    /// let patterns = RoutePatterns::new(["/users"]);
    /// assert_eq!(patterns.resolve("/users/"), None);
    ///
    /// // With the default trailing-slash policy in the stack, the router will
    /// // see `/users`, so that is what this resolves.
    /// let normalised = patterns.normalized_by(NormalizePathConfig::default());
    /// assert_eq!(normalised.resolve("/users/"), Some("/users"));
    /// ```
    #[must_use]
    pub fn normalized_by(mut self, config: NormalizePathConfig) -> Self {
        self.normalize = Some(config);
        self
    }

    /// How many patterns the table holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether the table is empty, in which case nothing can ever resolve.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The pattern this path will match, or `None`.
    ///
    /// `None` means the honest `<unmatched>`: a path outside the route table, a
    /// path served by a mounted Axum router, or a path a redirecting normaliser
    /// will answer with a 308 before routing ever happens.
    #[must_use]
    pub fn resolve(&self, path: &str) -> Option<&str> {
        self.lookup(path).map(|pattern| &**pattern)
    }

    /// [`RoutePatterns::resolve`], keeping the shared handle.
    ///
    /// What the layer calls: publishing the pattern is then a refcount bump on
    /// one of a boot-fixed set of strings, which is also why the extension
    /// cannot contribute to label cardinality.
    fn resolve_shared(&self, path: &str) -> Option<Arc<str>> {
        self.lookup(path).map(Arc::clone)
    }

    /// Resolve against the path the *router* will be given.
    fn lookup(&self, path: &str) -> Option<&Arc<str>> {
        let Some(config) = self.normalize else {
            return self.at(path);
        };
        match config.canonical(path) {
            // A redirecting normaliser answers 308 from outside the router, so
            // the request never reaches the route table and has no pattern.
            Some(_) if config.trailing_slash == TrailingSlash::Redirect => None,
            Some(rewritten) => self.at(&rewritten),
            None => self.at(path),
        }
    }

    /// One `matchit` lookup, with the match discarded and the value kept.
    fn at(&self, path: &str) -> Option<&Arc<str>> {
        self.table.at(path).ok().map(|matched| matched.value)
    }
}

// ---------------------------------------------------------------------------
// The layer
// ---------------------------------------------------------------------------

/// Wrap `service` in the pattern resolver.
///
/// Applied outside the whole composed stack, so every slot inside it — `trace`
/// and `timeout` near the outside, `metrics` innermost — reads the same answer.
/// An empty table wraps nothing: with no patterns to resolve the layer would
/// only cost a clone per request.
pub fn layer(patterns: Arc<RoutePatterns>, service: Route) -> Route {
    if patterns.is_empty() {
        return service;
    }
    Route::new(ResolveRoute {
        inner: service,
        patterns,
    })
}

/// The service [`layer`] builds.
#[derive(Clone)]
struct ResolveRoute {
    inner: Route,
    patterns: Arc<RoutePatterns>,
}

impl Service<Request> for ResolveRoute {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);

        // A request that resolves to nothing is left without the extension
        // rather than given a placeholder, so `matched_route` keeps one way to
        // say "no pattern" and the readers keep one constant to say it with.
        if let Some(pattern) = self.patterns.resolve_shared(req.uri().path()) {
            req.extensions_mut().insert(ResolvedRoute(pattern));
        }

        Box::pin(async move { inner.call(req).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt as _;

    // ── resolution ────────────────────────────────────────────────────────

    #[test]
    fn a_concrete_path_resolves_to_its_pattern_and_never_to_itself() {
        let patterns = RoutePatterns::new(["/users", "/users/{id}", "/users/{id}/posts"]);
        assert_eq!(patterns.resolve("/users"), Some("/users"));
        assert_eq!(patterns.resolve("/users/42"), Some("/users/{id}"));
        assert_eq!(
            patterns.resolve("/users/42/posts"),
            Some("/users/{id}/posts")
        );
    }

    #[test]
    fn a_static_segment_beats_a_parameter_exactly_as_the_router_does() {
        // `matchit` backtracks, so both coexist and the static one wins. This
        // is the case a hand-written matcher gets wrong, and getting it wrong
        // would attribute `/users/me` to `/users/{id}`.
        let patterns = RoutePatterns::new(["/users/{id}", "/users/me"]);
        assert_eq!(patterns.resolve("/users/me"), Some("/users/me"));
        assert_eq!(patterns.resolve("/users/other"), Some("/users/{id}"));
    }

    #[test]
    fn an_unknown_path_resolves_to_nothing_at_all() {
        let patterns = RoutePatterns::new(["/users/{id}"]);
        assert_eq!(patterns.resolve("/orders/1"), None);
        assert_eq!(patterns.resolve("/"), None);
        assert_eq!(patterns.resolve("/users"), None);
    }

    #[test]
    fn an_empty_table_resolves_nothing_and_says_so() {
        let patterns = RoutePatterns::default();
        assert!(patterns.is_empty());
        assert_eq!(patterns.len(), 0);
        assert_eq!(patterns.resolve("/anything"), None);
    }

    #[test]
    fn every_answer_comes_from_the_table_and_never_from_the_request() {
        // The pattern is a metric label and a timeout-exemption key, so nothing
        // a client writes may reach either. A path that spells the braces out is
        // matched, not echoed: it resolves to `/events/{id}` because that is the
        // route it genuinely hits, and to nothing when it hits no route.
        let patterns = RoutePatterns::new(["/events/{id}"]);
        assert_eq!(patterns.resolve("/events/%7Bid%7D"), Some("/events/{id}"));
        assert_eq!(patterns.resolve("/events/{id}/extra"), None);

        let other = RoutePatterns::new(["/logs/{id}"]);
        assert_eq!(other.resolve("/events/%7Bid%7D"), None);
    }

    #[test]
    fn the_first_of_two_indistinguishable_patterns_wins() {
        let patterns = RoutePatterns::new(["/users/{id}", "/users/{user_id}"]);
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns.resolve("/users/7"), Some("/users/{id}"));
    }

    #[test]
    fn a_pattern_matchit_refuses_is_dropped_rather_than_reported() {
        // `App::build` already rejects this shape where it can name the file
        // and line; here it only has to not panic and not lie.
        let patterns = RoutePatterns::new(["/users/{id}", "/{*a}/{*b}"]);
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns.resolve("/users/7"), Some("/users/{id}"));
    }

    // ── normalisation ─────────────────────────────────────────────────────

    #[test]
    fn without_a_normaliser_a_trailing_slash_is_a_different_path() {
        let patterns = RoutePatterns::new(["/users"]);
        assert_eq!(patterns.resolve("/users/"), None);
    }

    #[test]
    fn trimming_resolves_the_path_the_router_will_be_given() {
        let patterns = RoutePatterns::new(["/users", "/users/{id}"])
            .normalized_by(NormalizePathConfig::default());
        assert_eq!(patterns.resolve("/users/"), Some("/users"));
        assert_eq!(patterns.resolve("/users/42/"), Some("/users/{id}"));
        assert_eq!(patterns.resolve("/users"), Some("/users"));
    }

    #[test]
    fn appending_resolves_the_mirror_image() {
        let patterns = RoutePatterns::new(["/users/"]).normalized_by(NormalizePathConfig {
            trailing_slash: TrailingSlash::Append,
            collapse_slashes: false,
        });
        assert_eq!(patterns.resolve("/users"), Some("/users/"));
    }

    #[test]
    fn collapsing_repeated_slashes_is_predicted_too() {
        let patterns = RoutePatterns::new(["/users/{id}"]).normalized_by(NormalizePathConfig {
            trailing_slash: TrailingSlash::Trim,
            collapse_slashes: true,
        });
        assert_eq!(patterns.resolve("/users//42/"), Some("/users/{id}"));
    }

    #[test]
    fn a_request_a_redirecting_normaliser_will_answer_has_no_pattern() {
        // The 308 is written before the router is reached, so claiming the
        // pattern would attribute a redirect to an endpoint that never ran.
        let patterns =
            RoutePatterns::new(["/users"]).normalized_by(NormalizePathConfig::redirect());
        assert_eq!(patterns.resolve("/users/"), None);
        // …and the canonical spelling still resolves, because it is routed.
        assert_eq!(patterns.resolve("/users"), Some("/users"));
    }

    #[test]
    fn a_disabled_normaliser_predicts_no_rewrite() {
        let patterns = RoutePatterns::new(["/users"]).normalized_by(NormalizePathConfig::off());
        assert_eq!(patterns.resolve("/users/"), None);
        assert_eq!(patterns.resolve("/users"), Some("/users"));
    }

    // ── the layer ─────────────────────────────────────────────────────────

    /// Echoes whatever the resolver published, or `-` for nothing.
    fn echo_resolved() -> Route {
        Route::new(tower::service_fn(|req: Request| async move {
            let body = req
                .extensions()
                .get::<ResolvedRoute>()
                .map_or("-", ResolvedRoute::as_str)
                .to_owned();
            Ok::<_, Infallible>(Response::new(axum::body::Body::from(body)))
        }))
    }

    async fn seen(patterns: RoutePatterns, path: &str) -> String {
        use http_body_util::BodyExt as _;

        let request = http::Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .expect("request");
        let response = layer(Arc::new(patterns), echo_resolved())
            .oneshot(request)
            .await
            .expect("infallible");
        String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .expect("body")
                .to_bytes()
                .to_vec(),
        )
        .expect("utf-8")
    }

    #[tokio::test]
    async fn the_layer_publishes_the_pattern_for_a_matched_request() {
        let seen = seen(RoutePatterns::new(["/users/{id}"]), "/users/42").await;
        assert_eq!(seen, "/users/{id}");
    }

    #[tokio::test]
    async fn the_layer_publishes_nothing_for_an_unmatched_request() {
        let seen = seen(RoutePatterns::new(["/users/{id}"]), "/orders/42").await;
        assert_eq!(seen, "-");
    }

    #[tokio::test]
    async fn an_empty_table_installs_no_layer_at_all() {
        // The `Route` comes back unchanged, so a router with no Moso routes
        // pays nothing for a resolver that could never resolve.
        let seen = seen(RoutePatterns::default(), "/users/42").await;
        assert_eq!(seen, "-");
    }

    #[test]
    fn the_debug_form_says_how_many_patterns_are_in_the_table() {
        let patterns = RoutePatterns::new(["/a", "/b"]);
        assert!(format!("{patterns:?}").contains("patterns: 2"));
    }

    // ── through a real application ────────────────────────────────────────
    //
    // The unit tests above prove the table. These prove the wiring: that
    // `App::build` hands the table to the stack, that the stack installs the
    // resolver outside itself, and that the `route` label an operator sees is
    // therefore the pattern.

    use std::sync::Mutex;

    use crate::app::App;
    use crate::config::{Config, ConfigDescriptor, ConfigKey, ConfigLoader};
    use crate::error::BootErrors;
    use crate::middleware::metrics::{MetricsRecorder, RequestSample};

    /// The smallest thing that satisfies `App::new`.
    #[derive(Debug, Clone)]
    struct TestConfig;

    impl Config for TestConfig {
        fn descriptor() -> &'static ConfigDescriptor {
            static DESCRIPTOR: ConfigDescriptor = ConfigDescriptor {
                type_name: "TestConfig",
                fields: &[],
            };
            &DESCRIPTOR
        }

        fn load_nested(
            _loader: &ConfigLoader,
            _prefix: &ConfigKey,
            _errors: &mut BootErrors,
        ) -> Option<Self> {
            Some(TestConfig)
        }
    }

    /// Records the `route` label of every request that reached the metrics slot.
    #[derive(Clone, Default)]
    struct Labels(Arc<Mutex<Vec<String>>>);

    impl MetricsRecorder for Labels {
        fn record(&self, sample: &RequestSample<'_>) {
            self.0.lock().expect("lock").push(sample.route.to_owned());
        }
    }

    /// Answer with nothing at all.
    async fn show() -> &'static str {
        "ok"
    }

    /// Boot an application with one Moso route, one mounted Axum route, and a
    /// recorder watching the `route` label.
    fn labelled_app() -> (Labels, axum::Router<()>) {
        let labels = Labels::default();
        let mounted = axum::Router::new().route("/thing/{id}", axum::routing::get(|| async { "" }));
        let recorder = labels.clone();

        let app = App::new(TestConfig)
            .mount(
                Router::new()
                    .get("/users/{id}", show)
                    .mount_axum("/external", mounted),
            )
            .with_middleware(move |stack| {
                stack.metrics(Arc::new(recorder));
            })
            .build()
            .expect("nothing to fail");

        (labels, app.into_service())
    }

    async fn hit(service: &axum::Router<()>, path: &str) {
        let request = http::Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .expect("request");
        service
            .clone()
            .into_service::<axum::body::Body>()
            .oneshot(request)
            .await
            .expect("infallible");
    }

    #[tokio::test]
    async fn a_booted_application_labels_two_paths_of_one_route_with_one_pattern() {
        let (labels, service) = labelled_app();
        hit(&service, "/users/1").await;
        hit(&service, "/users/2").await;
        assert_eq!(labels.0.lock().expect("lock").clone(), ["/users/{id}"; 2]);
    }

    #[tokio::test]
    async fn a_booted_application_folds_unknown_paths_into_one_series() {
        let (labels, service) = labelled_app();
        for path in ["/nope/1", "/nope/2", "/nope/3"] {
            hit(&service, path).await;
        }
        assert_eq!(
            labels.0.lock().expect("lock").clone(),
            [crate::middleware::UNMATCHED_ROUTE; 3]
        );
    }

    #[tokio::test]
    async fn a_mount_axum_route_is_unmatched_because_moso_cannot_see_its_pattern() {
        // Consistent with it being absent from the OpenAPI document: Moso does
        // not know the pattern, so it does not invent one.
        let (labels, service) = labelled_app();
        hit(&service, "/external/thing/7").await;
        assert_eq!(
            labels.0.lock().expect("lock").clone(),
            [crate::middleware::UNMATCHED_ROUTE]
        );
    }

    #[tokio::test]
    async fn a_booted_application_resolves_through_its_own_normaliser() {
        // `normalize_path` trims by default and runs *inside* the resolver, so
        // the resolver has to predict the rewrite or disagree with the router.
        let (labels, service) = labelled_app();
        hit(&service, "/users/9/").await;
        assert_eq!(labels.0.lock().expect("lock").clone(), ["/users/{id}"]);
    }
}
