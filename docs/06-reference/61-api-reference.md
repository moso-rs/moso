# 61 — Public API Reference

> Every public trait and the signature of every principal type, in one place.
>
> **This file now reflects the shipped code**, not the original design sketch. Where a design
> document still disagrees with a signature here, the *code* wins and that document is being
> corrected; see [`63-implementation-status.md`](63-implementation-status.md) for the ledger.
>
> Notation: `async fn` in a trait means a native RPITIT (`-> impl Future<Output = …> + Send + 'a`),
> not `#[async_trait]`, unless stated. `BoxFuture<'a, T>` is
> `Pin<Box<dyn Future<Output = T> + Send + 'a>>` and is used where a trait must be dyn-compatible.

---

## `moso-core`

### Application

```rust
pub struct App;
impl App {
    pub fn new<C: Config>(config: C) -> AppBuilder;
    pub async fn serve(self) -> Result<()>;
    pub async fn serve_on(self, listener: tokio::net::TcpListener) -> Result<()>;
    pub async fn serve_workers(self) -> Result<()>;
    pub fn into_service(self) -> axum::Router<()>;
    pub fn openapi(&self) -> &openapi::Document;        // NOT cfg-gated — see D1
    pub fn router_info(&self) -> &[RouteInfo];
    pub fn state(&self) -> &Arc<AppState>;
    pub fn resolver(&self) -> Resolver;
    pub fn shutdown_signal(&self) -> Signal;
}

pub struct AppBuilder;
impl AppBuilder {
    pub fn provide<T: Send + Sync + 'static>(self, value: T) -> Self;
    pub fn provide_arc<T: Send + Sync + 'static>(self, value: Arc<T>) -> Self;
    pub fn provide_with<T, F, Fut>(self, f: F) -> Self
        where F: FnOnce(Resolver) -> Fut + Send + 'static,
              Fut: Future<Output = Result<T>> + Send + 'static,
              T: Send + Sync + 'static;
    pub fn provide_dyn<T: ?Sized + Send + Sync + 'static>(self, value: Arc<T>) -> Self;
    pub fn mount(self, router: Router) -> Self;
    pub fn mount_at(self, prefix: &'static str, router: Router) -> Self;
    pub fn mount_axum(self, prefix: &'static str, router: axum::Router<()>) -> Self;
    pub fn middleware(self, stack: MiddlewareStack) -> Self;
    pub fn with_middleware(self, f: impl FnOnce(&mut MiddlewareStack)) -> Self;
    pub fn on_startup<F, Fut>(self, f: F) -> Self;
    pub fn on_shutdown<F, Fut>(self, f: F) -> Self;
    pub fn lifespan<F, Fut, G>(self, f: F) -> Self;
    pub fn health_check(self, name: &'static str, c: impl HealthCheck) -> Self;
    pub fn openapi(self, f: impl FnOnce(&mut DocumentBuilder)) -> Self;
    pub fn http_config(self, config: HttpConfig) -> Self;
    pub fn server_config(self, config: ServerConfig) -> Self;
    pub fn profile(self, profile: Profile) -> Self;
    pub fn secret_provider(self, provider: Arc<dyn SecretProvider>) -> Self;
    pub fn build(self) -> Result<App>;
    pub fn build_unchecked(self) -> App;                 // tests only; skips boot validation
    // introspection before build()
    pub fn router(&self) -> &Router;
    pub fn middleware_stack(&self) -> &MiddlewareStack;
    pub fn errors(&self) -> &BootErrors;
    pub fn secret_providers(&self) -> &[Arc<dyn SecretProvider>];
}

pub struct AppState;
impl AppState {
    pub fn providers(&self) -> &ProviderMap;
    pub fn http(&self) -> &HttpConfig;
    pub fn server(&self) -> &ServerConfig;
    pub fn profile(&self) -> Profile;
    pub fn shutdown(&self) -> &Signal;
    pub fn drain(&self) -> &Drain;
    pub fn blocking(&self) -> &BlockingPool;
    pub fn uptime(&self) -> Duration;
    pub fn document(&self) -> &Document;
    pub fn health_checks(&self) -> &[(&'static str, Arc<dyn HealthCheck>)];
}

pub struct Resolver;
impl Resolver {
    pub fn new(providers: Arc<ProviderMap>) -> Self;
    pub fn get<T: Send + Sync + 'static>(&self) -> Result<Arc<T>>;      // Arc, not &T
    pub fn get_arc<T: Send + Sync + 'static>(&self) -> Result<Arc<T>>;
    pub fn get_dyn<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>>;
    pub fn config<C: Config>(&self) -> Result<Arc<C>>;                  // Result<Arc<C>>, not &C
    pub fn has<T: ?Sized + 'static>(&self) -> bool;
}

/// Values a lifespan hook must keep alive for the process's lifetime.
pub struct Lifespan;
impl Lifespan {
    pub fn new() -> Self;
    pub fn push<G: Send + 'static>(&mut self, guard: G);
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn release(&mut self);
}

/// Dyn-compatible, so `AppState` can hold `Arc<dyn HealthCheck>`.
pub trait HealthCheck: Send + Sync + 'static {
    fn check<'a>(&'a self, resolver: &'a Resolver) -> BoxFuture<'a, HealthStatus>;
    fn critical(&self) -> bool { true }
}
pub enum HealthStatus { Up, Degraded(String), Down(String) }
```

