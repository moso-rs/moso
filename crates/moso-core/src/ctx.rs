//! The per-request context: providers, the dependency cache, and the limits.
//!
//! One [`RequestCtx`] is built per request, before the first extractor runs,
//! and every extractor, guard and dependency sees the same one. It is `Clone`
//! (an `Arc` inside), so passing it to a spawned task costs a refcount bump.
//!
//! # Why the request head is copied
//!
//! Extraction needs `&mut Parts` — an extractor may take a header out of the
//! map — while dependencies and guards need a stable read-only view of the
//! head. Handing both a `&mut` is not expressible, so the context takes its own
//! snapshot of the method, URI, version and headers when it is created. That is
//! one `HeaderMap` clone per request; it buys a `RequestCtx` that is `Clone`,
//! `'static` and shareable, which the whole dependency model rests on.
//!
//! # Single-flight dependency resolution
//!
//! Two extractors on the same handler can await `Depends<CurrentUser>`
//! concurrently — a body extractor's future and a guard's future can be joined.
//! A naive "check, resolve, insert" cache would run the database query twice.
//!
//! The cache therefore stores a `tokio::sync::OnceCell` per type rather than a
//! finished value: the first caller initialises it, later callers await the
//! same cell. The `std::sync::Mutex` guarding the slot vector is **only ever
//! held to find or create a cell**, never across an `.await` — the guard is
//! dropped before the cell is awaited. Failure is not memoised: a `resolve`
//! that returned `Err` leaves the cell empty, so a retry within the same
//! request is possible.
//!
//! One consequence worth stating plainly: a dependency whose `resolve` awaits
//! **itself** — directly, or around a cycle — waits on a cell it is itself
//! initialising, and never completes. There is no cheap way to tell that apart
//! from the legitimate concurrent case (two extractors awaiting the same type
//! at once), which is the case single-flight exists to serve, so the framework
//! does not try: the request's timeout layer ends it, and the fix is not to
//! write a dependency cycle. Provider cycles, which *can* be detected cheaply,
//! are rejected at boot instead.

// `RequestCtx::depends` consults the test-override table behind
// `cfg(any(test, feature = "test"))`. Both halves are declared: `cfg(test)` for
// this crate's own tests, the `test` cargo feature for a downstream test suite.
// A default build takes neither branch and pays for neither.

use std::any::{Any, TypeId};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use http::{HeaderMap, Method, Uri, Version};
use moso_schema::{Locale, MessageProvider, ValidationCtx};
use ulid::Ulid;

use crate::app::AppState;
use crate::di::Dependency;
use crate::error::{Error, Result};
use crate::extract::Cookies;
use crate::http_config::HttpConfig;
use crate::{BoxFuture, Response};

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// The request-shaped safety limits, snapshotted from `HttpConfig` at boot.
///
/// Copied into every [`RequestCtx`] so an extractor can enforce a limit without
/// reaching back through the provider map. Exceeding any of them produces a
/// documented problem response — 400 for nesting, 413 for a body, 414 for a
/// request target, 431 for headers — never a panic and never a silent
/// truncation.
///
/// There is deliberately no 408. It would need a read deadline on the request
/// body distinct from the whole-request timeout, and Moso has no such deadline
/// and no configuration key for one; the `timeout` slot already ends a request
/// that does not finish, and it answers 504. A second spelling of the same
/// condition would be a status a client cannot act on differently.
///
/// ```
/// use moso::prelude::*;
/// use moso::http_config::HttpConfig;
///
/// let limits = HttpConfig::default().limits();
///
/// // The defaults are the ones an extractor enforces before allocating.
/// assert_eq!(limits.body_max, 2 * 1024 * 1024);
/// assert!(limits.header_max_count >= 100);
/// ```
///
/// Reached from a handler through `ctx.limits()`, which is how a hand-written
/// [`ExtractBody`](crate::ExtractBody) reads the same cap `Json<T>` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum request body, in bytes. `http.body_max`, default 2 MiB.
    ///
    /// Enforced *before* deserialising: `Json<T>` reads with a hard cap rather
    /// than allocating four gigabytes and then failing.
    pub body_max: usize,
    /// Maximum total multipart payload. `http.multipart_max`, default 32 MiB.
    pub multipart_max: usize,
    /// Maximum size of a single multipart file. `http.multipart_file_max`,
    /// default 16 MiB.
    pub multipart_file_max: usize,
    /// Maximum number of request headers. `http.header_max_count`, default 100.
    pub header_max_count: usize,
    /// Maximum total header bytes. `http.header_max_bytes`, default 16 KiB.
    pub header_max_bytes: usize,
    /// Maximum request-target length. `http.uri_max`, default 8 KiB.
    pub uri_max: usize,
    /// Maximum bracket nesting in a query string. `http.query_depth_max`,
    /// default 8.
    pub query_depth_max: usize,
    /// Maximum JSON nesting depth. `http.json_depth_max`, default 64.
    pub json_depth_max: usize,
}

impl Limits {
    /// The documented defaults, as a `const` so they can be named in tests and
    /// in `.env.example` generation.
    pub const DEFAULT: Limits = Limits {
        body_max: 2 * 1024 * 1024,
        multipart_max: 32 * 1024 * 1024,
        multipart_file_max: 16 * 1024 * 1024,
        header_max_count: 100,
        header_max_bytes: 16 * 1024,
        uri_max: 8 * 1024,
        query_depth_max: 8,
        json_depth_max: 64,
    };

    /// The limits implied by an `HttpConfig`.
    pub fn from_config(config: &HttpConfig) -> Self {
        Limits {
            body_max: config.body_max,
            multipart_max: config.multipart_max,
            multipart_file_max: config.multipart_file_max,
            header_max_count: config.header_max_count,
            header_max_bytes: config.header_max_bytes,
            uri_max: config.uri_max,
            query_depth_max: config.query_depth_max,
            json_depth_max: config.json_depth_max,
        }
    }