**Deviations from the original sketch, and why:**
`App::openapi` is not `#[cfg(feature = "openapi")]` (D1). `Resolver::get`/`config` return
`Result<Arc<T>>` rather than `Result<&T>` — the provider map stores `Arc<dyn Any>`, so handing out a
reference would tie the borrow to the resolver rather than to the value, which breaks the
`provide_with` factory closures that need to hold what they resolved. `HealthCheck::check` is boxed
because the trait must be dyn-compatible.

### Routing

```rust
pub struct Router;
impl Router {
    pub fn new() -> Self;
    pub fn get<H, M>(self, path: &'static str, h: H) -> Self where H: Handler<M>, M: 'static;
    // post, put, patch, delete, head, options — identical shape
    pub fn method<H, M>(self, m: HttpMethod, path: &'static str, h: H) -> Self;
    pub fn endpoint<E>(self, m: HttpMethod, path: &'static str) -> Self
        where E: Endpoint + HandlerFn + Clone + Default;
    pub fn route(self, path: &'static str, m: MethodRouter) -> Self;
    pub fn nest(self, prefix: &'static str, r: Router) -> Self;
    pub fn merge(self, r: Router) -> Self;
    pub fn static_files(self, path: &'static str, src: StaticSource) -> Self;
    pub fn tag(self, tag: &'static str) -> Self;
    pub fn security(self, s: SecurityRequirement) -> Self;
    pub fn deprecated(self) -> Self;
    pub fn hidden(self) -> Self;
    pub fn responds(self, status: u16, spec: ResponseSpec) -> Self;
    pub fn layer<L>(self, layer: L) -> Self
        where L: tower::Layer<Route> + Clone + Send + Sync + 'static, /* + Service bounds */;
    pub fn guard<G: Guard>(self, guard: G) -> Self;
    pub fn timeout(self, timeout: Duration) -> Self;
    pub fn fallback<H, M>(self, h: H) -> Self where H: Handler<M>, M: 'static;
    pub fn method_not_allowed<H, M>(self, h: H) -> Self where H: Handler<M>, M: 'static;
    pub fn mount_axum(self, prefix: &'static str, r: axum::Router<()>) -> Self;
    pub fn into_axum(self) -> axum::Router<()>;
    // introspection
    pub fn entries(&self) -> &[RouteEntry];
    pub fn into_entries(self) -> Vec<RouteEntry>;
    pub fn metadata(&self) -> &RouteMetadata;
    pub fn describe(&self) -> Vec<RouteInfo>;
    pub fn conflicts(&self) -> Vec<BootError>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn fallback_handler(&self) -> Option<&BoxedHandler>;
    pub fn method_not_allowed_handler(&self) -> Option<&BoxedHandler>;
    pub fn axum_mounts(&self) -> &[(String, axum::Router<()>)];
    pub fn static_mounts(&self) -> &[(String, StaticSource)];
}

pub struct MethodRouter;
impl MethodRouter {
    pub fn new() -> Self;
    pub fn on<H, M>(self, m: HttpMethod, h: H) -> Self;
    pub fn get<H, M>(self, h: H) -> Self;   // post, put, patch, delete
    pub fn methods(&self) -> Vec<HttpMethod>;
}
pub fn get<H, M>(h: H) -> MethodRouter;     // free fns: post, put, patch, delete

pub type Route = tower::util::BoxCloneSyncService<Request, Response, core::convert::Infallible>;
pub type RouteService = Route;

pub struct RouteEntry { /* method, path, spec, providers, handler, layers, guards */ }
impl RouteEntry {
    pub fn describe(&self, op: &mut OperationBuilder);
    pub fn into_service(self) -> Route;
    pub fn path_parameters(&self) -> Vec<&str>;
}

pub struct RouteInfo { /* what `moso routes` prints */ }

pub enum StaticSource { Dir { .. }, Embedded { .. } }
impl StaticSource {
    pub fn dir(root: impl Into<PathBuf>) -> Self;
    pub fn spa(root: impl Into<PathBuf>, fallback: impl Into<String>) -> Self;
    pub fn embedded(files: &'static [EmbeddedFile]) -> Self;
}
pub struct EmbeddedFile { pub path: &'static str, pub bytes: &'static [u8], .. }

/// Compile-time path validation; `route_path!("/users/{id}")` wraps it.
pub const fn validate_path(path: &'static str) -> &'static str;
pub fn path_parameters(path: &str) -> Vec<&str>;
```

### Handlers

```rust
pub trait Endpoint: 'static {
    const NAME: &'static str;
    fn spec(b: &mut OperationBuilder);
    fn required_providers() -> &'static [ProviderReq];
}

pub trait HandlerFn: Send + Sync + 'static {
    fn invoke(req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response>;
}

pub trait Handler<M>: Clone + Send + Sync + 'static {
    type Endpoint: Endpoint;
    fn call(self, req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response>;
}

// marker types for the three impl families
pub struct EndpointMarker;  pub struct PartsOnly;  pub struct WithBody;
pub struct UndocumentedEndpoint;    // Handler::Endpoint for a plain `async fn`
pub struct HandlerAdapter<H, M>;    // the tower::Service a route stores

pub trait ErasedHandler: Send + Sync + 'static {
    fn call_erased(&self, req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response>;
    fn describe(&self, op: &mut OperationBuilder);
    fn required_providers(&self) -> &'static [ProviderReq];
    fn name(&self) -> &'static str;
}
pub type BoxedHandler = Arc<dyn ErasedHandler>;
pub fn boxed<H: Handler<M>, M: 'static>(h: H) -> BoxedHandler;

pub const MAX_HANDLER_PARAMS: usize = 16;
pub const UNDOCUMENTED_EXTENSION: &str = "x-moso-undocumented";

#[macro_export] macro_rules! concat_reqs;   // const-evaluable &[ProviderReq] concatenation
#[macro_export] macro_rules! route_path;    // const path check at the literal's span
```

### Extraction & response

```rust
pub trait Extract: Sized + Send {
    fn describe(op: &mut OperationBuilder);
    const PROVIDER_REQ: &'static [ProviderReq] = &[];
    fn extract<'a>(parts: &'a mut Parts, ctx: &'a RequestCtx)
        -> impl Future<Output = Result<Self>> + Send + 'a;
}

pub trait ExtractBody: Sized + Send {
    fn describe(op: &mut OperationBuilder);
    const PROVIDER_REQ: &'static [ProviderReq] = &[];
    fn extract_body<'a>(req: Request, ctx: &'a RequestCtx)
        -> impl Future<Output = Result<Self>> + Send + 'a;
}
// NOTE: there is NO blanket `impl<T: Extract> ExtractBody for T` — coherence forbids it.

pub trait Describe { fn describe(op: &mut OperationBuilder); }
pub use axum::response::IntoResponse;

/// Dyn-compatible: the route table stores `Arc<dyn DynGuard>`.
pub trait Guard: Clone + Send + Sync + 'static {
    fn describe(&self, op: &mut OperationBuilder);              // &self, not an associated fn
    fn check<'a>(&'a self, parts: &'a Parts, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<()>>;
}
pub trait DynGuard: Send + Sync + 'static { /* blanket impl for every `G: Guard` */ }

// Axum interop — by wrapper, not by blanket impl (orphan rule)
pub struct Opaque<T>(pub T);        // Axum FromRequestParts → Moso Extract
pub struct OpaqueBody<T>(pub T);    // Axum FromRequest      → Moso ExtractBody
pub struct MosoExt<T>(pub T);       // Moso Extract     → Axum FromRequestParts
pub struct MosoExtBody<T>(pub T);   // Moso ExtractBody → Axum FromRequest
pub fn ctx_from_parts(parts: &Parts) -> Result<RequestCtx>;
pub fn axum_rejection<R: axum::response::IntoResponse>(rejection: R) -> Error;

// extractors
pub struct Path<T>(pub T);
pub struct Query<T>(pub T);          pub struct QueryMap;  pub enum QueryValue { .. }
pub struct Headers<T>(pub T);
pub struct Json<T>(pub T);           // also a response type
pub struct Form<T>(pub T);
pub struct Inject<T: ?Sized + 'static>(pub Arc<T>);
pub struct Depends<T: Dependency>(pub T);
pub struct Cookies;                  pub struct SignedCookies;  pub struct PrivateCookies;
pub struct CookieKey;                pub struct CookieJar;      pub struct Cookie;
pub struct CookieDefaults { pub secure: bool }                  pub enum SameSite { .. }
pub struct RequestId(pub Ulid);      pub struct ClientIp(pub IpAddr);
pub struct ConnectInfo<T>(pub T);    pub struct Extension<T>(pub T);
pub struct MatchedPath(pub Arc<str>);
pub struct Bytes(pub bytes::Bytes);  pub struct Text(pub String);
pub struct BodyStream(..);           pub struct RawBody(pub axum::body::Body);
#[cfg(feature = "multipart")] pub struct Multipart;  pub struct Field;  pub struct MultipartLimits;

pub async fn read_limited(req: Request, max: usize) -> Result<Vec<u8>>;
pub async fn read_body_limited(body: axum::body::Body, headers: &HeaderMap, max: usize)
    -> Result<Vec<u8>>;

// responses
pub struct Created<T>;      pub struct Accepted<T>;
pub struct NoContent;       pub type Empty = NoContent;
pub struct Page<T>;         pub struct PageLinks;
pub struct Redirect;        pub struct File;           pub struct Attachment;
pub struct Sse<S>;          pub struct Event;
pub struct Raw<T>;          pub struct Cached<T>;      pub struct ETag;
pub struct Html;            // `Text` and `Bytes` above are both extractor and response
pub enum Either<A, B> { A(A), B(B) }

pub fn json_response<T: Schema>(status: StatusCode, value: &T) -> Response;
pub fn describe_json<T: Schema>(op: &mut OperationBuilder, status: u16);
pub fn empty_response(status: StatusCode) -> Response;
pub fn set_header(response: &mut Response, name: HeaderName, value: &str);
```

**Not present, though the original reference listed them:** `Upload<T>` (needs `moso-storage`),
`Multipart` outside the `multipart` feature, `Stream`/`String` as body extractor names (they are
`BodyStream`/`Text`), `Router::negotiate`, and a blanket
`impl<T: Extract> axum::extract::FromRequestParts<()> for T`.