    /// Reject a request head that does not fit inside the configured bounds.
    ///
    /// The single definition of "does this head fit": the `request_limits`
    /// middleware slot calls it on the way in, and anything that has a
    /// [`RequestCtx`] can call it again without a second opinion about what the
    /// numbers mean.
    ///
    /// The count is compared before the bytes are summed, so a header flood
    /// costs one comparison rather than a walk over the flood.
    ///
    /// # What this is *not*
    ///
    /// It is not the first line of defence, and pretending otherwise would be
    /// dishonest: by the time any Moso code runs, hyper has already read and
    /// parsed the head, under its own `max_headers` and buffer limits. This is
    /// the **policy** layer — it applies the operator's configured numbers and
    /// answers with a documented RFC 9457 problem instead of hyper's
    /// connection-level rejection, which carries no body a client can read.
    ///
    /// ```
    /// use moso::prelude::*;
    /// use moso::ctx::Limits;
    /// use moso::deps::http::{HeaderMap, Uri};
    ///
    /// let limits = Limits {
    ///     uri_max: 16,
    ///     ..Limits::DEFAULT
    /// };
    ///
    /// let short: Uri = "/posts".parse().expect("a URI");
    /// assert!(limits.check_head(&short, &HeaderMap::new()).is_ok());
    ///
    /// let long: Uri = "/posts?search=a-very-long-search-term".parse().expect("a URI");
    /// let error = limits.check_head(&long, &HeaderMap::new()).expect_err("too long");
    /// assert_eq!(error.status(), 414);
    /// ```
    ///
    /// # Errors
    /// [`ErrorKind::UriTooLong`](crate::ErrorKind::UriTooLong) — a 414 — when the
    /// request target is longer than `uri_max`, and
    /// [`ErrorKind::HeaderFieldsTooLarge`](crate::ErrorKind::HeaderFieldsTooLarge)
    /// — a 431 — when there are more than `header_max_count` header fields or
    /// their names and values total more than `header_max_bytes`.
    pub fn check_head(&self, uri: &Uri, headers: &HeaderMap) -> Result<()> {
        if request_target_len(uri) > self.uri_max {
            return Err(Error::uri_too_long(self.uri_max));
        }
        if headers.len() > self.header_max_count {
            return Err(Error::too_many_headers(self.header_max_count));
        }
        if header_bytes(headers) > self.header_max_bytes {
            return Err(Error::headers_too_large(self.header_max_bytes));
        }
        Ok(())
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits::DEFAULT
    }
}

/// The byte length of the request target, without rendering the URI.
///
/// `Uri::to_string` would allocate on every request to measure something that
/// is a sum of three borrowed slices. Both RFC 9112 forms are covered: the
/// origin-form `/posts?page=2` that almost every request uses, and the
/// absolute-form `http://host/posts` a proxy sees, whose scheme and authority
/// are part of the target and therefore part of the length.
///
/// A URI with no path and query at all — the asterisk-form `OPTIONS *` — counts
/// as one byte, which is what was on the wire.
fn request_target_len(uri: &Uri) -> usize {
    let scheme = uri
        .scheme_str()
        .map_or(0, |scheme| scheme.len() + "://".len());
    let authority = uri
        .authority()
        .map_or(0, |authority| authority.as_str().len());
    let target = uri
        .path_and_query()
        .map_or(1, |target| target.as_str().len());
    scheme + authority + target
}

/// The total header bytes, counted as names plus values.
///
/// Framing is deliberately excluded. HTTP/1.1 spends four more bytes per field
/// on `": "` and CRLF while HTTP/2 spends none and may spend *fewer* than the
/// name and value themselves once HPACK has indexed them, so a limit expressed
/// in wire bytes would mean a different thing on each protocol. Names and values
/// are what the application actually has to hold in memory, and they are the
/// same on both.
fn header_bytes(headers: &HeaderMap) -> usize {
    headers
        .iter()
        .map(|(name, value)| name.as_str().len() + value.len())
        .sum()
}

// ---------------------------------------------------------------------------
// DependencyCache
// ---------------------------------------------------------------------------

/// Per-request memoisation for [`Dependency`] values, keyed by `TypeId`.
///
/// A linear scan over a `Vec` beats hashing at the sizes involved — a handler
/// with more than a handful of distinct dependencies is unusual — and costs no
/// allocation at all for the common zero-or-one case.
#[derive(Debug, Default)]
pub struct DependencyCache {
    slots: Mutex<Vec<CacheSlot>>,
}

/// The type-erased memoised value. One `Box` per distinct dependency type per
/// request.
type CachedValue = Box<dyn Any + Send + Sync>;

/// A cell that is either empty, being filled by exactly one caller, or filled.
type Cell = Arc<tokio::sync::OnceCell<CachedValue>>;

/// One memoised type. Private: the cell type is an implementation detail that
/// callers must not be able to observe or hold across a suspension point.
#[derive(Debug)]
struct CacheSlot {
    type_id: TypeId,
    cell: Cell,
}

impl DependencyCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many distinct dependency types have been asked for.
    ///
    /// Exposed for the test assertion `assert_eq!(cache.len(), 1)` that proves
    /// memoisation actually happens.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing has been resolved yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `T` has been resolved (successfully) in this request.
    ///
    /// A type whose `resolve` failed, or is still running, reads as `false`:
    /// the question this answers is "is there a value", not "has anyone asked".
    pub fn contains<T: 'static>(&self) -> bool {
        self.existing_cell(TypeId::of::<T>())
            .is_some_and(|cell| cell.initialized())
    }

    /// The slot vector, recovering from a poisoned mutex.
    ///
    /// Nothing in the critical sections can panic — they compare `TypeId`s and
    /// clone `Arc`s — so a poisoned lock means a panic elsewhere in the process
    /// while the guard happened to be held. Refusing to serve the rest of the
    /// request over that would turn an unrelated panic into a second failure,
    /// and the data behind the lock is a plain vector that cannot be left
    /// half-updated.
    fn lock(&self) -> MutexGuard<'_, Vec<CacheSlot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cell for `type_id`, if one has been created.
    fn existing_cell(&self, type_id: TypeId) -> Option<Cell> {
        let slots = self.lock();
        slots
            .iter()
            .find(|slot| slot.type_id == type_id)
            .map(|slot| Arc::clone(&slot.cell))
    }

    /// The cell for `T`, creating it if this is the first ask.
    ///
    /// Private, and deliberately not `async`: it takes the mutex, finds or
    /// inserts, clones the `Arc` and drops the guard. Awaiting happens in
    /// [`RequestCtx::depends`], with no lock held.
    fn cell_for(&self, type_id: TypeId) -> Cell {
        let mut slots = self.lock();
        if let Some(slot) = slots.iter().find(|slot| slot.type_id == type_id) {
            return Arc::clone(&slot.cell);
        }
        let cell = Cell::default();
        slots.push(CacheSlot {
            type_id,
            cell: Arc::clone(&cell),
        });
        cell
    }

    /// Resolve `T` once, or hand back what this request already resolved.
    ///
    /// Generic over the error type so the memoisation itself can be tested
    /// without constructing a framework [`Error`]. `Ok(None)` is the impossible
    /// case — a slot keyed by `TypeId::of::<T>()` holding something that is not
    /// a `T` — which [`Self::resolve_with`] turns into an error rather than a
    /// panic.
    ///
    /// The `std::sync::Mutex` guard obtained by [`Self::cell_for`] is dropped
    /// before the first `.await` in this function. That is not an accident of
    /// formatting: holding it across the suspension point would let a second
    /// extractor on the same thread deadlock the request.
    async fn get_or_init<T, E, F, Fut>(&self, init: F) -> core::result::Result<Option<T>, E>
    where
        T: Clone + Send + Sync + 'static,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = core::result::Result<T, E>> + Send,
    {
        let cell = self.cell_for(TypeId::of::<T>());
        let stored = cell
            .get_or_try_init(|| async { init().await.map(|value| Box::new(value) as CachedValue) })
            .await?;
        Ok((**stored).downcast_ref::<T>().cloned())
    }

    /// [`Self::get_or_init`] at the framework's error type.
    async fn resolve_with<T, F, Fut>(&self, init: F) -> Result<T>
    where
        T: Clone + Send + Sync + 'static,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T>> + Send,
    {
        self.get_or_init::<T, Error, F, Fut>(init)
            .await?
            .ok_or_else(cache_invariant_error::<T>)
    }
}