### Dependency injection

```rust
pub trait Dependency: Clone + Send + Sync + 'static {
    const PROVIDER_REQ: &'static [ProviderReq] = &[];
    fn describe(op: &mut OperationBuilder) { let _ = op; }
    fn resolve<'a>(ctx: &'a RequestCtx) -> impl Future<Output = Result<Self>> + Send + 'a;
}

pub struct ProviderReq {
    pub type_id:   fn() -> TypeId,
    pub type_name: fn() -> &'static str,     // a fn pointer, NOT &'static str
    pub optional:  bool,
}
impl ProviderReq {
    pub const fn of<T: ?Sized + 'static>() -> Self;
    pub const fn optional_of<T: ?Sized + 'static>() -> Self;
    pub fn id(&self) -> TypeId;
    pub fn name(&self) -> &'static str;
}

pub struct ProviderMap;  pub struct ProviderMapBuilder;
pub fn missing_provider_error(req: &ProviderReq) -> Error;

pub struct RequestCtx;                       // Clone; an Arc<RequestCtxInner> inside
impl RequestCtx {
    pub fn provider<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>>;
    pub fn try_provider<T: ?Sized + Send + Sync + 'static>(&self) -> Option<Arc<T>>;
    pub fn config<C: Config>(&self) -> Result<Arc<C>>;
    pub fn depends<D: Dependency>(&self) -> BoxFuture<'_, Result<D>>;   // boxed: recursive
    pub fn cache(&self) -> &DependencyCache;
    pub fn headers(&self) -> &HeaderMap;
    pub fn method(&self) -> &Method;
    pub fn uri(&self) -> &Uri;
    pub fn version(&self) -> Version;
    pub fn path(&self) -> &str;
    pub fn matched_path(&self) -> Option<&str>;
    pub fn extension<T: Clone + Send + Sync + 'static>(&self) -> Option<T>;
    pub fn request_id(&self) -> &Ulid;
    pub fn limits(&self) -> &Limits;
    pub fn cookies(&self) -> &Cookies;            // this request's one jar, created on first ask
    pub fn cookies_if_used(&self) -> Option<&Cookies>;   // what the adapter drains after the handler
    pub fn state(&self) -> &Arc<AppState>;
    pub fn shutdown(&self) -> &Signal;
    pub fn path_params(&self) -> Option<&PathParams>;
}
```

`ProviderReq::type_name` is a **function pointer** because `core::any::type_name` is not
`const`-stable on the pinned toolchain (1.97.1), and `ProviderReq::of` must be `const` so that
`concat_reqs!` can build a `&'static [ProviderReq]` at compile time. Read it with `req.name()`.

### Errors

```rust
pub type Result<T, E = Error> = core::result::Result<T, E>;

pub struct Error(Box<ErrorInner>);           // boxed: unboxed it is 264 bytes and clippy's
                                             // `result_large_err` fires on every handler
#[non_exhaustive] pub enum ErrorKind {
    BadRequest, Unauthenticated, Forbidden, NotFound, MethodNotAllowed, NotAcceptable, Conflict,
    Gone, PreconditionFailed, PayloadTooLarge, UriTooLong, UnsupportedMedia, RangeNotSatisfiable,
    Validation, Locked, TooManyRequests, Internal, NotImplemented, BadGateway, Unavailable,
    GatewayTimeout, Timeout, Boot(BootErrors),
}
impl ErrorKind {
    pub const RESPONSE_KINDS: &'static [ErrorKind];
    pub fn status(&self) -> StatusCode;
    pub fn slug(&self) -> &'static str;
    pub fn type_uri(&self) -> &'static str;
    pub fn title(&self) -> &'static str;
    pub fn detail_is_client_safe(&self) -> bool;
    pub fn retryable(&self) -> bool;
    pub fn log_level(&self) -> tracing::Level;
}

impl Error {
    // constructors
    pub fn new(kind: ErrorKind) -> Self;
    pub fn bad_request(detail: impl Into<Cow<'static, str>>) -> Self;
    pub fn unauthenticated() -> Self;
    pub fn forbidden(detail: impl Into<Cow<'static, str>>) -> Self;
    pub fn not_found(resource: impl Into<Cow<'static, str>>) -> Self;
    pub fn method_not_allowed(allowed: &[Method]) -> Self;
    pub fn conflict(detail: impl Into<Cow<'static, str>>) -> Self;
    pub fn payload_too_large(limit: usize) -> Self;
    pub fn unsupported_media(content_type: impl Into<Cow<'static, str>>) -> Self;
    pub fn validation(errors: ValidationErrors) -> Self;
    pub fn too_many(retry_after: Duration) -> Self;
    pub fn internal(source: impl Into<BoxError>) -> Self;
    pub fn internal_msg(detail: impl Into<Cow<'static, str>>) -> Self;
    pub fn unavailable(detail: impl Into<Cow<'static, str>>) -> Self;
    pub fn timeout(after: Duration) -> Self;
    pub fn boot(errors: BootErrors) -> Self;
    pub fn from_json_path(e: serde_path_to_error::Error<serde_json::Error>) -> Self;
    pub fn from_form_path(e: serde_path_to_error::Error<serde::de::value::Error>) -> Self;
    // builders
    pub fn with_type(self, uri: &'static str) -> Self;
    pub fn with_title(self, title: impl Into<Cow<'static, str>>) -> Self;
    pub fn with_detail(self, detail: impl Into<Cow<'static, str>>) -> Self;
    pub fn with_extension(self, key: &'static str, value: impl Serialize) -> Self;
    pub fn with_field(self, pointer: &str, code: &'static str, message: &str) -> Self;
    pub fn with_fields(self, errors: ValidationErrors) -> Self;
    pub fn with_header(self, name: HeaderName, value: HeaderValue) -> Self;
    pub fn with_source(self, source: impl Into<BoxError>) -> Self;
    // accessors: kind, status, type_uri, title, detail, fields, extensions, headers,
    //            backtrace, is_client_error, is_server_error, retryable, chain
}

pub struct Problem {                          // the RFC 9457 wire shape; Serialize + Deserialize
    pub r#type: String, pub title: String, pub status: u16,
    pub detail: Option<String>, pub instance: Option<String>,
    pub errors: Option<Vec<ProblemField>>,    // an owned type, so Problem round-trips
    /* flattened extensions */
}
pub struct ProblemField { pub pointer: String, pub code: String, pub message: String, .. }
pub struct ProblemOptions { /* expose_internal_errors, request id, instance */ }

pub enum BootError { /* 12 variants, each with a rendered fix */ }
pub struct BootErrors(Vec<BootError>);
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
```