/// The error for a state [`DependencyCache`] does not allow itself to be in.
///
/// A slot is keyed by `TypeId::of::<T>()` and only ever written by the
/// `get_or_init::<T>` that created it, so the downcast cannot fail. It is an
/// error rather than an `unwrap` because a framework has no business panicking
/// inside someone else's request to report its own bug.
fn cache_invariant_error<T: 'static>() -> Error {
    Error::internal_msg(format!(
        "the request dependency cache holds a value of the wrong type for `{}`; this is a bug in \
         moso-core, please report it",
        core::any::type_name::<T>()
    ))
}

// ---------------------------------------------------------------------------
// RequestCtx
// ---------------------------------------------------------------------------

/// Everything a handler's extractors, guards and dependencies can see.
///
/// Cheap to clone — an `Arc` inside. Created once per request by the handler
/// adapter, before the first extractor runs, and handed to every [`Extract`],
/// [`ExtractBody`] and [`Dependency`] in turn. Application code rarely names it
/// except when writing one of those.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::NoContent;
///
/// /// Whoever this request is acting as, resolved once per request.
/// #[derive(Clone, Debug)]
/// pub struct Actor {
///     /// The account's public handle.
///     pub name: String,
/// }
///
/// impl Dependency for Actor {
///     const PROVIDER_REQ: &'static [moso::ProviderReq] = &[];
///
///     async fn resolve(ctx: &RequestCtx) -> Result<Self> {
///         // The head, the correlation id and the configured limits are all
///         // here; there is no second place to look.
///         let name = ctx
///             .headers()
///             .get("x-actor")
///             .and_then(|value| value.to_str().ok())
///             .unwrap_or("anonymous")
///             .to_owned();
///         let _ = (ctx.request_id(), ctx.limits().body_max, ctx.method());
///         Ok(Actor { name })
///     }
/// }
///
/// /// Report who is asking.
/// #[endpoint]
/// async fn whoami(Depends(actor): Depends<Actor>) -> Result<Json<String>> {
///     Ok(Json(actor.name))
/// }
/// # fn main() { assert_eq!(Router::new().get("/me", moso::ep!(whoami)).len(), 1); }
/// ```
///
/// Resolution is cached and single-flight *per request*: two extractors that
/// both ask for `Depends<Actor>` share one `resolve`, and the second awaits the
/// first rather than running it again.
///
/// [`Extract`]: crate::Extract
/// [`ExtractBody`]: crate::ExtractBody
/// [`Dependency`]: crate::Dependency
///
/// ```
/// use moso::prelude::*;
/// use moso::response::NoContent;
///
/// /// Who the request acts as.
/// #[derive(Clone, Debug)]
/// pub struct CurrentUser {
///     /// Their identifier.
///     pub id: u64,
/// }
///
/// impl Dependency for CurrentUser {
///     async fn resolve(ctx: &RequestCtx) -> Result<Self> {
///         // Everything a dependency can see comes from here.
///         let _ = ctx.headers().get("authorization");
///         let _ = ctx.request_id();
///         Ok(CurrentUser { id: 1 })
///     }
/// }
///
/// /// Whoever asks for `CurrentUser` twice pays for it once.
/// #[endpoint]
/// async fn whoami(ctx: RequestCtx) -> Result<NoContent> {
///     let first = ctx.depends::<CurrentUser>().await?;
///     let second = ctx.depends::<CurrentUser>().await?;
///     assert_eq!(first.id, second.id);   // one resolve, cached by `TypeId`
///     Ok(NoContent)
/// }
/// # fn main() { assert_eq!(Router::new().get("/me", moso::ep!(whoami)).len(), 1); }
/// ```
///
/// Taking it as a handler parameter is a last resort: `Path`, `Query`, `Headers`,
/// `Inject` and `Depends` say what a handler reads, and a bare `RequestCtx` does
/// not. It is the right parameter for a hand-written extractor or dependency.
#[derive(Clone)]
pub struct RequestCtx(Arc<RequestCtxInner>);

/// The contents of a [`RequestCtx`].
///
/// Public so that `moso-test` can build one directly for a unit test of an
/// extractor, and so the shape is documented rather than guessed at. Normal
/// code goes through [`RequestCtx`].
pub struct RequestCtxInner {
    /// Providers, config and the shutdown signal.
    pub state: Arc<AppState>,
    /// Per-request dependency memoisation.
    pub cache: DependencyCache,
    /// The request's extensions, snapshotted after middleware ran.
    ///
    /// Middleware communicates with dependencies through this: a `TenantLayer`
    /// inserts a `Tenant`, and `impl Dependency for Tenant` reads it back.
    pub extensions: http::Extensions,
    /// The correlation id, from `x-request-id` or freshly generated.
    pub request_id: Ulid,
    /// The request method.
    pub method: Method,
    /// The request URI, including the query string.
    pub uri: Uri,
    /// The HTTP version.
    pub version: Version,
    /// The request headers, as they were when the context was created.
    pub headers: HeaderMap,
    /// The matched route pattern, `/users/{id}` rather than `/users/42`.
    ///
    /// `None` for a fallback. Used for metrics labels — a raw path label is how
    /// you get a million-series cardinality explosion — and for error reports.
    pub matched_path: Option<Arc<str>>,
    /// The limits in force for this request.
    pub limits: Limits,
    /// This request's one cookie jar, created the first time anything asks.
    ///
    /// The jar has to outlive the request head — the handler adapter drains it
    /// into `Set-Cookie` headers *after* the handler returned and the head was
    /// consumed — and it has to be reachable from a guard, which only ever sees
    /// `&Parts`. The context is the one thing both ends hold, so it is the
    /// jar's one home; see [`RequestCtx::cookies`].
    ///
    /// A `OnceLock` rather than an eager jar because most requests never
    /// mention a cookie, and those must pay neither the header parse nor the
    /// allocation. Reading it back is one atomic load.
    pub cookies: OnceLock<Cookies>,
}