### Configuration

```rust
pub trait Config: Sized + Send + Sync + 'static {
    fn descriptor() -> &'static ConfigDescriptor;
    fn load_nested(loader: &ConfigLoader, prefix: &ConfigKey, errors: &mut BootErrors)
        -> Option<Self>;                                     // ← what the derive implements
    fn load_from(loader: &ConfigLoader) -> Result<Self> { .. }
    fn load() -> Result<Self> { .. }
}
```

The original signature was `fn load_from(sources: &[Box<dyn ConfigSource>]) -> Result<Self>`.
It was replaced because it cannot do two things the design elsewhere requires: report *every* bad
field in one run (it short-circuits on the first `?`), and support `#[config(nested)]` (it has no
key prefix to root a section at). `ConfigLoader` carries the source stack, the profile, the prefix
and the secret providers; `load_nested` accumulates into a `BootErrors` and returns `None`.

```rust
pub enum Profile { Dev, Test, Production }   // three, not four — there is no `Staging`
pub struct ConfigLoader;                    // standard(), for_profile(), from_sources(), with_*()
pub struct ConfigDescriptor;                // field metadata; drives `moso config` + .env.example
pub struct FieldDescriptor;  pub struct FieldSpec;  pub struct ConfigKey;
pub struct ResolvedConfig;   pub struct ResolvedEntry;  pub enum Origin { .. }
pub trait ConfigSource: Send + Sync { fn get(&self, key: &ConfigKey) -> Option<ConfigValue>; }
pub trait Coerce: Sized {
    const TYPE_NAME: &'static str;           // `integer in 1..=1000`, `URL` — for the boot error
    fn coerce(value: &RawValue) -> Result<Self, CoerceError>;
}
pub struct CoerceError { pub expected: String, pub found: String }
pub trait SecretProvider: Send + Sync {
    fn resolve<'a>(&'a self, r: &'a SecretRef) -> BoxFuture<'a, Result<SecretString>>;
}
pub struct SecretString;  pub struct SecretBytes;  pub struct SecretRef;
pub struct Reloadable<T>; pub fn on_sighup<F>(reload: F) -> Result<JoinHandle<()>>;

pub struct HttpConfig { /* body_max, multipart_max, multipart_file_max, header_max_count,
    header_max_bytes, uri_max, query_depth_max, json_depth_max, timeout,
    expose_internal_errors, expose_docs, docs_path, openapi_path, health_path, ready_path,
    trusted_proxies */ }
pub struct ServerConfig { /* bind, shutdown_grace, keep_alive, http2_prior_knowledge, nodelay, .. */ }
pub struct Limits { /* the HttpConfig limits, resolved once and reachable from ctx.limits() */ }
```

`SecretProvider::resolve` is boxed (dyn-compatibility), not an `async fn`.

### Middleware

```rust
pub struct MiddlewareStack;
pub enum Slot { CatchPanic, RequestId, Trace, SensitiveHeaders, CatchError, Timeout,
                BodyLimit, NormalizePath, Cors, SecurityHeaders, Compression, RateLimit,
                Session, Metrics }
impl Slot { pub const ORDER: [Slot; 14]; pub const fn as_str(self) -> &'static str; }

pub struct Next;
impl Next { pub async fn run(self, req: Request) -> Response; }

/// A Tower layer with its type erased.
///
/// `tower::Layer<S>` is generic over the service it wraps and so is not usefully object-safe. The
/// stack stores this instead: a trait that applies a layer to the one concrete service type routes
/// are erased to (`Route`). Application code never implements it — `Router::layer` and
/// `MiddlewareStack::insert_after` accept a real `tower::Layer` and wrap it.
pub trait CustomLayer: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn apply(&self, service: Route) -> Route;
    fn summary(&self) -> String { String::new() }
}
pub fn layer_fn<L>(name: &'static str, layer: L) -> Arc<dyn CustomLayer>
    where L: tower::Layer<Route> + Clone + Send + Sync + 'static,
          L::Service: tower::Service<Request, Error = Infallible> + Clone + Send + Sync + 'static;
/// Runtime support for `#[moso::middleware]` with leading extractor parameters.
pub fn middleware_ctx(parts: &Parts) -> Result<RequestCtx>;
```

`Router::layer<L>` and `MiddlewareStack` take `L: tower::Layer<Route> + Clone + Send + Sync +
'static` (the same bounds `axum::Router::layer` uses) and wrap it with `layer_fn`; `CustomLayer` is
the erased form they store.

### Other

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub use axum::extract::Request;
pub use axum::response::{IntoResponse, Response};
pub use moso_openapi as openapi;
pub use moso_schema  as schema;
pub mod deps { pub use {axum, bytes, http, serde, serde_json, tokio, tower, tower_http, tracing}; }
pub const REQUEST_ID_HEADER: &str = "x-request-id";
pub const COMPONENTS_SCHEMAS_PREFIX: &str = "#/components/schemas/";

pub mod task {
    pub async fn blocking<F, R>(f: F) -> Result<R>
        where F: FnOnce() -> R + Send + 'static, R: Send + 'static;
    pub async fn blocking_timeout<F, R>(timeout: Duration, f: F) -> Result<R>;
    pub struct BlockingPool;   // new(), sized_for_machine(), global(), max_concurrency(), close()
}
pub mod shutdown { pub struct Signal; pub struct Drain; }
```

---

## `moso-schema`

```rust
pub trait Schema: Serialize + DeserializeOwned + Validate + Send + Sync + 'static {
    fn schema_name() -> Cow<'static, str>;
    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode;   // `generator`, not `gen`
    fn schema_ref() -> SchemaRef { SchemaRef::inline_or_named(Self::schema_name()) }
    const HAS_CONSTRAINTS: bool = false;
}

pub trait Validate { fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors>; }