impl RequestCtx {
    /// Build a context from the application state and a request head.
    ///
    /// Called by the handler adapter. `request_id` comes from the `request_id`
    /// middleware, which has already put it in the extensions; the header and a
    /// freshly generated id are the fallbacks, in that order, so a context
    /// built for a route that bypassed the middleware still has a correlation
    /// id rather than an `Option`.
    ///
    /// The matched path and the captured path parameters come from the
    /// extensions Axum's router populated during matching. The parameters are
    /// re-published into this context's own extension snapshot under
    /// [`PathParams`], which is what [`RequestCtx::path_params`] reads and what
    /// `Path<T>` deserialises from.
    ///
    /// Cost: one `HeaderMap` clone, one `Extensions` clone, and — only for a
    /// route that captured something — one vector of captures. See the module
    /// header for why the head is copied rather than borrowed.
    pub fn new(state: Arc<AppState>, parts: &http::request::Parts) -> Self {
        let limits = state.http().limits();
        let matched_path = parts
            .extensions
            .get::<axum::extract::MatchedPath>()
            .map(|matched| Arc::<str>::from(matched.as_str()));
        let request_id = request_id_for(parts);

        let (mut extensions, path_params) = take_path_params(parts.extensions.clone());
        if let Some(params) = path_params {
            extensions.insert(params);
        }

        RequestCtx(Arc::new(RequestCtxInner {
            state,
            cache: DependencyCache::new(),
            extensions,
            request_id,
            method: parts.method.clone(),
            uri: parts.uri.clone(),
            version: parts.version,
            headers: parts.headers.clone(),
            matched_path,
            limits,
            cookies: OnceLock::new(),
        }))
    }

    /// Build a context from its parts, for tests and for `moso-test`.
    pub fn from_inner(inner: RequestCtxInner) -> Self {
        RequestCtx(Arc::new(inner))
    }

    /// The context's contents.
    pub fn inner(&self) -> &RequestCtxInner {
        &self.0
    }

    // ── providers ─────────────────────────────────────────────────────────

    /// The provider registered for `T`.
    ///
    /// Returns `Err` only when the caller declared no [`ProviderReq`] for `T`
    /// and the application therefore never registered it — see
    /// [`crate::di::missing_provider_error`]. `Inject<T>` cannot reach that
    /// path, because its `PROVIDER_REQ` made boot check it.
    ///
    /// [`ProviderReq`]: crate::di::ProviderReq
    pub fn provider<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        self.0
            .state
            .providers()
            .get::<T>()
            .ok_or_else(|| crate::di::missing_provider_error(core::any::type_name::<T>()))
    }

    /// The provider registered for `T`, or `None`.
    pub fn try_provider<T: ?Sized + Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.0.state.providers().get::<T>()
    }

    /// The application's configuration.
    pub fn config<C: crate::config::Config>(&self) -> Result<Arc<C>> {
        self.provider::<C>()
    }

    // ── dependencies ──────────────────────────────────────────────────────

    /// Resolve `D`, or return the value already resolved for this request.
    ///
    /// Boxed rather than an `async fn` because dependency resolution is
    /// recursive — a `resolve` body calls `ctx.depends` again — and a recursive
    /// `async fn` has an infinitely-sized future. The box is the fixed point.
    ///
    /// Concurrent callers of the same `D` share one resolution; see the module
    /// header. A `resolve` that fails is not memoised, so an error is not
    /// sticky for the rest of the request.
    ///
    /// A test override installed with `override_dependency` replaces
    /// `D::resolve` and is memoised exactly the same way, so a fixture is
    /// evaluated once per request like the real thing. The lookup that finds it
    /// is compiled out of a release build.
    pub fn depends<D: Dependency>(&self) -> BoxFuture<'_, Result<D>> {
        Box::pin(async move {
            #[cfg(any(test, feature = "test"))]
            if let Some(overridden) = self.dependency_override::<D>() {
                let ctx = self.clone();
                return self
                    .0
                    .cache
                    .resolve_with::<D, _, _>(move || (*overridden)(ctx))
                    .await;
            }

            self.0
                .cache
                .resolve_with::<D, _, _>(|| D::resolve(self))
                .await
        })
    }

    /// The test override registered for `D`, if the application installed one.
    ///
    /// The table is an ordinary provider, so this is one provider lookup. It
    /// exists only in a test build; see
    /// [`DependencyOverrides`](crate::di::DependencyOverrides).
    #[cfg(any(test, feature = "test"))]
    fn dependency_override<D: Dependency>(&self) -> Option<crate::di::DependencyOverrideFn<D>> {
        self.try_provider::<crate::di::DependencyOverrides>()?
            .get::<D>()
    }

    /// The per-request dependency cache, for assertions in tests.
    pub fn cache(&self) -> &DependencyCache {
        &self.0.cache
    }

    // ── the request head ──────────────────────────────────────────────────

    /// The request headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.0.headers
    }

    /// The request method.
    pub fn method(&self) -> &Method {
        &self.0.method
    }

    /// The request URI, query string included.
    pub fn uri(&self) -> &Uri {
        &self.0.uri
    }

    /// The HTTP version.
    pub fn version(&self) -> Version {
        self.0.version
    }

    /// The path as the client sent it.
    pub fn path(&self) -> &str {
        self.0.uri.path()
    }

    /// The matched route pattern, `/users/{id}` style.
    pub fn matched_path(&self) -> Option<&str> {
        self.0.matched_path.as_deref()
    }

    /// A clone of a request extension inserted by middleware.
    pub fn extension<T: Clone + Send + Sync + 'static>(&self) -> Option<T> {
        self.0.extensions.get::<T>().cloned()
    }

    /// The correlation id. Present on every request; generated if the client
    /// did not send one.
    pub fn request_id(&self) -> &Ulid {
        &self.0.request_id
    }

    /// The limits in force.
    pub fn limits(&self) -> &Limits {
        &self.0.limits
    }

    // ── cookies ───────────────────────────────────────────────────────────

    /// This request's cookies — the one jar, whoever asks.
    ///
    /// Parsed from the request's `Cookie` header the first time it is called,
    /// and shared by everything that calls it afterwards: a guard, a
    /// dependency, [`Cookies`] as a handler parameter, the handler body. What
    /// any of them adds or removes is written back as `Set-Cookie` by the
    /// handler adapter once the response exists.
    ///
    /// This is the only constructor a request goes through, which is the point.
    /// A second jar would accept every write and then be dropped, and nothing
    /// about the code that wrote to it would look wrong.
    ///
    /// ```
    /// use moso::prelude::*;
    /// use moso::extract::Cookie;
    /// use moso::response::NoContent;
    ///
    /// /// Set a cookie without taking `Cookies` as a parameter.
    /// #[endpoint]
    /// async fn visit(ctx: RequestCtx) -> Result<NoContent> {
    ///     ctx.cookies().add(Cookie::new("seen", "1"));
    ///     Ok(NoContent)
    /// }
    /// # fn main() { assert_eq!(Router::new().get("/visit", moso::ep!(visit)).len(), 1); }
    /// ```
    pub fn cookies(&self) -> &Cookies {
        self.0.cookies.get_or_init(|| Cookies::for_request(self))
    }

    /// The cookie jar, only if this request has already created one.
    ///
    /// What the handler adapter calls once the response exists. `None` means
    /// nothing in the request ever mentioned a cookie — the overwhelming
    /// majority — and the response is passed on untouched. One atomic load: no
    /// lock, no allocation, no header parse.
    pub fn cookies_if_used(&self) -> Option<&Cookies> {
        self.0.cookies.get()
    }

    // ── validation messages ───────────────────────────────────────────────

    /// The [`MessageProvider`] the application registered, if it registered one.
    ///
    /// One lookup in the frozen provider map — a hash of a `TypeId` — and
    /// nothing else. An application that registers no provider pays for the
    /// miss and stops there; in particular it never parses `Accept-Language`,
    /// because with no provider the locale cannot change a single message.
    #[must_use]
    pub fn message_provider(&self) -> Option<Arc<dyn MessageProvider>> {
        self.try_provider::<dyn MessageProvider>()
    }

    /// The locale this request asked for, from `Accept-Language`.
    ///
    /// Quality values are honoured, `q=0` means "not acceptable", `*` is not a
    /// language tag and is skipped, and an entry that does not parse — a
    /// malformed weight, a non-tag, a header that is not even UTF-8 — is
    /// dropped rather than failing the request. `None` when nothing in the
    /// header is usable, which is the caller's cue to apply its own default;
    /// see [`Locale::from_accept_language`], which is where the parsing lives.
    ///
    /// A request has exactly one locale, so this is *read* per validating
    /// extractor rather than per field.
    #[must_use]
    pub fn locale(&self) -> Option<Locale> {
        self.0
            .headers
            .get(http::header::ACCEPT_LANGUAGE)
            .and_then(|value| value.to_str().ok())
            .and_then(Locale::from_accept_language)
    }

    /// The [`ValidationCtx`] this request validates through, rooted at
    /// `pointer`.
    ///
    /// **Every extractor that validates builds its context here** — `Json`,
    /// `Form`, `Query`, `Headers` and `Path` all call it — so the registered
    /// [`MessageProvider`] and the request's locale reach validation without any
    /// of them having to remember. An extractor that called
    /// `ValidationCtx::new()` instead would silently ship the bundled English,
    /// which is exactly the drift this method exists to delete.
    ///
    /// `pointer` is the JSON Pointer root failures are reported under: `""` for
    /// a body, `"/query"`, `"/path"` or `"/header"` for the other sources.
    ///
    /// An application registers its provider once, at boot:
    ///
    /// ```
    /// use moso::prelude::*;
    /// use moso::schema::{Locale, MessageProvider, codes};
    /// use std::collections::BTreeMap;
    /// use std::sync::Arc;
    /// use serde_json::Value;
    ///
    /// /// Everything this application reads from its environment.
    /// #[derive(Config, Debug, Clone, Default)]
    /// pub struct AppConfig {}
    ///
    /// /// French wording for the codes this application cares about.
    /// pub struct French;
    ///
    /// impl MessageProvider for French {
    ///     fn message(
    ///         &self,
    ///         code: &str,
    ///         params: &BTreeMap<&'static str, Value>,
    ///         locale: &Locale,
    ///     ) -> Option<String> {
    ///         if locale.language() != "fr" || code != codes::LEN {
    ///             return None;   // fall through to the bundled English
    ///         }
    ///         let min = params.get("min")?;
    ///         Some(format!("doit contenir au moins {min} caractères"))
    ///     }
    /// }
    ///
    /// # fn main() -> Result<()> {
    /// let app = App::new(AppConfig::default())
    ///     .provide_dyn::<dyn MessageProvider>(Arc::new(French))
    ///     .mount(Router::new())
    ///     .build()?;
    /// # let _ = app;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// From then on a `422` for a request sending `Accept-Language: fr` carries
    /// the French wording, and one sending nothing carries the English, with no
    /// change to any model or handler.
    #[must_use]
    pub fn validation(&self, pointer: &str) -> ValidationCtx {
        let validation = ValidationCtx::rooted_at(pointer);
        let Some(provider) = self.message_provider() else {
            return validation;
        };
        let validation = validation.with_messages(provider);
        match self.locale() {
            Some(locale) => validation.with_locale(locale),
            None => validation,
        }
    }

    // ── application state ─────────────────────────────────────────────────

    /// The frozen application state.
    pub fn state(&self) -> &Arc<AppState> {
        &self.0.state
    }

    /// The shutdown signal, for long-lived handlers.
    ///
    /// An SSE or WebSocket handler should `select!` on this and close cleanly;
    /// the framework logs a warning naming any route still open when the grace
    /// period ends, which is how a leak gets found.
    pub fn shutdown(&self) -> &crate::shutdown::Signal {
        self.0.state.shutdown()
    }

    /// The path-parameter values `matchit` captured, in declaration order.
    ///
    /// `Path<T>` reads this. `None` when the route has no parameters, and also
    /// when a captured segment was not valid UTF-8 once percent-decoded —
    /// `Path<T>` reports that as a 400 rather than guessing at a replacement
    /// character.
    pub fn path_params(&self) -> Option<&PathParams> {
        self.0.extensions.get::<PathParams>()
    }
}

/// The correlation id for a request head.
///
/// In order: the `Ulid` the `request_id` middleware inserted, the
/// [`RequestId`](crate::extract::RequestId) extension an application's own
/// middleware may have inserted instead, a well-formed ULID in the
/// `x-request-id` header, and finally a fresh one.
///
/// The header is only adopted when it parses as a ULID. A client-supplied id
/// ends up in log lines and in problem documents, so accepting arbitrary text
/// here would be a log-injection surface; the `request_id` middleware applies
/// the same rule with its own configured length and character checks.
fn request_id_for(parts: &http::request::Parts) -> Ulid {
    if let Some(id) = parts.extensions.get::<Ulid>() {
        return *id;
    }
    if let Some(id) = parts.extensions.get::<crate::extract::RequestId>() {
        return id.0;
    }
    parts
        .headers
        .get(crate::REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Ulid::from_string(value).ok())
        .unwrap_or_else(crate::middleware::request_id::generate)
}