pub struct ValidationErrors(SmallVec<[FieldError; 4]>);
pub struct FieldError { pub pointer: String, pub code: Cow<'static, str>,
                        pub message: Cow<'static, str>,
                        pub params: BTreeMap<&'static str, Value> }
pub struct ValidationCtx;
impl ValidationCtx { pub fn new() -> Self; pub fn rooted_at(prefix: &str) -> Self; .. }

pub trait MessageProvider: Send + Sync + 'static {      // dyn-compatible
    fn message(&self, code: &str, params: &BTreeMap<&'static str, Value>, locale: &Locale)
        -> Option<String>;
}

pub fn inline_schema_ref<T: Schema>() -> SchemaRef;
pub fn generic_schema_name(base: &str, arguments: &[Cow<'static, str>]) -> Cow<'static, str>;

// the closed error-code set
pub mod codes { pub const REQUIRED/TYPE/LEN/RANGE/PATTERN/FORMAT/ENUM/UNIQUE/MULTIPLE_OF/CUSTOM;
                pub const CUSTOM_PREFIX: &str = "custom:"; pub const ALL: &[&str]; }
pub enum ErrorCode { Required, Type, Len, Range, Pattern, Format, Enum, Unique, MultipleOf,
                     Custom(&'static str) }
```

### The JSON Schema model (D2 — it lives here, not in `moso-openapi`)

```rust
pub mod json_schema {
    pub struct SchemaNode { /* $ref, type, format, title, description, default, examples,
        deprecated, readOnly, writeOnly, enum, const, minLength, maxLength, pattern,
        contentEncoding, contentMediaType, minimum, maximum, exclusiveMinimum, exclusiveMaximum,
        multipleOf, items, prefixItems, minItems, maxItems, uniqueItems, properties, required,
        additionalProperties, minProperties, maxProperties, oneOf, anyOf, allOf, not,
        discriminator, $defs, and flattened x-* extensions */ }
    pub enum JsonType { Null, Boolean, Object, Array, Number, Integer, String }
    pub struct TypeSet(SmallVec<[JsonType; 2]>);
    pub enum AdditionalProperties { Any(bool), Schema(Box<SchemaNode>) }
    pub struct Discriminator { pub property_name: String, pub mapping: IndexMap<String, String> }

    pub enum SchemaRef { Inline(Box<SchemaNode>), Ref(String) }

    pub struct SchemaGenerator;
    impl SchemaGenerator {
        pub fn new(ref_prefix: &'static str) -> Self;      // Default = DEFAULT_REF_PREFIX
        pub fn subschema_for<T: Schema>(&mut self) -> SchemaNode;   // what every impl calls
        pub fn define<T: Schema>(&mut self) -> SchemaRef;           // recursion-safe
        pub fn insert(&mut self, name: impl Into<String>, node: SchemaNode) -> SchemaRef;
        pub fn definitions(&self) -> &IndexMap<String, SchemaNode>;
        pub fn sort_definitions(&mut self);
        pub fn collisions(&self) -> &[SchemaCollision];             // → boot error
        // builders take &self, so they compose in argument position while &mut self is live
        pub fn object(&self, name: impl Into<Cow<'static, str>>) -> ObjectBuilder;
        pub fn string(&self) -> StringBuilder;
        pub fn number(&self) -> NumberBuilder;   pub fn integer(&self) -> NumberBuilder;
        pub fn array(&self) -> ArrayBuilder;
        pub fn boolean(&self) -> SchemaNode; pub fn null(&self) -> SchemaNode;
        pub fn any(&self) -> SchemaNode;
    }
    pub const DEFAULT_REF_PREFIX: &str = "#/components/schemas/";
}
```

Builders share `title / description / description_opt / format / default_value / example /
enumeration / constant / deprecated / read_only / write_only / extension / nullable / build`.
`description_opt(Option<Cow>)` exists because `.description(None)` cannot infer its type parameter.

### Constrained types

```rust
pub struct Email;    pub struct Url;      pub struct Slug;      pub struct Password;
pub struct PhoneE164; pub struct Hostname; pub struct IpCidr;   pub struct Trimmed;
pub struct Cursor;   pub struct Id<E>(Uuid);
pub struct NonEmpty<T>;  pub struct Bounded<T, const MIN: i64, const MAX: i64>;
pub struct Length<T, const MIN: usize, const MAX: usize>;  pub struct Sanitised<P>;
pub enum ConstraintError { .. }
// sanitiser policies: EscapeHtml, StripTags
```

---

## `moso-openapi`

```rust
pub struct Document { pub openapi: String, pub info: Info, pub json_schema_dialect: Option<String>,
    pub servers: Vec<Server>, pub paths: IndexMap<String, PathItem>,
    pub webhooks: IndexMap<String, PathItem>, pub components: Components,
    pub security: Vec<SecurityRequirement>, pub tags: Vec<Tag>,
    pub external_docs: Option<ExternalDocs>, pub extensions: IndexMap<String, Value> }
impl Document { pub fn to_json_bytes(&self) -> Result<Vec<u8>, _>; .. }

pub struct DocumentBuilder;   // title/version/description/contact/license/server/tag_description/
                              // security_scheme/extension/operation/build
pub struct OperationBuilder;  // owns a SchemaGenerator by value for one operation
impl OperationBuilder {
    pub fn summary(&mut self, s: impl Into<String>) -> &mut Self;     // first writer wins
    pub fn description(&mut self, s: impl Into<String>) -> &mut Self;
    pub fn operation_id(&mut self, s: impl Into<String>) -> &mut Self;
    pub fn tag(&mut self, t: impl Into<String>) -> &mut Self;         // append-dedup
    pub fn deprecated(&mut self) -> &mut Self;                        // no bool arg; sticky
    pub fn sunset(&mut self, date: impl Into<String>) -> &mut Self;
    pub fn hidden(&mut self) -> &mut Self;
    pub fn source(&mut self, file: &'static str, line: u32) -> &mut Self;
    pub fn parameter(&mut self, p: Param) -> &mut Self;
    pub fn request_body(&mut self, ct: ContentType, s: SchemaRef, required: bool) -> &mut Self;
    pub fn request_body_of<T: Schema>(&mut self, ct: ContentType, required: bool) -> &mut Self;
    pub fn response(&mut self, status: u16, spec: ResponseSpec) -> &mut Self;
    pub fn response_key(&mut self, key: impl Into<String>, spec: ResponseSpec) -> &mut Self;
    pub fn default_response(&mut self, spec: ResponseSpec) -> &mut Self;
    pub fn security(&mut self, r: SecurityRequirement) -> &mut Self;
    pub fn public(&mut self) -> &mut Self;                            // security: []
    pub fn extension(&mut self, key: &'static str, value: Value) -> &mut Self;
    pub fn external_docs(&mut self, url: impl Into<String>, d: impl Into<String>) -> &mut Self;
    pub fn mark_validated(&mut self) -> &mut Self;
    pub fn generator(&mut self) -> &mut SchemaGenerator;
    pub fn spec(&self) -> &OperationSpec;   pub fn spec_mut(&mut self) -> &mut OperationSpec;
    pub fn finish(self) -> (OperationSpec, SchemaGenerator);
}

pub struct Param;
impl Param {
    pub fn path(name: impl Into<String>) -> Self;    // required forced true
    pub fn query(name: impl Into<String>) -> Self;
    pub fn header(name: impl Into<String>) -> Self;
    pub fn cookie(name: impl Into<String>) -> Self;
    pub fn required(self, required: bool) -> Self;
    pub fn description(self, d: impl Into<String>) -> Self;
    pub fn schema_of<T: Schema>(self) -> Self;       // DEFERRED — use this in argument position
    pub fn schema<T: Schema>(self, g: &mut SchemaGenerator) -> Self;   // eager
    pub fn schema_node(self, node: SchemaNode) -> Self;
    pub fn explode(self, b: bool) -> Self;  pub fn style(self, s: ParameterStyle) -> Self;
    pub fn deep_object(self) -> Self;  pub fn example(self, v: impl Into<Value>) -> Self;
}

pub struct ResponseSpec;
impl ResponseSpec {
    pub fn empty(description: impl Into<String>) -> Self;
    pub fn problem(description: impl Into<String>) -> Self;
    pub fn json_of<T: Schema>() -> Self;                     // deferred; no description argument
    pub fn validation_problem_of<T: Schema>() -> Self;       // deferred
    pub fn validation_problem<T: Schema>(g: &mut SchemaGenerator) -> Self;   // eager
    pub fn description(self, description: impl Into<String>) -> Self;
    pub fn header(self, name: impl Into<String>, schema: SchemaNode) -> Self;
    pub fn header_spec(self, name: impl Into<String>, header: Header) -> Self;
    pub fn example(self, value: impl Into<Value>) -> Self;
}

pub enum HttpMethod { Get, Put, Post, Delete, Options, Head, Patch, Trace }
impl HttpMethod { pub const ALL: [HttpMethod; 8]; pub const fn as_upper_str(self) -> &'static str; }

pub struct SecurityRequirement;
pub enum SecurityScheme { ApiKey { .. }, Http { .. }, OAuth2 { .. }, OpenIdConnect { .. }, .. }
pub struct RouteMetadata;        pub struct SourceLocation;
pub struct OperationSpec;
pub enum ContentType { Json, ProblemJson, Form, Multipart, OctetStream, Text, Html,
                       EventStream, Yaml, Xml, .. }
pub const PROBLEM_SCHEMA_NAME: &str;  pub const VALIDATION_PROBLEM_SCHEMA_NAME: &str;
pub const OPENAPI_VERSION: &str = "3.1.1";
pub fn etag_for(bytes: &[u8]) -> String;

pub mod ui { pub struct DocsUi; pub enum Theme { System, Light, Dark };
             pub fn render(spec_url: &str, title: &str) -> String; pub const TEMPLATE: &str;
             pub const DEFAULT_SPEC_URL: &str; pub const DEFAULT_TITLE: &str; }
pub mod diff {
    pub enum ChangeKind { Added, Removed, Modified }        // symbol(): '+', '-', '~'
    pub struct Change { pub kind: ChangeKind, pub path: String, pub detail: String,
                        pub breaking: bool }
    pub fn diff(old: &Document, new: &Document) -> Vec<Change>;
    pub fn diff_with(old: &Document, new: &Document, options: &DiffOptions) -> Vec<Change>;
    pub struct ChangeReport<'a>;
}
```

---

## `moso-test`

```rust
pub struct TestApp;      pub struct TestAppBuilder;
pub struct TestClient;   pub struct RequestBuilder;   pub struct TestResponse;
pub struct TestClock;    pub struct LogAssertions;    pub struct LogRecord;  pub enum Level { .. }
pub struct RequestRecord;
pub struct Multipart;    pub struct Part;             pub enum SendFailure { .. }
pub struct ContractOptions;                            // assert against the OpenAPI document
pub enum DiffKind { .. }  pub struct Difference;
#[macro_export] macro_rules! test_app;
pub mod prelude { /* the 11 names a test file types */ }
```

`MailAssertions`, `JobAssertions` and `trait Factory: Entity` are **not present** — the batteries
they assert against do not exist in this build.

---

## ⛔ Not in this build

`moso-orm`, `moso-kv`, `moso-auth`, `moso-authz`, `moso-jobs`, `moso-mail`, `moso-storage`,
`moso-admin`, `moso-sql`, `moso-migrate`. Their designed signatures are in `docs/02-data/` and
`docs/03-batteries/`; none of them compiles today and nothing in the workspace references them.

---

## Macro inventory — as shipped

| Macro | Crate | Purpose | Status |
| --- | --- | --- | --- |
| `#[endpoint]` | `moso-macros` | operation spec + companion type + assertion codegen | ✅ |
| `routes!` | `moso-macros` | tabular route registration | ✅ |
| `ep!` | `moso-macros` | name one endpoint's companion type | ✅ (new; ADR-0013) |
| `#[middleware]` | `moso-macros` | fn-shaped Tower layer | ✅ |
| `#[derive(Schema)]` | `moso-macros` | serde + validate + JSON Schema + `IntoResponse`/`Describe` | ✅ |
| `#[derive(Constrained)]` | `moso-macros` | custom constrained newtype | ✅ |
| `#[derive(Responder)]` | `moso-macros` | `IntoResponse` + `Describe` | ✅ |
| `#[derive(Dependency)]` | `moso-macros` | request-scoped dependency | ✅ |
| `#[derive(Config)]` | `moso-macros` | layered typed config | ✅ |
| `#[derive(Error)]` | `moso-macros` | status/type mapping + `Into<Error>` | ✅ |
| `concat_reqs!` | `moso-core` | const `&[ProviderReq]` concatenation | ✅ (internal) |
| `route_path!` | `moso-core` | const path validation at the literal's span | ✅ (internal) |
| `test_app!` | `moso-test` | boot a `TestApp` from an `AppBuilder` | ✅ |
| `sql!` | — | parameterised raw SQL | ⛔ |
| `#[derive(Entity)]` / `Projection` / `Factory` / `#[migration]` | — | ORM | ⛔ |
| `namespace!` / `#[cached]` | — | KV | ⛔ |
| `permissions!` / `roles!` / `#[requires]` / `#[public]` | — | authz | ⛔ |
| `#[job]` | — | background jobs | ⛔ |
| `#[derive(Email)]` | — | templated email | ⛔ |
| `#[moso::test]` / `assert_queries!` | — | test attribute + statement counting | ⛔ |
| `flags!` | — | typed feature flags | ⛔ |

Full expansions: [`62-macro-reference.md`](62-macro-reference.md).