/// Read Axum's captured path parameters out of an extension map, in Moso's
/// shape, and hand the map back.
///
/// Axum stores the captures in a crate-private `UrlParams` type, reachable only
/// through `RawPathParams`, whose only constructor is a `FromRequestParts` impl
/// that wants a `&mut Parts`. [`RequestCtx::new`] has a `&Parts`. Rather than
/// widen a public signature — or clone the head a second time — the extension
/// map (which the context has to take a copy of anyway) is lent to a throwaway
/// `Parts` for the length of one call and moved straight back out. Nothing is
/// copied that was not already going to be.
///
/// `RawPathParams::from_request_parts` does not await; `now_or_never` is how
/// that is stated rather than assumed, and a future that somehow did suspend
/// yields `None` instead of blocking.
fn take_path_params(extensions: http::Extensions) -> (http::Extensions, Option<PathParams>) {
    use axum::extract::{FromRequestParts, RawPathParams};
    use futures_util::future::FutureExt as _;

    let (mut scratch, _body) = http::Request::new(()).into_parts();
    scratch.extensions = extensions;

    let captured = match RawPathParams::from_request_parts(&mut scratch, &()).now_or_never() {
        Some(Ok(raw)) => raw
            .iter()
            .map(|(name, value)| (Arc::<str>::from(name), value.to_owned()))
            .collect::<Vec<_>>(),
        // Either the route captured nothing (no `UrlParams` extension) or a
        // capture was invalid UTF-8. Both are "no usable parameters here".
        _ => Vec::new(),
    };

    let params = (!captured.is_empty()).then(|| PathParams::new(captured));
    (scratch.extensions, params)
}

impl core::fmt::Debug for RequestCtx {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RequestCtx")
            .field("request_id", &self.0.request_id)
            .field("method", &self.0.method)
            .field("path", &self.0.uri.path())
            .finish_non_exhaustive()
    }
}

/// The path parameters captured for the matched route.
///
/// A slice of `(name, value)` in declaration order, which is what both the
/// struct form (`Path<ListParams>`) and the tuple form (`Path<(Uuid, String)>`)
/// need. Percent-decoding has already happened.
#[derive(Debug, Clone, Default)]
pub struct PathParams(Vec<(Arc<str>, String)>);

impl PathParams {
    /// Build from an ordered list of captures.
    pub fn new(params: Vec<(Arc<str>, String)>) -> Self {
        Self(params)
    }

    /// The value captured for `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| &**key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The value at `index`, for the tuple form.
    pub fn nth(&self, index: usize) -> Option<&str> {
        self.0.get(index).map(|(_, value)| value.as_str())
    }

    /// The captured names, in declaration order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(name, _)| &**name)
    }

    /// How many parameters the route captured.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the route captured nothing.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A response built by the framework rather than by a handler.
///
/// Returned by the handler adapter when extraction fails, so the layer above
/// can tell a handler's own 404 from the framework's.
pub type FrameworkResponse = Response;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limits_match_the_documented_table() {
        let limits = Limits::default();
        assert_eq!(limits.body_max, 2 * 1024 * 1024);
        assert_eq!(limits.multipart_max, 32 * 1024 * 1024);
        assert_eq!(limits.uri_max, 8 * 1024);
        assert_eq!(limits.json_depth_max, 64);
    }

    #[test]
    fn request_ctx_is_clone_and_send() {
        fn assert_clone_send<T: Clone + Send + Sync>() {}
        assert_clone_send::<RequestCtx>();
    }

    #[test]
    fn path_params_look_up_by_name_and_position() {
        let params = PathParams::new(vec![
            (Arc::from("id"), "42".to_owned()),
            (Arc::from("slug"), "hello".to_owned()),
        ]);
        assert_eq!(params.get("slug"), Some("hello"));
        assert_eq!(params.nth(0), Some("42"));
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn path_params_report_an_absent_capture() {
        let params = PathParams::default();
        assert!(params.is_empty());
        assert_eq!(params.get("id"), None);
        assert_eq!(params.nth(0), None);
        assert_eq!(params.names().count(), 0);
    }

    // ── limits ────────────────────────────────────────────────────────────

    #[test]
    fn limits_are_copied_from_the_http_config() {
        let config = HttpConfig {
            body_max: 1024,
            multipart_max: 2048,
            multipart_file_max: 512,
            header_max_count: 7,
            header_max_bytes: 99,
            uri_max: 128,
            query_depth_max: 3,
            json_depth_max: 5,
            ..HttpConfig::default()
        };

        let limits = Limits::from_config(&config);

        assert_eq!(limits.body_max, 1024);
        assert_eq!(limits.multipart_max, 2048);
        assert_eq!(limits.multipart_file_max, 512);
        assert_eq!(limits.header_max_count, 7);
        assert_eq!(limits.header_max_bytes, 99);
        assert_eq!(limits.uri_max, 128);
        assert_eq!(limits.query_depth_max, 3);
        assert_eq!(limits.json_depth_max, 5);
    }

    #[test]
    fn the_default_http_config_implies_the_default_limits() {
        assert_eq!(HttpConfig::default().limits(), Limits::DEFAULT);
    }

    // ── the head limits ───────────────────────────────────────────────────

    fn uri(value: &str) -> Uri {
        value.parse().expect("a URI")
    }

    fn header_map(fields: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in fields {
            headers.append(
                http::HeaderName::from_bytes(name.as_bytes()).expect("a header name"),
                http::HeaderValue::from_str(value).expect("a header value"),
            );
        }
        headers
    }

    #[test]
    fn the_request_target_is_measured_without_rendering_the_uri() {
        assert_eq!(request_target_len(&uri("/posts")), "/posts".len());
        assert_eq!(
            request_target_len(&uri("/posts?page=2")),
            "/posts?page=2".len()
        );
        // Absolute form: the scheme and the authority are part of the target.
        assert_eq!(
            request_target_len(&uri("http://example.test/posts")),
            "http://example.test/posts".len()
        );
        // Asterisk form has no path and query; one byte was on the wire.
        assert_eq!(request_target_len(&uri("*")), 1);
    }

    #[test]
    fn header_bytes_counts_names_and_values_only() {
        let headers = header_map(&[("a", "1"), ("bb", "22")]);
        assert_eq!(header_bytes(&headers), 1 + 1 + 2 + 2);

        // A repeated header is two fields, and both are counted.
        let headers = header_map(&[("accept-encoding", "gzip"), ("accept-encoding", "br")]);
        assert_eq!(headers.len(), 2);
        assert_eq!(header_bytes(&headers), 15 + 4 + 15 + 2);
    }

    #[test]
    fn a_head_inside_every_limit_is_accepted() {
        let limits = Limits::DEFAULT;
        assert!(
            limits
                .check_head(&uri("/posts?page=2"), &header_map(&[("accept", "*/*")]))
                .is_ok()
        );
    }

    #[test]
    fn an_over_long_request_target_is_a_414_naming_the_limit() {
        let limits = Limits {
            uri_max: 8,
            ..Limits::DEFAULT
        };
        let error = limits
            .check_head(&uri("/posts?page=2"), &HeaderMap::new())
            .expect_err("the target is 13 bytes against a limit of 8");

        assert_eq!(error.status(), http::StatusCode::URI_TOO_LONG);
        assert_eq!(error.extensions()["max_bytes"], serde_json::json!(8));
    }

    #[test]
    fn too_many_header_fields_are_a_431() {
        let limits = Limits {
            header_max_count: 1,
            ..Limits::DEFAULT
        };
        let error = limits
            .check_head(&uri("/"), &header_map(&[("a", "1"), ("b", "2")]))
            .expect_err("two fields against a limit of one");

        assert_eq!(
            error.status(),
            http::StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );
        assert_eq!(error.extensions()["max_count"], serde_json::json!(1));
    }

    #[test]
    fn over_large_headers_are_a_431_reporting_the_byte_limit() {
        let limits = Limits {
            header_max_bytes: 8,
            ..Limits::DEFAULT
        };
        let error = limits
            .check_head(&uri("/"), &header_map(&[("x-long", "0123456789")]))
            .expect_err("16 bytes against a limit of 8");

        assert_eq!(
            error.status(),
            http::StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );
        assert_eq!(error.extensions()["max_bytes"], serde_json::json!(8));
    }

    #[test]
    fn the_count_is_checked_before_the_bytes_are_summed() {
        // Both limits are broken; the count wins, because it is the check that
        // does not walk the flood.
        let limits = Limits {
            header_max_count: 1,
            header_max_bytes: 1,
            ..Limits::DEFAULT
        };
        let error = limits
            .check_head(&uri("/"), &header_map(&[("a", "1"), ("b", "2")]))
            .expect_err("both limits are broken");
        assert!(error.extensions().contains_key("max_count"));
    }

    // ── validation messages ───────────────────────────────────────────────

    fn ctx_with(headers: &[(&str, &str)]) -> RequestCtx {
        let mut builder = http::Request::builder().uri("/posts");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let (parts, ()) = builder.body(()).expect("a request head").into_parts();
        RequestCtx::new(Arc::new(AppState::for_tests()), &parts)
    }

    #[test]
    fn the_locale_comes_from_accept_language() {
        let ctx = ctx_with(&[("accept-language", "fr;q=0.8, en-GB;q=0.9")]);
        assert_eq!(
            ctx.locale().as_ref().map(Locale::as_str),
            Some("en-GB"),
            "the highest quality entry wins"
        );
    }

    #[test]
    fn a_malformed_accept_language_degrades_to_no_locale() {
        for header in ["*", "", "en_US", ",,;;", "fr;q=high"] {
            let ctx = ctx_with(&[("accept-language", header)]);
            assert_eq!(ctx.locale(), None, "{header:?} is not a usable locale");
        }
        assert_eq!(ctx_with(&[]).locale(), None);
    }

    #[test]
    fn with_no_provider_registered_the_context_is_bare() {
        let ctx = ctx_with(&[("accept-language", "fr")]);
        let validation = ctx.validation(crate::extract::QUERY_POINTER_ROOT);

        assert_eq!(validation.pointer(), "/query");
        assert!(validation.messages().is_none());
        assert!(
            validation.locale().is_none(),
            "the header is not even parsed when nothing could consult it"
        );
    }

    /// The smallest thing that satisfies `App::new`.
    #[derive(Debug, Clone)]
    struct TestConfig;

    impl crate::config::Config for TestConfig {
        fn descriptor() -> &'static crate::config::ConfigDescriptor {
            static DESCRIPTOR: crate::config::ConfigDescriptor = crate::config::ConfigDescriptor {
                type_name: "TestConfig",
                fields: &[],
            };
            &DESCRIPTOR
        }

        fn load_nested(
            _loader: &crate::config::ConfigLoader,
            _prefix: &crate::config::ConfigKey,
            _errors: &mut crate::error::BootErrors,
        ) -> Option<Self> {
            Some(TestConfig)
        }
    }

    /// Answers one code, in one language, and falls through for everything
    /// else — which is the shape a real application's provider has.
    struct French;

    impl MessageProvider for French {
        fn message(
            &self,
            code: &str,
            _params: &std::collections::BTreeMap<&'static str, serde_json::Value>,
            locale: &Locale,
        ) -> Option<String> {
            (locale.language() == "fr" && code == moso_schema::codes::LEN)
                .then(|| "trop court".to_owned())
        }
    }

    /// A context for a request against an application that registered `French`.
    fn ctx_with_provider(headers: &[(&str, &str)]) -> RequestCtx {
        let app = crate::App::new(TestConfig)
            .middleware(crate::middleware::MiddlewareStack::bare())
            .provide_dyn::<dyn MessageProvider>(Arc::new(French))
            .build()
            .expect("an application with one provider and no routes");

        let mut builder = http::Request::builder().uri("/posts");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let (parts, ()) = builder.body(()).expect("a request head").into_parts();
        RequestCtx::new(Arc::clone(app.state()), &parts)
    }

    #[test]
    fn a_registered_provider_reaches_the_validation_context() {
        let ctx = ctx_with_provider(&[("accept-language", "fr-CA, en;q=0.5")]);
        let validation = ctx.validation("");

        assert!(validation.messages().is_some());
        assert_eq!(validation.locale().map(Locale::as_str), Some("fr-CA"));
        assert_eq!(
            validation.message(moso_schema::codes::LEN, &std::collections::BTreeMap::new()),
            "trop court"
        );
    }

    #[test]
    fn a_provider_that_declines_falls_through_to_the_bundled_english() {
        let ctx = ctx_with_provider(&[("accept-language", "fr")]);
        let validation = ctx.validation("");

        assert_eq!(
            validation.message(
                moso_schema::codes::REQUIRED,
                &std::collections::BTreeMap::new()
            ),
            "this field is required"
        );
    }

    #[test]
    fn a_request_that_asks_for_no_locale_still_reaches_the_provider() {
        // A provider registered to *reword* rather than translate has no reason
        // to need a locale, so an absent `Accept-Language` must not skip it.
        let ctx = ctx_with_provider(&[]);
        let validation = ctx.validation("");

        assert!(validation.messages().is_some());
        assert!(validation.locale().is_none());
    }

    // ── the path-parameter bridge ─────────────────────────────────────────

    #[test]
    fn a_route_without_captures_has_no_path_params() {
        let (extensions, params) = take_path_params(http::Extensions::new());
        assert!(params.is_none());
        assert!(extensions.is_empty());
    }

    #[test]
    fn the_extension_map_survives_the_path_parameter_read() {
        let mut extensions = http::Extensions::new();
        extensions.insert(7u32);
        let (extensions, params) = take_path_params(extensions);
        assert!(params.is_none());
        assert_eq!(extensions.get::<u32>(), Some(&7));
    }

    // ── the request id ────────────────────────────────────────────────────

    fn parts_with(headers: &[(&str, &str)]) -> http::request::Parts {
        let mut builder = http::Request::builder().uri("/users/42");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder
            .body(())
            .expect("a valid request head")
            .into_parts()
            .0
    }

    #[test]
    fn the_middleware_supplied_id_wins() {
        let expected = Ulid::generate();
        let mut parts = parts_with(&[(crate::REQUEST_ID_HEADER, &Ulid::generate().to_string())]);
        parts.extensions.insert(expected);

        assert_eq!(request_id_for(&parts), expected);
    }

    #[test]
    fn a_well_formed_header_is_adopted_when_there_is_no_extension() {
        let expected = Ulid::generate();
        let parts = parts_with(&[(crate::REQUEST_ID_HEADER, &expected.to_string())]);

        assert_eq!(request_id_for(&parts), expected);
    }

    #[test]
    fn a_malformed_header_is_replaced_rather_than_trusted() {
        let parts = parts_with(&[(crate::REQUEST_ID_HEADER, "not-a-ulid\\n injected log line")]);

        // No panic, no adoption: a fresh id, generated twice over to prove it
        // is not a constant.
        let first = request_id_for(&parts);
        let second = request_id_for(&parts);
        assert_ne!(first, second);
    }

    #[test]
    fn a_head_with_no_id_at_all_still_gets_one() {
        let parts = parts_with(&[]);
        assert_ne!(request_id_for(&parts), Ulid::nil());
    }

    // ── the dependency cache ──────────────────────────────────────────────

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The cache is generic over the error type precisely so these tests do not
    /// need to build a framework `Error`.
    type TestResult<T> = core::result::Result<T, &'static str>;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct User(&'static str);

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Tenant(u32);

    #[tokio::test]
    async fn a_dependency_resolved_twice_runs_once() {
        let cache = DependencyCache::new();
        let runs = AtomicUsize::new(0);

        let init = || async {
            runs.fetch_add(1, Ordering::SeqCst);
            TestResult::Ok(User("ada"))
        };

        let first = cache.get_or_init::<User, _, _, _>(init).await;
        let second = cache.get_or_init::<User, _, _, _>(init).await;

        assert_eq!(first, Ok(Some(User("ada"))));
        assert_eq!(second, Ok(Some(User("ada"))));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "resolve ran more than once");
        assert_eq!(cache.len(), 1);
        assert!(cache.contains::<User>());
    }

    #[tokio::test]
    async fn concurrent_askers_share_one_resolution() {
        let cache = DependencyCache::new();
        let runs = AtomicUsize::new(0);

        // The yield is what makes this a real test: without single-flight, the
        // second future observes an empty cell while the first is suspended and
        // starts its own query.
        let init = || async {
            runs.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            TestResult::Ok(User("ada"))
        };

        let (first, second) = tokio::join!(
            cache.get_or_init::<User, _, _, _>(init),
            cache.get_or_init::<User, _, _, _>(init),
        );

        assert_eq!(first, Ok(Some(User("ada"))));
        assert_eq!(second, Ok(Some(User("ada"))));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the query ran twice");
        assert_eq!(cache.len(), 1);
    }

    #[tokio::test]
    async fn a_failed_resolution_is_not_memoised() {
        let cache = DependencyCache::new();
        let runs = AtomicUsize::new(0);

        let init = || async {
            if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("the database was asleep")
            } else {
                Ok(User("ada"))
            }
        };

        assert_eq!(
            cache.get_or_init::<User, _, _, _>(init).await,
            Err("the database was asleep")
        );
        // The slot exists, but it holds nothing, so a retry within the same
        // request is possible.
        assert_eq!(cache.len(), 1);
        assert!(!cache.contains::<User>());

        assert_eq!(
            cache.get_or_init::<User, _, _, _>(init).await,
            Ok(Some(User("ada")))
        );
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert!(cache.contains::<User>());
    }

    #[tokio::test]
    async fn distinct_types_do_not_share_a_slot() {
        let cache = DependencyCache::new();

        let user = cache
            .get_or_init::<User, _, _, _>(|| async { TestResult::Ok(User("ada")) })
            .await;
        let tenant = cache
            .get_or_init::<Tenant, _, _, _>(|| async { TestResult::Ok(Tenant(9)) })
            .await;

        assert_eq!(user, Ok(Some(User("ada"))));
        assert_eq!(tenant, Ok(Some(Tenant(9))));
        assert_eq!(cache.len(), 2);
        assert!(cache.contains::<User>());
        assert!(cache.contains::<Tenant>());
    }

    #[test]
    fn a_fresh_cache_holds_nothing() {
        let cache = DependencyCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
        assert!(!cache.contains::<User>());
    }

    #[test]
    fn the_slot_for_a_type_is_created_once_and_reused() {
        let cache = DependencyCache::new();

        let first = cache.cell_for(TypeId::of::<User>());
        let second = cache.cell_for(TypeId::of::<User>());
        let other = cache.cell_for(TypeId::of::<Tenant>());

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &other));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn a_poisoned_lock_does_not_take_the_cache_down() {
        let cache = Arc::new(DependencyCache::new());
        cache.cell_for(TypeId::of::<User>());

        // The one way to poison a `std::sync::Mutex` is to panic while holding
        // its guard, so this test prints a panic line to stderr on purpose.
        let poisoner = Arc::clone(&cache);
        let panicked = std::thread::spawn(move || {
            let _guard = poisoner.slots.lock().expect("not yet poisoned");
            panic!("something unrelated went wrong while the guard was held");
        })
        .join();
        assert!(panicked.is_err());

        // The vector behind the lock is intact, so the request carries on.
        assert_eq!(cache.len(), 1);
        assert!(!cache.contains::<User>());
        cache.cell_for(TypeId::of::<Tenant>());
        assert_eq!(cache.len(), 2);
    }
}
