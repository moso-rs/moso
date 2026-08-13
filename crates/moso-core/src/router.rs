//! The route table: registration, composition, and what `App::build()` does
//! with it.
//!
//! # `Router` is not generic over state
//!
//! There is no `Router<S>` and no `FromRef`. This is the single largest
//! diagnostic decision in the framework: state-generic routers are where most
//! Axum trait errors come from, and where most of an application's
//! monomorphisation goes. State lives in the provider map and is read with
//! [`Inject<T>`](crate::Inject), which boot proved exists.
//!
//! # A router accumulates, it does not compose eagerly
//!
//! `Router` holds a `Vec<RouteEntry>` plus the pending metadata, and builds
//! nothing. The `axum::Router` is constructed once, at `App::build()`, *after*
//! conflict detection has run — so a route conflict is a boot error naming both
//! source locations rather than a panic from inside `matchit`.
//!
//! ```text
//! Router::get(..)      → push a RouteEntry
//! Router::tag("users") → apply to every entry registered SO FAR
//! Router::layer(l)     → attach `l` to every entry registered SO FAR
//! Router::nest("/v1")  → prefix the inner entries' paths, push the outer metadata down
//! App::build()         → detect conflicts, then build one axum::Router
//! ```
//!
//! ## "So far" is deliberate, and it is signposted
//!
//! `.tag()`, `.layer()` and `.guard()` apply to routes registered *before* the
//! call. That is Tower's ordering, inherited rather than invented, and it is a
//! known source of confusion. Moso mitigates it three ways: it is stated in
//! every affected doc comment, `moso middleware` prints the effective per-route
//! stack, and `moso check` warns when a `.layer()` call is the last statement
//! in a router function — which is almost always someone expecting it to apply
//! to everything.
//!
//! The one exception is composition: [`Router::nest`] pushes the outer router's
//! accumulated metadata *down* onto the routes it absorbs, because a nested
//! router is a section of the API rather than a sibling. [`Router::merge`] does
//! not, because a merged router is a sibling that has already described itself.
//!
//! # Path syntax
//!
//! `/{param}` and `/{*rest}`, matching Axum 0.8 and OpenAPI. `:param` and
//! `*rest` are rejected by [`validate_path`], a `const fn`: every registration
//! method takes a `&'static str`, so wrapping the literal in
//! [`route_path!`](crate::route_path) — which `routes!` and `#[endpoint]` do —
//! turns a routing mistake into a *compile* error. Registration also calls it
//! directly, so a path that reaches the router by another road still fails at
//! boot rather than 404ing in staging.
//!
//! # Two descriptions of one operation
//!
//! Each entry carries a [`RouteEntry::spec`] built at registration time. It is
//! the *preview*: what `moso routes` prints, what conflict detection reads the
//! source location from, and what the boot-time path-parameter check compares
//! against. It is built with a throwaway [`SchemaGenerator`], so the named
//! schemas it registered are gone and its `$ref`s resolve only against the
//! document that `App::build()` assembles.
//!
//! The *authoritative* description is [`RouteEntry::describe`], which re-runs
//! the handler's `Endpoint::spec`, the guards' `describe`, and the router
//! metadata against the document's own generator. `App::build()` must use that
//! one; nothing else produces a document whose `$ref`s resolve.

use core::convert::Infallible;
use core::task::{Context, Poll};
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use indexmap::map::Entry;
use moso_openapi::{
    HttpMethod, OperationBuilder, OperationSpec, ResponseSpec, RouteMetadata, SchemaGenerator,
    SecurityRequirement, SourceLocation,
};
use tower::Service;
use tower::util::BoxCloneSyncService;

use crate::error::boot::ConflictReason;
use crate::error::{BootError, ErrorKind, Problem};
use crate::handler::{BoxedHandler, Endpoint, Handler, HandlerFn, UndocumentedEndpoint, boxed};
use crate::middleware::{CustomLayer, Guard};
use crate::response::IntoResponse;
use crate::{BoxFuture, Request, RequestCtx, Response};

// ---------------------------------------------------------------------------
// Path validation
// ---------------------------------------------------------------------------

/// Check a route path, in a `const` context if the caller provides one.
///
/// Returns `path` unchanged when it is a valid Moso route template, and panics
/// otherwise. The rules, and the reason for each:
///
/// | Rejected | Why |
/// | --- | --- |
/// | `""`, `"users"` | `matchit` requires a leading `/` |
/// | `/users/:id` | pre-0.8 Axum / Actix syntax; write `{id}` |
/// | `/files/*rest` | pre-0.8 wildcard syntax; write `{*rest}` |
/// | `/users/{id` , `/users/id}` | unbalanced braces |
/// | `/users/{}` | a parameter must have a name |
/// | `/users/{a}{b}` | one parameter per segment |
/// | `/users/{id}x`, `/files/v{version}` | static text may not sit beside a parameter |
/// | `/{*rest}/more` | a catch-all must be the last segment |
/// | `/{id}/posts/{id}` | duplicate parameter name |
///
/// The first three are what a reader coming from Axum 0.7, Actix or Rocket will
/// type; the rest are what `matchit` would otherwise reject at boot with a
/// message about a data structure the reader has never heard of.
///
/// ```
/// # use moso_core::router::validate_path;
/// const PATH: &str = validate_path("/users/{id}/posts/{slug}");
/// assert_eq!(PATH, "/users/{id}/posts/{slug}");
/// ```
///
/// Wrap the literal in [`route_path!`](crate::route_path) to force the check
/// into compile time:
///
/// ```compile_fail
/// # use moso_core::route_path;
/// let path = route_path!("/users/:id"); // error: legacy path parameter syntax
/// ```
pub const fn validate_path(path: &'static str) -> &'static str {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        panic!("a route path must not be empty: write \"/\" for the root");
    }
    if bytes[0] != b'/' {
        panic!("a route path must start with `/`");
    }
    reject_legacy_syntax(bytes);
    check_parameter_syntax(bytes);
    check_unique_parameters(bytes);
    path
}

/// Panic on `:param` / `*rest`, the syntax every other Rust router used to use.
const fn reject_legacy_syntax(bytes: &[u8]) {
    let mut index = 0;
    while index < bytes.len() {
        let starts_segment = index == 0 || bytes[index - 1] == b'/';
        if starts_segment {
            if bytes[index] == b':' {
                panic!(
                    "legacy path parameter syntax: write `{{id}}` in braces, not `:id` — Moso \
                     uses OpenAPI-style path parameters throughout"
                );
            }
            if bytes[index] == b'*' {
                panic!(
                    "legacy wildcard syntax: write `{{*rest}}` in braces, not `*rest` — Moso \
                     uses OpenAPI-style path parameters throughout"
                );
            }
        }
        index += 1;
    }
}

/// Panic on unbalanced braces, unnamed or illegal parameters, more than one
/// parameter per segment, static text beside a parameter, or a catch-all that
/// is not the final segment.
///
/// "Beside" covers both sides. `/files/{name}x` and `/files/v{version}` are
/// equally refused, because a segment that holds a parameter holds nothing
/// else. `matchit` would accept the second and capture `v1` rather than `1`,
/// which is not what the `{version}` in the OpenAPI path template means — so
/// the document and the router would quietly disagree about what the parameter
/// is. This is also the rule `routes!` enforces on the literal, and the two
/// checkers exist to answer the same question in two places, never to answer it
/// differently.
const fn check_parameter_syntax(bytes: &[u8]) {
    let mut index = 0;
    let mut parameters_in_segment = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'/' {
            parameters_in_segment = 0;
            index += 1;
            continue;
        }
        if byte == b'}' {
            panic!("unmatched `}}` in a route path");
        }
        if byte != b'{' {
            if parameters_in_segment > 0 {
                panic!(
                    "static text may not sit beside a parameter inside one path segment: give the \
                     parameter a segment of its own"
                );
            }
            index += 1;
            continue;
        }

        if parameters_in_segment > 0 {
            panic!("only one parameter is allowed per path segment");
        }
        // The mirror image of the check above: a parameter that does not open
        // its own segment has static text in front of it. One test covers both
        // sides, and a catch-all needs no separate rule.
        if index > 0 && bytes[index - 1] != b'/' {
            panic!(
                "static text may not sit beside a parameter inside one path segment: give the \
                 parameter a segment of its own"
            );
        }
        let close = closing_brace(bytes, index);
        let mut name_start = index + 1;
        let catch_all = bytes[name_start] == b'*';
        if catch_all {
            name_start += 1;
            if close + 1 != bytes.len() {
                panic!("a catch-all parameter must be the last segment of a route path");
            }
        }
        if name_start >= close {
            panic!("a path parameter must have a name: write `{{id}}`, not `{{}}`");
        }
        check_parameter_name(bytes, name_start, close);
        parameters_in_segment += 1;
        index = close + 1;
    }
}

/// The index of the `}` closing the `{` at `open`. Panics if there is none.
const fn closing_brace(bytes: &[u8], open: usize) -> usize {
    let mut index = open + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'}' => return index,
            b'{' => panic!("nested `{{` in a route path"),
            b'/' => panic!("unterminated `{{` in a route path"),
            _ => index += 1,
        }
    }
    panic!("unterminated `{{` in a route path")
}

/// Parameter names are `[A-Za-z0-9_]+`, because they must also be usable as the
/// field name of the `Path<T>` struct that reads them.
const fn check_parameter_name(bytes: &[u8], start: usize, end: usize) {
    let mut index = start;
    while index < end {
        let byte = bytes[index];
        let ok = byte.is_ascii_alphanumeric() || byte == b'_';
        if !ok {
            panic!(
                "a path parameter name may contain only letters, digits and `_`, because it must \
                 also be a Rust field name on the `Path<T>` struct that reads it"
            );
        }
        index += 1;
    }
}

/// Panic when one path declares the same parameter name twice.
const fn check_unique_parameters(bytes: &[u8]) {
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'{' {
            let (start, end, next) = parameter_name_range(bytes, index);
            let mut other = next;
            while other < bytes.len() {
                if bytes[other] == b'{' {
                    let (other_start, other_end, after) = parameter_name_range(bytes, other);
                    if range_eq(bytes, start, end, other_start, other_end) {
                        panic!(
                            "a route path may not declare the same parameter name twice: \
                             `matchit` would capture only one of them"
                        );
                    }
                    other = after;
                } else {
                    other += 1;
                }
            }
            index = next;
        } else {
            index += 1;
        }
    }
}

/// `(name start, name end, index just past the closing brace)`.
const fn parameter_name_range(bytes: &[u8], open: usize) -> (usize, usize, usize) {
    let close = closing_brace(bytes, open);
    let mut start = open + 1;
    if bytes[start] == b'*' {
        start += 1;
    }
    (start, close, close + 1)
}

/// Byte equality of two ranges of the same slice.
const fn range_eq(
    bytes: &[u8],
    a_start: usize,
    a_end: usize,
    b_start: usize,
    b_end: usize,
) -> bool {
    if a_end - a_start != b_end - b_start {
        return false;
    }
    let mut offset = 0;
    while a_start + offset < a_end {
        if bytes[a_start + offset] != bytes[b_start + offset] {
            return false;
        }
        offset += 1;
    }
    true
}

/// Check a route path *at compile time*.
///
/// [`validate_path`] is a `const fn`, but a `&'static str` that arrives as a
/// function argument is not a constant, so `Router::get(path, ..)` can only run
/// the check at boot. Binding the literal to a `const` moves it to compile
/// time, which is what `routes!` and `ep!` emit:
///
/// ```
/// # use moso_core::route_path;
/// let path = route_path!("/users/{id}");
/// assert_eq!(path, "/users/{id}");
/// ```
///
/// ```compile_fail
/// # use moso_core::route_path;
/// let path = route_path!("/files/*rest"); // error: legacy wildcard syntax
/// ```
///
/// # Why a named `const` and not `const { … }`
///
/// An inline `const { … }` block is only const-evaluated during codegen, so
/// `cargo check` — the inner loop, and what `moso check` and rust-analyzer run —
/// accepts a bad path in silence and the panic surfaces on the next real build.
/// A named `const` item is evaluated during type checking instead, so the error
/// appears in the fast loop where it is useful. The two are otherwise identical.
///
/// The proc macros validate the literal themselves and never reach this panic;
/// it is the backstop for a hand-written `Router::get(route_path!("…"), …)`.
#[macro_export]
macro_rules! route_path {
    ($path:literal) => {{
        const __MOSO_ROUTE_PATH: &'static str = $crate::router::validate_path($path);
        __MOSO_ROUTE_PATH
    }};
}

/// The parameter names a path template declares, in order.
///
/// `/users/{id}/posts/{slug}` yields `["id", "slug"]`; `/files/{*rest}` yields
/// `["rest"]`. Used by the boot-time check that a `Path<T>`'s fields match the
/// parameters the route actually captures.
pub fn path_parameters(path: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'{' {
            let Some(close) = bytes[index..].iter().position(|byte| *byte == b'}') else {
                break;
            };
            let close = index + close;
            let mut start = index + 1;
            if start < close && bytes[start] == b'*' {
                start += 1;
            }
            if start < close {
                names.push(&path[start..close]);
            }
            index = close + 1;
        } else {
            index += 1;
        }
    }
    names
}

/// The structural shape of a path, with every parameter name erased.
///
/// `/users/{id}` and `/users/{user_id}` both become `/users/{}`, which is
/// exactly the equivalence `matchit` uses: it cannot tell them apart, so
/// registering both is a conflict rather than two routes.
fn path_shape(path: &str) -> String {
    let mut shape = String::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'{' {
            let Some(close) = bytes[index..].iter().position(|byte| *byte == b'}') else {
                shape.push_str(&path[index..]);
                break;
            };
            let close = index + close;
            if bytes[index + 1] == b'*' {
                shape.push_str("{*}");
            } else {
                shape.push_str("{}");
            }
            index = close + 1;
        } else {
            shape.push(bytes[index] as char);
            index += 1;
        }
    }
    shape
}

/// Join a `nest` prefix and an inner path.
///
/// Both are already validated, so this only has to get the slashes right:
/// `("/api/v1", "/users") → "/api/v1/users"` and `("/api", "/") → "/api"`.
fn join_paths(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    if path.is_empty() || path == "/" {
        if prefix.is_empty() {
            "/".to_owned()
        } else {
            prefix.to_owned()
        }
    } else if prefix.is_empty() {
        path.to_owned()
    } else {
        format!("{prefix}{path}")
    }
}

/// Whether a path is the root, for which Axum's `nest` is not allowed.
fn is_root(path: &str) -> bool {
    path.is_empty() || path == "/"
}

// ---------------------------------------------------------------------------
// Route services
// ---------------------------------------------------------------------------

/// The erased service a route becomes, and what a [`tower::Layer`] wraps.
///
/// `axum::routing::Route` would be the obvious choice, but its constructor is
/// crate-private, so a Moso layer could never build one. A
/// `BoxCloneSyncService` is the same shape — clonable, `Sync`, infallible — and
/// satisfies `axum::Router::route_service`'s bounds exactly.
pub type Route = BoxCloneSyncService<Request, Response, core::convert::Infallible>;

/// [`Route`] under the name the middleware documentation uses.
pub type RouteService = Route;

/// One registered route, before it becomes an Axum route.
///
/// This is what makes boot-time validation possible: the method, the path, the
/// operation's description and the provider requirements are all available as
/// plain data, together, before anything is compiled into a service.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::NoContent;
/// # /// A database handle.
/// # #[derive(Default)] pub struct Db;
/// /// List users.
/// #[endpoint]
/// async fn list(Inject(db): Inject<Db>) -> Result<NoContent> {
///     let _ = db;
///     Ok(NoContent)
/// }
///
/// # fn main() {
/// let router = Router::new().get("/users", moso::ep!(list)).tag("users");
/// let entry = &router.entries()[0];
///
/// // The whole route is plain data before anything becomes a service — which is
/// // what lets `App::build()` check it, and `moso routes` print it.
/// assert_eq!(entry.path, "/users");
/// assert_eq!(entry.spec.summary.as_deref(), Some("List users."));
/// assert_eq!(entry.spec.tags, ["users"]);
/// assert_eq!(entry.providers.len(), 1);
/// # }
/// ```
pub struct RouteEntry {
    /// The HTTP method.
    pub method: HttpMethod,
    /// The path, with any `nest` prefixes already applied.
    pub path: String,
    /// The operation as described at registration time: the handler's
    /// `Endpoint::spec`, then each guard's `describe`, then the router
    /// metadata.
    ///
    /// This is the **preview**, and the distinction matters. It was built with
    /// a throwaway [`SchemaGenerator`], so any named schema it registered has
    /// been dropped and its `$ref`s resolve only against the document
    /// `App::build()` assembles. Read it for the summary, the tags, the
    /// parameters and the source location — `moso routes`, conflict reports and
    /// the path-parameter check all do — but build the document itself with
    /// [`RouteEntry::describe`], which re-describes against the real generator.
    pub spec: OperationSpec,
    /// The providers this route needs, from `Endpoint::required_providers`.
    pub providers: &'static [crate::di::ProviderReq],
    /// The handler, type-erased.
    pub handler: BoxedHandler,
    /// Layers to apply, innermost first.
    pub layers: Vec<Arc<dyn CustomLayer>>,
    /// Guards to run after routing and before extraction, in order.
    pub guards: Vec<Arc<dyn DynGuard>>,
    /// The router-level metadata that applies to this route.
    ///
    /// Accumulated by `Router::tag`, `security`, `responds`, `deprecated` and
    /// `hidden`, and pushed down by [`Router::nest`]. Kept alongside
    /// [`RouteEntry::spec`] — rather than only folded into it — because
    /// `ResponseSpec` carries deferred schemas that must be resolved with the
    /// document's generator, not with the throwaway one the preview used.
    pub metadata: RouteMetadata,
}

impl RouteEntry {
    /// Describe this route into `op`, authoritatively.
    ///
    /// The handler first, then each guard, then the router metadata — the order
    /// that makes "first writer wins" mean "the handler's own words win". Call
    /// it with an [`OperationBuilder`] holding the document's real
    /// [`SchemaGenerator`] and every schema referenced by the result will be in
    /// `components/schemas`:
    ///
    /// ```
    /// use moso::prelude::*;
    /// use moso::openapi::{DocumentBuilder, OperationBuilder};
    /// use moso::schema::SchemaGenerator;
    /// # /// Liveness.
    /// # #[endpoint] async fn healthz() -> Result<moso::response::NoContent> {
    /// #     Ok(moso::response::NoContent) }
    /// # fn main() {
    /// let router = Router::new().get("/healthz", moso::ep!(healthz)).tag("ops");
    /// let entry = &router.entries()[0];
    ///
    /// let mut op = OperationBuilder::new(SchemaGenerator::default());
    /// entry.describe(&mut op);
    ///
    /// let (spec, _) = op.finish();
    /// assert_eq!(spec.summary.as_deref(), Some("Liveness."));
    /// assert_eq!(spec.tags, ["ops"]);
    /// # }
    /// ```
    ///
    /// A document builder drives this for every route:
    /// `document.operation(entry.method, &entry.path, |op| entry.describe(op))`.
    pub fn describe(&self, op: &mut OperationBuilder) {
        self.handler.describe(op);
        name_positional_path_parameters(&self.path, op);
        for guard in &self.guards {
            guard.describe_dyn(op);
        }
        self.metadata.apply(op);
    }

    /// Compile this route into the service that answers it.
    ///
    /// The handler, wrapped by its guards, wrapped by its layers innermost
    /// first. Called once per route at boot.
    pub fn into_service(self) -> Route {
        let mut service = Route::new(RouteHandler {
            handler: self.handler,
            guards: Arc::from(self.guards),
        });
        for layer in &self.layers {
            service = layer.apply(service);
        }
        service
    }

    /// The parameter names this route's path declares, in order.
    pub fn path_parameters(&self) -> Vec<&str> {
        path_parameters(&self.path)
    }
}

/// Give `Path<T>`'s positional placeholders the names the template declares.
///
/// A struct `Path<T>` knows its own field names, so
/// [`Path::describe`](crate::extract::Path) emits real names for it. A *scalar*
/// or *tuple* `Path<T>` — `Path<u64>`, `Path<(u64, String)>` — has no names to
/// emit: the parameter it stands for is identified by position, and only the
/// route knows what that position is called. `describe` emits `Param::path("")`
/// for each one, and this fills them in, in order, from the path template.
///
/// Without this step every scalar `Path<T>` on a parameterised route fails
/// `App::build()` with "path parameter mismatch: declared `id`, expected ``" —
/// the document would carry a nameless parameter that matches no `{placeholder}`.
///
/// Only empty names are touched, so a struct `Path<T>` whose field names
/// genuinely disagree with the template still reports the mismatch it should.
fn name_positional_path_parameters(path: &str, op: &mut OperationBuilder) {
    use moso_openapi::path::ParameterLocation;

    let anonymous = op
        .spec()
        .parameters
        .iter()
        .filter(|parameter| {
            parameter.location == ParameterLocation::Path && parameter.name.is_empty()
        })
        .count();
    if anonymous == 0 {
        return;
    }

    // Names not already claimed by a parameter that knew its own, so a handler
    // taking `Path<u64>` *and* an explicitly named parameter still lines up.
    let claimed: Vec<String> = op
        .spec()
        .parameters
        .iter()
        .filter(|parameter| parameter.location == ParameterLocation::Path)
        .map(|parameter| parameter.name.clone())
        .collect();
    let mut available = path_parameters(path)
        .into_iter()
        .filter(|name| !claimed.iter().any(|taken| taken == name));

    for parameter in op.spec_mut().parameters.iter_mut() {
        if parameter.location != ParameterLocation::Path || !parameter.name.is_empty() {
            continue;
        }
        let Some(name) = available.next() else {
            // More placeholders than the template declares. Leaving the name
            // empty makes `DocumentBuilder::build` report the arity mismatch,
            // which is the accurate complaint; inventing a name would hide it.
            break;
        };
        parameter.name = name.to_owned();
    }
}

impl core::fmt::Debug for RouteEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RouteEntry")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("layers", &self.layers.len())
            .field("guards", &self.guards.len())
            .finish_non_exhaustive()
    }
}

/// A [`Guard`] with its type erased, as the route table stores it.
///
/// Separate from [`Guard`] so that `Guard` can stay a pleasant trait to
/// implement while the table holds `dyn` values. A blanket
/// `impl<G: Guard> DynGuard for G` supplies it, so implementing [`Guard`] is
/// the only thing anyone does.
///
/// ```
/// use moso::prelude::*;
/// use moso::middleware::guard::RequireHeader;
/// use moso::router::DynGuard;
/// use std::sync::Arc;
///
/// # fn main() {
/// // Any `Guard` is already a `DynGuard`, which is what `Router::guard` stores.
/// let erased: Arc<dyn DynGuard> = Arc::new(RequireHeader::new("x-internal"));
/// # let _ = erased;
/// # }
/// ```
///
/// # Why this carries its own message
///
/// Nobody implements `DynGuard`, but a user can still *fail* it: passing a type
/// that is not a guard to [`Router::guard`] reports the bound the route table
/// needs, which is this one. The message is deliberately the same as [`Guard`]'s,
/// so which of the two halves the compiler happened to name does not change what
/// the user is told to do.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a guard",
    label = "not a guard",
    note = "`DynGuard` is the type-erased half of `Guard` that the route table stores; a blanket \
            impl supplies it for every `Guard`, so `Guard` is the trait to write",
    note = "help: write `impl Guard for {Self}` with a `describe(&self, op)` and a \
            `check(&self, parts, ctx)`",
    note = "for middleware that does not affect the API contract, use a `tower::Layer` with \
            `Router::layer` instead"
)]
pub trait DynGuard: Send + Sync + 'static {
    /// Run the check.
    fn check_dyn<'a>(
        &'a self,
        parts: &'a http::request::Parts,
        ctx: &'a crate::RequestCtx,
    ) -> crate::BoxFuture<'a, crate::Result<()>>;

    /// Contribute the responses and security requirements the guard implies.
    fn describe_dyn(&self, op: &mut moso_openapi::OperationBuilder);
}

/// The service one route becomes: guards, then the erased handler.
///
/// The [`RequestCtx`] comes from the request extensions. It is put there either
/// by an outer layer that already built one, or built here from the
/// `Arc<AppState>` such a layer inserted — the second form is the one
/// `App::build` uses, because the matched route pattern only exists *inside*
/// routing and the context snapshots it.
///
/// It is also where the request's cookie jar is drained. Extraction cannot
/// touch the response, so `Cookies` records into
/// [`RequestCtx::cookies`](crate::RequestCtx::cookies) and this is the one
/// place that turns the result into `Set-Cookie` headers — on *every* exit,
/// including a guard's rejection, because a guard that clears a stale session
/// before answering 401 must not have its write thrown away. A request that
/// never mentioned a cookie pays one atomic load for the check.
#[derive(Clone)]
struct RouteHandler {
    handler: BoxedHandler,
    guards: Arc<[Arc<dyn DynGuard>]>,
}

impl Service<Request> for RouteHandler {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let handler = Arc::clone(&self.handler);
        let guards = Arc::clone(&self.guards);
        Box::pin(async move {
            let (mut parts, body) = req.into_parts();
            let Some(ctx) = request_context(&parts) else {
                return Ok(problem_response(
                    &ErrorKind::Internal,
                    "this router was mounted without an application: no request context is \
                     available",
                ));
            };
            // Moso extractors reached through Axum's traits read the context
            // back out of the extensions; see `extract::ctx_from_parts`.
            parts.extensions.insert(ctx.clone());
            for guard in guards.iter() {
                if let Err(error) = guard.check_dyn(&parts, &ctx).await {
                    let mut response = error.into_response();
                    apply_pending_cookies(&ctx, &mut response);
                    return Ok(response);
                }
            }
            let mut response = handler
                .call_erased(Request::from_parts(parts, body), ctx.clone())
                .await;
            apply_pending_cookies(&ctx, &mut response);
            Ok(response)
        })
    }
}

/// Give `response` the `Set-Cookie` headers this request accumulated.
///
/// The whole cost on a request that never touched a cookie is
/// [`RequestCtx::cookies_if_used`], which is one atomic load returning `None`:
/// the jar is a `OnceLock` that only an explicit ask initialises, so there is no
/// lock to take and nothing to free.
///
/// Appending rather than setting is deliberate — see
/// [`Cookies::apply_to`](crate::extract::Cookies::apply_to) for what happens to
/// a name the response already sets.
fn apply_pending_cookies(ctx: &RequestCtx, response: &mut Response) {
    if let Some(cookies) = ctx.cookies_if_used() {
        cookies.apply_to(response.headers_mut());
    }
}

/// The context for one request, if this router was mounted by an application.
///
/// A [`RequestCtx`] an outer layer already built is used as it is; otherwise one
/// is built here from the `Arc<AppState>` that layer inserted. The second form
/// is the one `App::build` relies on, because the matched route pattern the
/// context snapshots only exists once routing has happened — which is here.
pub(crate) fn request_context(parts: &http::request::Parts) -> Option<RequestCtx> {
    if let Some(ctx) = parts.extensions.get::<RequestCtx>() {
        return Some(ctx.clone());
    }
    let state = parts.extensions.get::<Arc<crate::AppState>>()?;
    Some(RequestCtx::new(Arc::clone(state), parts))
}

/// An RFC 9457 document, for the responses the router itself produces.
///
/// The 404, the 405, the per-route timeout, the static-file answers and the "no
/// application state" 500 have to be renderable with no configuration, no
/// provider map and no `catch_error` layer in play — [`Router::into_axum`] is
/// documented to work detached. That is why they are *rendered* here rather
/// than through [`Error`](crate::Error), which needs an application to ask
/// about disclosure before it can decide what to say.
///
/// What they are **not** allowed to do is invent their own taxonomy. `status`,
/// `type` and `title` all come from the [`ErrorKind`] the caller names, which
/// is the same place [`Error`](crate::Error) reads them from, so a fallback 404
/// and an [`Error::not_found`](crate::Error::not_found) 404 carry the same
/// `type` URI and a test can assert one rule for both. Only `detail` — which is
/// request-specific by definition — is written here.
fn problem_response(kind: &ErrorKind, detail: &str) -> Response {
    let mut problem = Problem::new(kind.status(), kind.type_uri(), kind.title());
    problem.detail = Some(detail.to_owned());
    problem.into_response()
}

/// The framework's default answer to a request that matched no route.
#[derive(Clone, Copy)]
struct NotFound;

impl Service<Request> for NotFound {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let path = req.uri().path().to_owned();
        Box::pin(async move {
            Ok(problem_response(
                &ErrorKind::NotFound,
                &format!("no route matches {path}"),
            ))
        })
    }
}

/// The framework's default answer to a request whose path matched but whose
/// method did not.
///
/// Axum's own `MethodRouter` fallback is a bare 405 with an empty body, which
/// would be the one framework-generated error that does not share the RFC 9457
/// shape. Installing this in its place fixes that without costing the `Allow`
/// header: Axum attaches `Allow` to whatever the method-router fallback
/// returned, and only when the response does not already carry one, so the
/// header still lists exactly the methods Axum will actually route.
///
/// The detail therefore does not repeat the method list. Naming it here would
/// mean re-deriving Axum's rule that a registered `GET` also answers `HEAD`,
/// and a detail that disagreed with the header beside it would be worse than no
/// detail at all.
#[derive(Clone, Copy)]
struct MethodNotAllowed;

impl Service<Request> for MethodNotAllowed {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let method = req.method().clone();
        Box::pin(async move {
            Ok(problem_response(
                &ErrorKind::MethodNotAllowed,
                &format!("this path does not accept {method}; see the `Allow` header"),
            ))
        })
    }
}

/// A per-route timeout, as [`Router::timeout`] installs it.
#[derive(Debug, Clone, Copy)]
struct RouteTimeout {
    timeout: Duration,
}

impl CustomLayer for RouteTimeout {
    fn name(&self) -> &'static str {
        "timeout"
    }

    fn apply(&self, service: Route) -> Route {
        Route::new(TimeoutService {
            inner: service,
            timeout: self.timeout,
        })
    }

    fn summary(&self) -> String {
        humantime::format_duration(self.timeout).to_string()
    }
}

#[derive(Clone)]
struct TimeoutService {
    inner: Route,
    timeout: Duration,
}

impl Service<Request> for TimeoutService {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        // The clone-and-swap dance: `self.inner` is the instance that was
        // polled ready, so it is the one that must be called.
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);
        let timeout = self.timeout;
        Box::pin(async move {
            match tokio::time::timeout(timeout, inner.call(req)).await {
                Ok(result) => result,
                // `ErrorKind::Timeout`, not `GatewayTimeout`: both are 504, but
                // the first is "our own timeout layer fired" and the second is
                // "an upstream did not answer". This is our own layer, and it
                // must say what `Error::timeout` says.
                Err(_) => Ok(problem_response(
                    &ErrorKind::Timeout,
                    "the request exceeded this route's time budget",
                )),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// A collection of routes and the metadata that applies to them.
///
/// A `Router` is not generic over application state — there is no `Router<S>`
/// and no `FromRef`. Everything a handler needs comes from the provider map
/// through `Inject<T>` or from the request through `Depends<T>`.
///
/// ```
/// use moso::prelude::*;
/// # /// A post, as the API returns one.
/// # #[derive(Schema)] pub struct PostOut { /// URL-safe identifier.
/// #     pub slug: Slug }
/// # /// List posts.
/// # #[endpoint] async fn list() -> Result<Json<Vec<PostOut>>> { Ok(Json(vec![])) }
/// # /// Create a post.
/// # #[endpoint] async fn create() -> Result<Created<PostOut>> {
/// #     Ok(Created::at("/posts/x", PostOut { slug: Slug::from_title("x").unwrap() })) }
/// # /// Show a post.
/// # #[endpoint] async fn show(Path(slug): Path<Slug>) -> Result<Json<PostOut>> {
/// #     Ok(Json(PostOut { slug })) }
/// /// Everything this module serves.
/// pub fn router() -> Router {
///     moso::routes! {
///         GET    "/posts"        => list,
///         POST   "/posts"        => create,
///         GET    "/posts/{slug}" => show,
///     }
///     .tag("posts")
/// }
/// # fn main() { assert_eq!(router().len(), 3); }
/// ```
#[derive(Default)]
pub struct Router {
    entries: Vec<RouteEntry>,
    metadata: RouteMetadata,
    fallback: Option<BoxedHandler>,
    method_not_allowed: Option<BoxedHandler>,
    axum_mounts: Vec<(String, axum::Router<()>)>,
    static_mounts: Vec<(String, StaticSource)>,
    problems: Vec<BootError>,
}

impl Router {
    /// An empty router.
    pub fn new() -> Self {
        Self::default()
    }

    // ── method shorthands ─────────────────────────────────────────────────

    /// Register a `GET` route.
    ///
    /// `path` is `&'static str` so the legacy-syntax check can run at compile
    /// time. Metadata applied later with [`Router::tag`] and friends reaches
    /// this route; metadata applied earlier does not.
    pub fn get<H, M>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.method(HttpMethod::Get, path, handler)
    }

    /// Register a `POST` route.
    pub fn post<H, M>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.method(HttpMethod::Post, path, handler)
    }

    /// Register a `PUT` route.
    pub fn put<H, M>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.method(HttpMethod::Put, path, handler)
    }

    /// Register a `PATCH` route.
    pub fn patch<H, M>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.method(HttpMethod::Patch, path, handler)
    }

    /// Register a `DELETE` route.
    pub fn delete<H, M>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.method(HttpMethod::Delete, path, handler)
    }

    /// Register a `HEAD` route.
    ///
    /// Rarely needed: a `GET` route answers `HEAD` automatically, with the
    /// headers and no body.
    pub fn head<H, M>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.method(HttpMethod::Head, path, handler)
    }

    /// Register an `OPTIONS` route.
    ///
    /// Rarely needed: CORS preflight is handled by the `cors` layer.
    pub fn options<H, M>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.method(HttpMethod::Options, path, handler)
    }

    /// Register a route for an arbitrary method.
    pub fn method<H, M>(mut self, method: HttpMethod, path: &'static str, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        let path = validate_path(path);
        self.entries.push(RouteEntry {
            method,
            path: path.to_owned(),
            spec: describe_endpoint::<H::Endpoint>(),
            providers: <H::Endpoint as Endpoint>::required_providers(),
            handler: boxed(handler),
            layers: Vec::new(),
            guards: Vec::new(),
            metadata: RouteMetadata::new(),
        });
        self
    }

    /// Register an endpoint by type, the explicit form of what `routes!` emits.
    ///
    /// ```
    /// use moso::prelude::*;
    /// use moso::openapi::HttpMethod;
    /// # /// A user, as the API returns one.
    /// # #[derive(Schema)] pub struct UserOut { /// Stable identifier.
    /// #     pub id: u64 }
    /// /// List users.
    /// #[endpoint]
    /// async fn list() -> Result<Json<Vec<UserOut>>> { Ok(Json(vec![])) }
    ///
    /// # fn main() {
    /// let router = Router::new().endpoint::<__moso_op_list>(HttpMethod::Get, "/users");
    /// assert_eq!(router.len(), 1);
    /// # }
    /// ```
    ///
    /// `moso::routes! { GET "/users" => list }` expands to exactly this;
    /// `Router::new().get("/users", moso::ep!(list))` is the same thing again.
    pub fn endpoint<E>(self, method: HttpMethod, path: &'static str) -> Self
    where
        E: Endpoint + HandlerFn + Clone + Default,
    {
        self.method(method, path, E::default())
    }

    /// Register several methods on one path.
    pub fn route(mut self, path: &'static str, methods: MethodRouter) -> Self {
        let path = validate_path(path);
        for (method, handler, spec, providers) in methods.handlers {
            self.entries.push(RouteEntry {
                method,
                path: path.to_owned(),
                spec,
                providers,
                handler,
                layers: Vec::new(),
                guards: Vec::new(),
                metadata: RouteMetadata::new(),
            });
        }
        self
    }

    // ── composition ───────────────────────────────────────────────────────

    /// Nest a router under `prefix`.
    ///
    /// Paths compose and so does metadata: the outer router's tags, security
    /// requirements and extra responses are pushed down onto the inner routes.
    /// A prefix containing a path parameter is allowed, and the parameter is
    /// available to the nested handlers.
    ///
    /// A prefix ending in a catch-all is not: nothing could follow it. That is
    /// recorded as a boot problem, reported by [`Router::conflicts`], rather
    /// than panicking here.
    pub fn nest(mut self, prefix: &'static str, router: Router) -> Self {
        let prefix = validate_path(prefix);
        if prefix.contains("{*") {
            self.problems.push(BootError::Other {
                message: format!("cannot nest a router under the catch-all prefix `{prefix}`"),
                notes: vec![
                    "a catch-all matches the rest of the path, so no nested route could ever \
                     match"
                        .to_owned(),
                ],
                fix: Some(format!(
                    "nest under a static prefix and serve the catch-all as one route:\n    \
                     .nest(\"{}\", router)",
                    prefix.split("/{*").next().unwrap_or("/")
                )),
            });
            return self;
        }

        let Router {
            entries,
            metadata: _inner_pending,
            fallback,
            method_not_allowed,
            axum_mounts,
            static_mounts,
            problems,
        } = router;

        for mut entry in entries {
            entry.path = join_paths(prefix, &entry.path);
            extend_metadata(&mut entry.metadata, &self.metadata);
            preview_metadata(&mut entry.spec, &self.metadata);
            self.entries.push(entry);
        }
        for (mounted_at, mounted) in axum_mounts {
            self.axum_mounts
                .push((join_paths(prefix, &mounted_at), mounted));
        }
        for (mounted_at, source) in static_mounts {
            self.static_mounts
                .push((join_paths(prefix, &mounted_at), source));
        }
        self.adopt_fallbacks(fallback, method_not_allowed);
        self.problems.extend(problems);
        self
    }

    /// Merge a router at the same level, with no prefix.
    ///
    /// Unlike [`Router::nest`], the outer router's accumulated metadata is
    /// *not* pushed down: a merged router is a sibling that has already
    /// described itself. Metadata applied *after* the merge reaches the merged
    /// routes, exactly as it reaches any other route registered so far.
    pub fn merge(mut self, router: Router) -> Self {
        let Router {
            entries,
            metadata: _inner_pending,
            fallback,
            method_not_allowed,
            axum_mounts,
            static_mounts,
            problems,
        } = router;

        self.entries.extend(entries);
        self.axum_mounts.extend(axum_mounts);
        self.static_mounts.extend(static_mounts);
        self.adopt_fallbacks(fallback, method_not_allowed);
        self.problems.extend(problems);
        self
    }

    /// Take an absorbed router's fallbacks, but never overwrite our own.
    ///
    /// One composed router serves one fallback, so `merge`ing a router that set
    /// one has to mean something: it means "use it, unless the outer router has
    /// an opinion".
    fn adopt_fallbacks(
        &mut self,
        fallback: Option<BoxedHandler>,
        method_not_allowed: Option<BoxedHandler>,
    ) {
        if self.fallback.is_none() {
            self.fallback = fallback;
        }
        if self.method_not_allowed.is_none() {
            self.method_not_allowed = method_not_allowed;
        }
    }

    /// Serve static files under `path`.
    ///
    /// Refuses path traversal, serves precompressed siblings when the client
    /// accepts them, and sets long-lived cache headers for fingerprinted names.
    pub fn static_files(mut self, path: &'static str, source: StaticSource) -> Self {
        let path = validate_path(path);
        self.static_mounts.push((path.to_owned(), source));
        self
    }

    // ── OpenAPI metadata, applied to routes registered so far ─────────────

    /// Tag every route registered so far.
    pub fn tag(mut self, tag: &'static str) -> Self {
        self.metadata.tag(tag);
        for entry in &mut self.entries {
            entry.metadata.tag(tag);
            entry.spec.merge_tag(tag);
        }
        self
    }

    /// Require `scheme` on every route registered so far.
    pub fn security(mut self, scheme: SecurityRequirement) -> Self {
        self.metadata.security(scheme.clone());
        for entry in &mut self.entries {
            entry.metadata.security(scheme.clone());
            preview_security(&mut entry.spec, scheme.clone());
        }
        self
    }

    /// Mark every route registered so far deprecated.
    pub fn deprecated(mut self) -> Self {
        self.metadata.deprecate();
        for entry in &mut self.entries {
            entry.metadata.deprecate();
            entry.spec.deprecated = true;
        }
        self
    }

    /// Document an extra response on every route registered so far.
    ///
    /// The 429 a rate limiter can return, say — a response no handler produces
    /// but every client must handle.
    pub fn responds(mut self, status: u16, spec: ResponseSpec) -> Self {
        self.metadata.responds(status, spec.clone());
        for entry in &mut self.entries {
            entry.metadata.responds(status, spec.clone());
            preview_response(&mut entry.spec, status, spec.clone());
        }
        self
    }

    /// Hide every route registered so far from the OpenAPI document.
    ///
    /// The routes still exist and are still served. This is for internal
    /// endpoints, not for security.
    pub fn hidden(mut self) -> Self {
        self.metadata.hide();
        for entry in &mut self.entries {
            entry.metadata.hide();
            entry.spec.hidden = true;
        }
        self
    }

    // ── middleware ────────────────────────────────────────────────────────

    /// Apply a Tower layer to every route registered so far.
    ///
    /// The layer applies to the routes registered *before* it, not after — so
    /// the position of the call in the chain is what scopes it:
    ///
    /// ```
    /// use moso::prelude::*;
    /// use moso::middleware::Next;
    /// use moso::response::NoContent;
    /// use moso::{Request, Response};
    /// # /// Count attempts.
    /// # #[moso::middleware]
    /// # async fn throttle(req: Request, next: Next) -> Result<Response> {
    /// #     Ok(next.run(req).await) }
    /// # /// Sign in.
    /// # #[endpoint] async fn login() -> Result<NoContent> { Ok(NoContent) }
    /// # /// Sign out.
    /// # #[endpoint] async fn logout() -> Result<NoContent> { Ok(NoContent) }
    /// # fn main() {
    /// let router = Router::new()
    ///     .post("/auth/login", moso::ep!(login))
    ///     .layer(ThrottleLayer::new())          // login only
    ///     .post("/auth/logout", moso::ep!(logout)); // not throttled
    ///
    /// assert_eq!(router.entries()[0].layers.len(), 1);
    /// assert_eq!(router.entries()[1].layers.len(), 0);
    /// # }
    /// ```
    pub fn layer<L>(mut self, layer: L) -> Self
    where
        L: tower::Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<Request, Error = core::convert::Infallible>
            + Clone
            + Send
            + Sync
            + 'static,
        <L::Service as tower::Service<Request>>::Response: crate::IntoResponse + 'static,
        <L::Service as tower::Service<Request>>::Future: Send + 'static,
    {
        let erased = crate::middleware::layer_fn(core::any::type_name::<L>(), layer);
        self.push_layer(erased);
        self
    }

    /// Apply an already-erased layer to every route registered so far.
    fn push_layer(&mut self, layer: Arc<dyn CustomLayer>) {
        for entry in &mut self.entries {
            entry.layers.push(Arc::clone(&layer));
        }
    }

    /// Apply a guard to every route registered so far.
    ///
    /// Unlike a layer, a guard contributes to the OpenAPI document: the 401 or
    /// 403 it can return appears on every operation it protects. That is the
    /// gap it exists to close — a bare layer that rejects requests makes the
    /// document quietly wrong.
    pub fn guard<G: Guard>(mut self, guard: G) -> Self {
        let guard: Arc<dyn DynGuard> = Arc::new(guard);
        for entry in &mut self.entries {
            describe_into(&mut entry.spec, |op| guard.describe_dyn(op));
            entry.guards.push(Arc::clone(&guard));
        }
        self
    }

    /// Apply a timeout to every route registered so far.
    ///
    /// A shorthand for the common per-router case; the global default lives in
    /// the middleware stack. Expiry is a 504 problem document, not a dropped
    /// connection.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.push_layer(Arc::new(RouteTimeout { timeout }));
        self
    }

    // ── fallbacks ─────────────────────────────────────────────────────────

    /// The handler for requests that match no route.
    ///
    /// Defaults to a 404 problem document, which is usually what you want; set
    /// one to serve an SPA's `index.html`.
    pub fn fallback<H, M>(mut self, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.fallback = Some(boxed(handler));
        self
    }

    /// The handler for requests whose path matches but whose method does not.
    ///
    /// Defaults to a 405 problem with an `Allow` header listing the methods the
    /// path does support. A handler set here replaces the body; the `Allow`
    /// header is added either way.
    pub fn method_not_allowed<H, M>(mut self, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.method_not_allowed = Some(boxed(handler));
        self
    }

    // ── escape hatches ────────────────────────────────────────────────────

    /// Mount an arbitrary Axum router under `prefix`.
    ///
    /// Contributes nothing to the OpenAPI document — Moso cannot inspect an
    /// Axum router — and the routes are invisible to boot-time validation. That
    /// is the price of the hatch and it is stated rather than hidden.
    pub fn mount_axum(mut self, prefix: &'static str, router: axum::Router<()>) -> Self {
        let prefix = validate_path(prefix);
        self.axum_mounts.push((prefix.to_owned(), router));
        self
    }

    /// Consume this router, yielding the underlying Axum router.
    ///
    /// OpenAPI metadata is dropped and no application state is attached, so
    /// `Inject<T>` will fail at runtime. Use it at the very edge of an
    /// application — to hand the routes to something that wants an
    /// `axum::Router` — and prefer
    /// [`App::into_service`](crate::App::into_service), which keeps the state.
    ///
    /// Requests are answered only if something upstream put a [`RequestCtx`] or
    /// an `Arc<AppState>` into the request extensions; without one, every
    /// matched route answers 500 saying exactly that. Routes that conflict are
    /// resolved first-registration-wins here, because `matchit` would panic;
    /// [`Router::conflicts`] is where they are reported.
    pub fn into_axum(self) -> axum::Router<()> {
        let Router {
            entries,
            fallback,
            method_not_allowed,
            axum_mounts,
            static_mounts,
            ..
        } = self;

        // Grouped by *shape*, not by the path as written: `matchit` cannot tell
        // `/users/{id}` from `/users/{user_id}`, so registering both would
        // panic inside `axum::Router::route`. Dropping the second is what
        // "first registration wins" means, and `Router::conflicts` has already
        // recorded it as a boot error — a panic here would take the whole
        // report down with it and leave the reader with a backtrace instead.
        let mut grouped: IndexMap<String, (String, Vec<RouteEntry>)> = IndexMap::new();
        for entry in entries {
            grouped
                .entry(path_shape(&entry.path))
                .or_insert_with(|| (entry.path.clone(), Vec::new()))
                .1
                .push(entry);
        }

        let mut router = axum::Router::new();
        for (_, (path, entries)) in grouped {
            let mut methods = axum::routing::MethodRouter::<()>::new();
            let mut registered: Vec<HttpMethod> = Vec::with_capacity(entries.len());
            for entry in entries {
                if registered.contains(&entry.method) {
                    continue;
                }
                registered.push(entry.method);
                let filter = method_filter(entry.method);
                methods = methods.on_service(filter, entry.into_service());
            }
            // Always replace Axum's method-router fallback: left alone it
            // answers a bare 405 with no body, which is the one place a
            // framework-generated error would not be an RFC 9457 problem. The
            // `Allow` header survives either way, because Axum sets it on
            // whatever this fallback returned. See [`MethodNotAllowed`].
            methods = match &method_not_allowed {
                Some(handler) => methods.fallback_service(RouteHandler {
                    handler: Arc::clone(handler),
                    guards: Arc::from(Vec::new()),
                }),
                None => methods.fallback_service(MethodNotAllowed),
            };
            router = router.route(&path, methods);
        }

        // A static mount at the root has no path of its own to be routed at, so
        // it becomes the fallback — unless the router set one, which wins.
        let mut root_static = None;
        for (prefix, source) in static_mounts {
            let service = Route::new(StaticService {
                source: Arc::new(source),
            });
            if is_root(&prefix) {
                root_static.get_or_insert(service);
            } else {
                router = router.nest_service(&prefix, service);
            }
        }

        for (prefix, mounted) in axum_mounts {
            router = if is_root(&prefix) {
                router.merge(mounted)
            } else {
                router.nest(&prefix, mounted)
            };
        }

        match (fallback, root_static) {
            (Some(handler), _) => router.fallback_service(RouteHandler {
                handler,
                guards: Arc::from(Vec::new()),
            }),
            (None, Some(service)) => router.fallback_service(service),
            (None, None) => router.fallback_service(NotFound),
        }
    }

    // ── inspection, used by App::build and by `moso routes` ───────────────

    /// The registered routes, in registration order.
    pub fn entries(&self) -> &[RouteEntry] {
        &self.entries
    }

    /// The registered routes, consumed.
    pub fn into_entries(self) -> Vec<RouteEntry> {
        self.entries
    }

    /// The metadata accumulated by [`Router::tag`] and friends.
    ///
    /// It has already been applied to every route registered so far. It is kept
    /// so that [`Router::nest`] can push it down onto the routes it absorbs
    /// later; it is *not* applied to routes registered afterwards with
    /// [`Router::get`] and friends, which is the "so far" convention this
    /// module's header describes.
    pub fn metadata(&self) -> &RouteMetadata {
        &self.metadata
    }

    /// How many routes are registered.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the router is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The fallback handler, if one was set.
    pub fn fallback_handler(&self) -> Option<&BoxedHandler> {
        self.fallback.as_ref()
    }

    /// The method-not-allowed handler, if one was set.
    pub fn method_not_allowed_handler(&self) -> Option<&BoxedHandler> {
        self.method_not_allowed.as_ref()
    }

    /// Axum routers mounted through [`Router::mount_axum`].
    pub fn axum_mounts(&self) -> &[(String, axum::Router<()>)] {
        &self.axum_mounts
    }

    /// Static file mounts.
    pub fn static_mounts(&self) -> &[(String, StaticSource)] {
        &self.static_mounts
    }

    /// One row per route, in registration order — what `moso routes` prints.
    ///
    /// Reads the registration-time preview, so it costs nothing beyond the
    /// clones: no handler is invoked and no schema is generated.
    pub fn describe(&self) -> Vec<RouteInfo> {
        self.entries.iter().map(RouteInfo::from_entry).collect()
    }

    /// Find conflicting routes, and any structural problem composition found.
    ///
    /// Two routes conflict when they share a method and their paths cannot be
    /// told apart by `matchit`: identical, or differing only in a parameter's
    /// name. Run by `App::build()` *before* the Axum router is constructed, so
    /// the report can name both source locations instead of a panic naming
    /// neither.
    ///
    /// Static and dynamic segments at the same position are *not* a conflict —
    /// `matchit` backtracks, so `/users/me` and `/users/{id}` coexist with the
    /// static route winning — and reporting them would be a false alarm on a
    /// pattern every API uses.
    pub fn conflicts(&self) -> Vec<BootError> {
        let mut problems = self.problems.clone();
        let mut seen: IndexMap<(HttpMethod, String), (&str, Option<SourceLocation>)> =
            IndexMap::new();

        for entry in &self.entries {
            let key = (entry.method, path_shape(&entry.path));
            match seen.entry(key) {
                Entry::Occupied(occupied) => {
                    let (first_path, first) = *occupied.get();
                    let reason = if first_path == entry.path {
                        ConflictReason::Identical
                    } else {
                        ConflictReason::ParameterNameMismatch
                    };
                    problems.push(BootError::RouteConflict {
                        method: entry.method.as_upper_str(),
                        first_path: first_path.to_owned(),
                        first,
                        second_path: entry.path.clone(),
                        second: entry.spec.source,
                        reason,
                    });
                }
                Entry::Vacant(vacant) => {
                    vacant.insert((&entry.path, entry.spec.source));
                }
            }
        }

        problems
    }
}

impl core::fmt::Debug for Router {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Router")
            .field("routes", &self.entries.len())
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

/// Describe an endpoint in isolation, for the registration-time preview.
///
/// The generator is thrown away with the builder: named schemas registered
/// while describing are re-registered against the document's own generator by
/// [`RouteEntry::describe`], which is the description that reaches the wire.
fn describe_endpoint<E: Endpoint>() -> OperationSpec {
    let mut builder = OperationBuilder::new(SchemaGenerator::new(crate::COMPONENTS_SCHEMAS_PREFIX));
    E::spec(&mut builder);
    builder.into_spec()
}

/// Run one describer against an existing preview spec.
fn describe_into(spec: &mut OperationSpec, describe: impl FnOnce(&mut OperationBuilder)) {
    let generator = SchemaGenerator::new(crate::COMPONENTS_SCHEMAS_PREFIX);
    let mut builder = OperationBuilder::from_parts(generator, core::mem::take(spec));
    describe(&mut builder);
    *spec = builder.into_spec();
}

/// Fold router metadata into a preview spec.
///
/// Mirrors [`RouteMetadata::apply`] over the preview's own merge rules: tags
/// and security requirements are deduplicated, a response only fills a status
/// nobody claimed, and the flags are sticky.
fn preview_metadata(spec: &mut OperationSpec, metadata: &RouteMetadata) {
    for tag in &metadata.tags {
        spec.merge_tag(tag.clone());
    }
    for requirement in &metadata.security {
        preview_security(spec, requirement.clone());
    }
    for (status, response) in &metadata.responses {
        preview_response(spec, *status, response.clone());
    }
    for (key, value) in &metadata.extensions {
        spec.merge_extension(key.clone(), value.clone());
    }
    spec.deprecated |= metadata.deprecated;
    spec.hidden |= metadata.hidden;
}

/// Add a security requirement to a preview spec unless it is already there.
fn preview_security(spec: &mut OperationSpec, requirement: SecurityRequirement) {
    let requirements = spec.security.get_or_insert_with(Vec::new);
    if !requirements.contains(&requirement) {
        requirements.push(requirement);
    }
}

/// Add a response to a preview spec unless the status is already documented.
fn preview_response(spec: &mut OperationSpec, status: u16, response: ResponseSpec) {
    let mut generator = SchemaGenerator::new(crate::COMPONENTS_SCHEMAS_PREFIX);
    let built = response.build(&mut generator);
    spec.responses.entry(status.to_string()).or_insert(built);
}

/// Absorb an outer router's metadata into an inner route's, as `nest` does.
///
/// The same rule as [`RouteMetadata::extend_from`]: the inner, more specific
/// declaration is listed first and the outer one is appended, so a nested
/// router's own tag leads and the section tag follows.
fn extend_metadata(inner: &mut RouteMetadata, outer: &RouteMetadata) {
    for tag in &outer.tags {
        inner.tag(tag.clone());
    }
    for requirement in &outer.security {
        inner.security(requirement.clone());
    }
    for (status, response) in &outer.responses {
        inner.responds(*status, response.clone());
    }
    for (key, value) in &outer.extensions {
        inner.extensions.entry(key.clone()).or_insert(value.clone());
    }
    inner.deprecated |= outer.deprecated;
    inner.hidden |= outer.hidden;
}

/// The Axum method filter for a Moso method.
fn method_filter(method: HttpMethod) -> axum::routing::MethodFilter {
    use axum::routing::MethodFilter;
    match method {
        HttpMethod::Get => MethodFilter::GET,
        HttpMethod::Put => MethodFilter::PUT,
        HttpMethod::Post => MethodFilter::POST,
        HttpMethod::Delete => MethodFilter::DELETE,
        HttpMethod::Options => MethodFilter::OPTIONS,
        HttpMethod::Head => MethodFilter::HEAD,
        HttpMethod::Patch => MethodFilter::PATCH,
        HttpMethod::Trace => MethodFilter::TRACE,
    }
}

// ---------------------------------------------------------------------------
// RouteInfo
// ---------------------------------------------------------------------------

/// One row of `moso routes`.
///
/// ```text
/// METHOD  PATH                HANDLER        AUTH      TAGS    SOURCE
/// GET     /api/v1/users       users::list    session   users   src/routes/users.rs:14
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteInfo {
    /// The HTTP method.
    pub method: HttpMethod,
    /// The full path, prefixes applied.
    pub path: String,
    /// The handler's name, or `<undocumented>` for a plain `async fn`.
    pub handler: &'static str,
    /// The `operationId`, when the handler declared one.
    pub operation_id: Option<String>,
    /// The one-line summary from the handler's doc comment.
    pub summary: Option<String>,
    /// The tags the operation carries.
    pub tags: Vec<String>,
    /// The security schemes required, by name. Empty means unauthenticated.
    pub security: Vec<String>,
    /// Where `#[endpoint]` was written.
    pub source: Option<SourceLocation>,
    /// Whether the route carries an `#[endpoint]` description.
    pub documented: bool,
    /// Whether the route is excluded from the OpenAPI document.
    pub hidden: bool,
    /// Whether clients should migrate away from it.
    pub deprecated: bool,
    /// The names of the layers applied, innermost first.
    pub layers: Vec<&'static str>,
    /// How many guards protect the route.
    pub guards: usize,
}

impl RouteInfo {
    /// Read one row off a registered route.
    fn from_entry(entry: &RouteEntry) -> Self {
        let security = entry
            .spec
            .security
            .as_ref()
            .map(|requirements| {
                requirements
                    .iter()
                    .flat_map(|requirement| requirement.schemes().map(|(name, _)| name.to_owned()))
                    .collect()
            })
            .unwrap_or_default();

        Self {
            method: entry.method,
            path: entry.path.clone(),
            handler: entry.handler.name(),
            operation_id: entry.spec.operation_id.clone(),
            summary: entry.spec.summary.clone(),
            tags: entry.spec.tags.clone(),
            security,
            source: entry.spec.source,
            documented: entry.handler.name() != UndocumentedEndpoint::NAME,
            hidden: entry.spec.hidden,
            deprecated: entry.spec.deprecated,
            layers: entry.layers.iter().map(|layer| layer.name()).collect(),
            guards: entry.guards.len(),
        }
    }
}

// ---------------------------------------------------------------------------
// MethodRouter
// ---------------------------------------------------------------------------

/// Several methods on one path.
///
/// ```
/// use moso::prelude::*;
/// use moso::router::{get, post};
/// # /// A user, as the API returns one.
/// # #[derive(Schema)] pub struct UserOut { /// Stable identifier.
/// #     pub id: u64 }
/// # /// List users.
/// # #[endpoint] async fn list() -> Result<Json<Vec<UserOut>>> { Ok(Json(vec![])) }
/// # /// Create a user.
/// # #[endpoint] async fn create() -> Result<Created<UserOut>> {
/// #     Ok(Created::at("/users/1", UserOut { id: 1 })) }
/// # fn main() {
/// let router = Router::new()
///     .route("/users", get(moso::ep!(list)).post(moso::ep!(create)));
/// assert_eq!(router.len(), 2);
/// # }
/// ```
///
/// Provided for familiarity. `Router::get(path, handler)` is the documented
/// preference because it keeps the path next to each handler, which is what a
/// reader scanning a route table is looking for.
#[derive(Default)]
pub struct MethodRouter {
    handlers: Vec<(
        HttpMethod,
        BoxedHandler,
        OperationSpec,
        &'static [crate::di::ProviderReq],
    )>,
}

impl MethodRouter {
    /// An empty method router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a handler for `method`.
    pub fn on<H, M>(mut self, method: HttpMethod, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.handlers.push((
            method,
            boxed(handler),
            describe_endpoint::<H::Endpoint>(),
            <H::Endpoint as Endpoint>::required_providers(),
        ));
        self
    }

    /// Add a `GET` handler.
    pub fn get<H, M>(self, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.on(HttpMethod::Get, handler)
    }

    /// Add a `POST` handler.
    pub fn post<H, M>(self, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.on(HttpMethod::Post, handler)
    }

    /// Add a `PUT` handler.
    pub fn put<H, M>(self, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.on(HttpMethod::Put, handler)
    }

    /// Add a `PATCH` handler.
    pub fn patch<H, M>(self, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.on(HttpMethod::Patch, handler)
    }

    /// Add a `DELETE` handler.
    pub fn delete<H, M>(self, handler: H) -> Self
    where
        H: Handler<M>,
        M: 'static,
    {
        self.on(HttpMethod::Delete, handler)
    }

    /// The methods this router answers.
    pub fn methods(&self) -> Vec<HttpMethod> {
        self.handlers
            .iter()
            .map(|(method, _, _, _)| *method)
            .collect()
    }
}

impl core::fmt::Debug for MethodRouter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MethodRouter")
            .field("methods", &self.methods())
            .finish()
    }
}

/// Start a [`MethodRouter`] with a `GET` handler.
pub fn get<H, M>(handler: H) -> MethodRouter
where
    H: Handler<M>,
    M: 'static,
{
    MethodRouter::new().get(handler)
}

/// Start a [`MethodRouter`] with a `POST` handler.
pub fn post<H, M>(handler: H) -> MethodRouter
where
    H: Handler<M>,
    M: 'static,
{
    MethodRouter::new().post(handler)
}

/// Start a [`MethodRouter`] with a `PUT` handler.
pub fn put<H, M>(handler: H) -> MethodRouter
where
    H: Handler<M>,
    M: 'static,
{
    MethodRouter::new().put(handler)
}

/// Start a [`MethodRouter`] with a `PATCH` handler.
pub fn patch<H, M>(handler: H) -> MethodRouter
where
    H: Handler<M>,
    M: 'static,
{
    MethodRouter::new().patch(handler)
}

/// Start a [`MethodRouter`] with a `DELETE` handler.
pub fn delete<H, M>(handler: H) -> MethodRouter
where
    H: Handler<M>,
    M: 'static,
{
    MethodRouter::new().delete(handler)
}

// ---------------------------------------------------------------------------
// StaticSource
// ---------------------------------------------------------------------------

/// Where [`Router::static_files`] reads from.
#[derive(Debug, Clone)]
pub enum StaticSource {
    /// A directory on disk, read per request.
    ///
    /// Right for development. In production it makes the container's filesystem
    /// part of the deployment, which is usually not what anyone intended.
    Dir {
        /// The directory root. Requests cannot escape it.
        root: std::path::PathBuf,
        /// The file served for a directory request.
        index: Option<String>,
        /// The file served when nothing matches — an SPA's `index.html`.
        fallback: Option<String>,
    },
    /// Files embedded in the binary at compile time.
    ///
    /// One artefact to deploy, no filesystem, no cold-start read. What
    /// `moso build --embed-assets` produces.
    Embedded {
        /// `(path, bytes, content-type)` for each file, sorted by path.
        files: &'static [EmbeddedFile],
        /// The file served when nothing matches.
        fallback: Option<&'static str>,
    },
}

impl StaticSource {
    /// Serve from a directory on disk.
    pub fn dir(root: impl Into<std::path::PathBuf>) -> Self {
        StaticSource::Dir {
            root: root.into(),
            index: Some("index.html".to_owned()),
            fallback: None,
        }
    }

    /// Serve from a directory, falling back to `fallback` for unmatched paths.
    ///
    /// The single-page-application arrangement: client-side routes that the
    /// server has never heard of still load the application shell.
    pub fn spa(root: impl Into<std::path::PathBuf>, fallback: impl Into<String>) -> Self {
        StaticSource::Dir {
            root: root.into(),
            index: Some("index.html".to_owned()),
            fallback: Some(fallback.into()),
        }
    }

    /// Serve files embedded at compile time.
    pub fn embedded(files: &'static [EmbeddedFile]) -> Self {
        StaticSource::Embedded {
            files,
            fallback: None,
        }
    }
}

/// One file embedded in the binary.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedFile {
    /// The path it is served at, relative to the mount point, without a leading
    /// slash.
    pub path: &'static str,
    /// The file's bytes.
    pub bytes: &'static [u8],
    /// The `Content-Type` to send.
    pub content_type: &'static str,
    /// A precomputed strong `ETag`, so a conditional request costs no hashing.
    pub etag: &'static str,
}

/// The service a [`StaticSource`] mount becomes.
#[derive(Clone)]
struct StaticService {
    source: Arc<StaticSource>,
}

impl Service<Request> for StaticService {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let source = Arc::clone(&self.source);
        Box::pin(async move {
            let (parts, _body) = req.into_parts();
            Ok(serve_static(&source, &parts).await)
        })
    }
}

/// Serve one request from a static source.
async fn serve_static(source: &StaticSource, parts: &http::request::Parts) -> Response {
    let head_only = parts.method == http::Method::HEAD;
    if parts.method != http::Method::GET && !head_only {
        let mut response = problem_response(
            &ErrorKind::MethodNotAllowed,
            "static files answer GET and HEAD",
        );
        response.headers_mut().insert(
            http::header::ALLOW,
            http::HeaderValue::from_static("GET, HEAD"),
        );
        return response;
    }

    let Some(relative) = safe_relative_path(parts.uri.path()) else {
        return problem_response(&ErrorKind::NotFound, "no such file");
    };
    let trailing_slash = parts.uri.path().ends_with('/');

    match source {
        StaticSource::Embedded { files, fallback } => serve_embedded(
            files,
            *fallback,
            &relative,
            trailing_slash,
            parts,
            head_only,
        ),
        StaticSource::Dir {
            root,
            index,
            fallback,
        } => {
            serve_directory(
                root,
                index.as_deref(),
                fallback.as_deref(),
                &relative,
                trailing_slash,
                parts,
                head_only,
            )
            .await
        }
    }
}

/// Reduce a request path to a relative path that cannot escape the root.
///
/// Percent-decodes, drops `.` and empty segments, and rejects the request
/// outright on `..`, a NUL, a backslash or an absolute Windows-style segment.
/// Rejecting rather than normalising is deliberate: a request containing `..`
/// is not a request for a file, it is an attempt.
fn safe_relative_path(path: &str) -> Option<String> {
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .ok()?;
    let mut segments: Vec<&str> = Vec::new();
    for segment in decoded.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".."
            || segment.contains('\\')
            || segment.contains('\0')
            || segment.contains(':')
        {
            return None;
        }
        segments.push(segment);
    }
    Some(segments.join("/"))
}

/// Whether the client accepts a content coding, honouring `q=0`.
fn accepts_encoding(headers: &http::HeaderMap, coding: &str) -> bool {
    let Some(value) = headers.get(http::header::ACCEPT_ENCODING) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    value.split(',').any(|part| {
        let mut pieces = part.split(';');
        let token = pieces.next().unwrap_or("").trim();
        if !token.eq_ignore_ascii_case(coding) {
            return false;
        }
        !pieces.any(|parameter| {
            let parameter = parameter.trim();
            parameter == "q=0" || parameter == "q=0.0" || parameter == "q=0.000"
        })
    })
}

/// Whether an `If-None-Match` header matches `etag`.
fn etag_matches(headers: &http::HeaderMap, etag: &str) -> bool {
    let Some(value) = headers.get(http::header::IF_NONE_MATCH) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    value
        .split(',')
        .any(|candidate| candidate.trim() == "*" || candidate.trim() == etag)
}

/// Whether a file name carries a content hash, and may therefore be cached
/// forever.
///
/// `app.4f3a9c1e.js` yes; `index.html` no. The rule is deliberately narrow: a
/// wrong "immutable" is a stale asset nobody can flush.
fn is_fingerprinted(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() < 3 {
        return false;
    }
    parts[1..parts.len() - 1].iter().any(|part| {
        part.len() >= 8
            && part.chars().all(|c| c.is_ascii_alphanumeric())
            && part.chars().any(|c| c.is_ascii_digit())
    })
}

/// The `Cache-Control` value for a served file.
fn cache_control_for(name: &str) -> &'static str {
    if is_fingerprinted(name) {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=0, must-revalidate"
    }
}

/// Assemble a static file response.
fn file_response(
    bytes: Vec<u8>,
    content_type: &str,
    etag: &str,
    cache_control: &'static str,
    encoding: Option<&'static str>,
    head_only: bool,
) -> Response {
    let length = bytes.len();
    let body = if head_only {
        axum::body::Body::empty()
    } else {
        axum::body::Body::from(bytes)
    };
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    if let Ok(value) = http::HeaderValue::from_str(content_type) {
        headers.insert(http::header::CONTENT_TYPE, value);
    }
    if let Ok(value) = http::HeaderValue::from_str(etag) {
        headers.insert(http::header::ETAG, value);
    }
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static(cache_control),
    );
    headers.insert(
        http::header::CONTENT_LENGTH,
        http::HeaderValue::from(length as u64),
    );
    if let Some(encoding) = encoding {
        headers.insert(
            http::header::CONTENT_ENCODING,
            http::HeaderValue::from_static(encoding),
        );
        headers.insert(
            http::header::VARY,
            http::HeaderValue::from_static("accept-encoding"),
        );
    }
    response
}

/// The 304 answer to a conditional request whose `ETag` still matches.
fn not_modified(etag: &str, cache_control: &'static str) -> Response {
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = http::StatusCode::NOT_MODIFIED;
    let headers = response.headers_mut();
    if let Ok(value) = http::HeaderValue::from_str(etag) {
        headers.insert(http::header::ETAG, value);
    }
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static(cache_control),
    );
    response
}

/// Serve from the embedded table.
fn serve_embedded(
    files: &'static [EmbeddedFile],
    fallback: Option<&'static str>,
    relative: &str,
    trailing_slash: bool,
    parts: &http::request::Parts,
    head_only: bool,
) -> Response {
    let find = |path: &str| files.iter().find(|file| file.path == path);

    let mut wanted = relative.to_owned();
    if wanted.is_empty() || trailing_slash {
        let index = if wanted.is_empty() {
            "index.html".to_owned()
        } else {
            format!("{wanted}/index.html")
        };
        if find(&index).is_some() {
            wanted = index;
        }
    }

    let file = match find(&wanted).or_else(|| fallback.and_then(find)) {
        Some(file) => file,
        None => {
            return problem_response(&ErrorKind::NotFound, "no such file");
        }
    };

    let cache_control = cache_control_for(file.path);
    if etag_matches(&parts.headers, file.etag) {
        return not_modified(file.etag, cache_control);
    }

    for (coding, suffix) in [("br", ".br"), ("gzip", ".gz")] {
        if !accepts_encoding(&parts.headers, coding) {
            continue;
        }
        if let Some(compressed) = find(&format!("{}{suffix}", file.path)) {
            return file_response(
                compressed.bytes.to_vec(),
                file.content_type,
                file.etag,
                cache_control,
                Some(if coding == "br" { "br" } else { "gzip" }),
                head_only,
            );
        }
    }

    file_response(
        file.bytes.to_vec(),
        file.content_type,
        file.etag,
        cache_control,
        None,
        head_only,
    )
}

/// A file read from disk: its bytes and an `ETag` derived from its metadata.
struct DiskFile {
    bytes: Vec<u8>,
    etag: String,
}

/// Read `relative` under `root`, refusing anything that escapes it.
///
/// The escape check is done on the *canonical* paths, so a symbolic link
/// pointing outside the root is refused as firmly as a `..` segment would have
/// been. `std::fs` on the blocking pool rather than `tokio::fs`, which this
/// crate does not enable.
async fn read_under(root: std::path::PathBuf, relative: String) -> Option<DiskFile> {
    tokio::task::spawn_blocking(move || {
        let root = std::fs::canonicalize(&root).ok()?;
        let target = std::fs::canonicalize(root.join(&relative)).ok()?;
        if !target.starts_with(&root) {
            return None;
        }
        let metadata = std::fs::metadata(&target).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_secs())
            .unwrap_or_default();
        let bytes = std::fs::read(&target).ok()?;
        let length = bytes.len();
        Some(DiskFile {
            bytes,
            etag: format!("\"{length:x}-{modified:x}\""),
        })
    })
    .await
    .ok()
    .flatten()
}

/// Serve from a directory on disk.
async fn serve_directory(
    root: &std::path::Path,
    index: Option<&str>,
    fallback: Option<&str>,
    relative: &str,
    trailing_slash: bool,
    parts: &http::request::Parts,
    head_only: bool,
) -> Response {
    let mut candidates: Vec<String> = Vec::with_capacity(3);
    if let Some(index) = index
        && (relative.is_empty() || trailing_slash)
    {
        candidates.push(if relative.is_empty() {
            index.to_owned()
        } else {
            format!("{relative}/{index}")
        });
    }
    if !relative.is_empty() {
        candidates.push(relative.to_owned());
    }
    if let Some(fallback) = fallback {
        candidates.push(fallback.to_owned());
    }

    for candidate in candidates {
        let Some(file) = read_under(root.to_owned(), candidate.clone()).await else {
            continue;
        };
        let content_type =
            crate::response::file::content_type_for(std::path::Path::new(&candidate));
        let cache_control = cache_control_for(&candidate);
        if etag_matches(&parts.headers, &file.etag) {
            return not_modified(&file.etag, cache_control);
        }
        for (coding, suffix) in [("br", ".br"), ("gzip", ".gz")] {
            if !accepts_encoding(&parts.headers, coding) {
                continue;
            }
            if let Some(compressed) =
                read_under(root.to_owned(), format!("{candidate}{suffix}")).await
            {
                return file_response(
                    compressed.bytes,
                    content_type,
                    &file.etag,
                    cache_control,
                    Some(if coding == "br" { "br" } else { "gzip" }),
                    head_only,
                );
            }
        }
        return file_response(
            file.bytes,
            content_type,
            &file.etag,
            cache_control,
            None,
            head_only,
        );
    }

    problem_response(&ErrorKind::NotFound, "no such file")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::NoContent;

    // ── fixtures ──────────────────────────────────────────────────────────

    async fn plain() -> NoContent {
        NoContent
    }

    #[derive(Clone, Copy, Default)]
    struct Documented;

    impl Endpoint for Documented {
        const NAME: &'static str = "documented";

        fn spec(b: &mut OperationBuilder) {
            b.summary("A documented operation.");
            b.operation_id("documented");
            b.source("src/routes/test.rs", 7);
        }

        fn required_providers() -> &'static [crate::di::ProviderReq] {
            &[]
        }
    }

    impl HandlerFn for Documented {
        fn invoke(_req: Request, _ctx: RequestCtx) -> BoxFuture<'static, Response> {
            Box::pin(async { Response::new(axum::body::Body::empty()) })
        }
    }

    // ── positional path parameters ────────────────────────────────────────

    /// Build an operation, contribute `count` nameless path parameters the way
    /// a scalar or tuple `Path<T>` does, then name them from `path`.
    fn name_positional(path: &str, count: usize) -> Vec<String> {
        let mut op = OperationBuilder::new(moso_openapi::SchemaGenerator::default());
        for _ in 0..count {
            op.parameter(moso_openapi::Param::path("").schema_of::<u64>());
        }
        name_positional_path_parameters(path, &mut op);
        op.spec()
            .parameters
            .iter()
            .map(|parameter| parameter.name.clone())
            .collect()
    }

    #[test]
    fn a_scalar_path_extractor_takes_the_templates_only_name() {
        // `Path<u64>` on `/users/{id}` — the case that made every such route
        // fail `App::build` with "declared `id`, expected ``".
        assert_eq!(name_positional("/users/{id}", 1), ["id"]);
    }

    #[test]
    fn a_tuple_path_extractor_takes_the_names_in_order() {
        assert_eq!(
            name_positional("/users/{id}/posts/{slug}", 2),
            ["id", "slug"]
        );
    }

    #[test]
    fn a_catch_all_is_named_like_any_other_capture() {
        assert_eq!(name_positional("/files/{*rest}", 1), ["rest"]);
    }

    #[test]
    fn a_route_without_captures_leaves_the_placeholder_alone() {
        // Nothing to draw on, so the name stays empty and the document builder
        // reports the arity mismatch — which is the accurate complaint.
        assert_eq!(name_positional("/users", 1), [""]);
    }

    #[test]
    fn a_named_parameter_is_not_renamed_and_not_consumed() {
        let mut op = OperationBuilder::new(moso_openapi::SchemaGenerator::default());
        op.parameter(moso_openapi::Param::path("slug").schema_of::<u64>());
        op.parameter(moso_openapi::Param::path("").schema_of::<u64>());
        name_positional_path_parameters("/posts/{slug}/comments/{id}", &mut op);

        let names: Vec<&str> = op
            .spec()
            .parameters
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect();
        assert_eq!(names, ["slug", "id"], "`slug` was already claimed");
    }

    // ── path validation ───────────────────────────────────────────────────

    #[test]
    fn valid_paths_pass_through_unchanged() {
        const ROOT: &str = validate_path("/");
        const NESTED: &str = validate_path("/users/{id}/posts/{slug}");
        const CATCH_ALL: &str = validate_path("/files/{*rest}");
        const VERSIONED: &str = validate_path("/files/v1/{name}");
        assert_eq!(ROOT, "/");
        assert_eq!(NESTED, "/users/{id}/posts/{slug}");
        assert_eq!(CATCH_ALL, "/files/{*rest}");
        assert_eq!(VERSIONED, "/files/v1/{name}");
    }

    #[test]
    fn the_macro_checks_at_compile_time() {
        assert_eq!(crate::route_path!("/users/{id}"), "/users/{id}");
    }

    /// Every rejection, asserted through a `catch_unwind` so one test can cover
    /// the whole table. The `const` form of each of these is a compile error,
    /// which `route_path!`'s doctest demonstrates.
    #[test]
    fn invalid_paths_are_rejected() {
        let cases = [
            "",
            "users",
            "/users/:id",
            "/files/*rest",
            "/users/{id",
            "/users/id}",
            "/users/{}",
            "/users/{a}{b}",
            "/users/{id}x",
            // Static text on *either* side of a parameter, which is the rule
            // `routes!` has always enforced and this checker used to let past.
            "/files/v{version}",
            "/files/v{*rest}",
            "/{*rest}/more",
            "/users/{id}/posts/{id}",
            "/users/{user-id}",
            "/users/{{id}}",
        ];
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcomes: Vec<bool> = cases
            .iter()
            .map(|case| {
                let leaked: &'static str = Box::leak(case.to_string().into_boxed_str());
                std::panic::catch_unwind(|| validate_path(leaked)).is_err()
            })
            .collect();
        std::panic::set_hook(previous);
        for (case, rejected) in cases.iter().zip(outcomes) {
            assert!(rejected, "`{case}` should have been rejected");
        }
    }

    #[test]
    fn path_parameters_are_read_in_order() {
        assert_eq!(path_parameters("/users/{id}/posts/{slug}"), ["id", "slug"]);
        assert_eq!(path_parameters("/files/{*rest}"), ["rest"]);
        assert!(path_parameters("/healthz").is_empty());
    }

    #[test]
    fn shapes_erase_parameter_names() {
        assert_eq!(path_shape("/users/{id}"), "/users/{}");
        assert_eq!(path_shape("/users/{user_id}"), "/users/{}");
        assert_eq!(path_shape("/files/{*rest}"), "/files/{*}");
        assert_ne!(path_shape("/users/{id}"), path_shape("/users/me"));
    }

    #[test]
    fn paths_join_without_doubling_slashes() {
        assert_eq!(join_paths("/api/v1", "/users"), "/api/v1/users");
        assert_eq!(join_paths("/api/v1/", "/users"), "/api/v1/users");
        assert_eq!(join_paths("/", "/users"), "/users");
        assert_eq!(join_paths("/api", "/"), "/api");
        assert_eq!(join_paths("/", "/"), "/");
    }

    // ── registration ──────────────────────────────────────────────────────

    #[test]
    fn a_new_router_is_empty() {
        let router = Router::new();
        assert!(router.is_empty());
        assert_eq!(router.len(), 0);
        assert!(router.fallback_handler().is_none());
    }

    #[test]
    fn registration_records_the_method_path_and_description() {
        let router = Router::new()
            .get("/users", Documented)
            .post("/users", plain);

        assert_eq!(router.len(), 2);
        let entries = router.entries();
        assert_eq!(entries[0].method, HttpMethod::Get);
        assert_eq!(entries[0].path, "/users");
        assert_eq!(
            entries[0].spec.summary.as_deref(),
            Some("A documented operation.")
        );
        assert_eq!(
            entries[0].spec.source.map(|source| source.line),
            Some(7),
            "the source location must survive registration, for the conflict report"
        );
        assert_eq!(entries[1].method, HttpMethod::Post);
        assert!(entries[1].spec.summary.is_none());
    }

    #[test]
    fn the_explicit_endpoint_form_matches_the_shorthand() {
        let shorthand = Router::new().get("/users", Documented);
        let explicit = Router::new().endpoint::<Documented>(HttpMethod::Get, "/users");
        assert_eq!(shorthand.entries()[0].spec, explicit.entries()[0].spec);
        assert_eq!(shorthand.describe()[0].path, explicit.describe()[0].path);
    }

    #[test]
    fn a_method_router_registers_every_method_on_one_path() {
        let router = Router::new().route("/users", get(Documented).post(plain));
        assert_eq!(router.len(), 2);
        assert_eq!(router.entries()[0].method, HttpMethod::Get);
        assert_eq!(router.entries()[1].method, HttpMethod::Post);
        assert!(router.entries().iter().all(|entry| entry.path == "/users"));
    }

    // ── metadata ──────────────────────────────────────────────────────────

    #[test]
    fn metadata_applies_to_routes_registered_so_far_and_no_others() {
        let router = Router::new()
            .get("/before", Documented)
            .tag("tagged")
            .get("/after", Documented);

        assert_eq!(router.entries()[0].spec.tags, ["tagged"]);
        assert!(
            router.entries()[1].spec.tags.is_empty(),
            "metadata applied before a route must not reach it"
        );
    }

    #[test]
    fn metadata_reaches_both_the_preview_and_the_entry_metadata() {
        let router = Router::new()
            .get("/users", Documented)
            .tag("users")
            .security(SecurityRequirement::bearer("jwt"))
            .deprecated();

        let entry = &router.entries()[0];
        assert_eq!(entry.spec.tags, ["users"]);
        assert!(entry.spec.deprecated);
        assert_eq!(
            entry.spec.security.as_ref().map(Vec::len),
            Some(1),
            "the preview carries the requirement"
        );
        assert_eq!(entry.metadata.tags, ["users"]);
        assert_eq!(entry.metadata.security.len(), 1);
        assert!(
            entry.metadata.deprecated,
            "the metadata must survive for App::build to re-apply with the real generator"
        );
    }

    #[test]
    fn tags_are_not_duplicated_by_repeated_application() {
        let router = Router::new().get("/users", Documented).tag("users");
        // `nest` re-applies the outer metadata; doing it twice must be a no-op.
        let router = Router::new().nest("/api", router).tag("users");
        assert_eq!(router.entries()[0].spec.tags, ["users"]);
        assert_eq!(router.entries()[0].metadata.tags, ["users"]);
    }

    #[test]
    fn hidden_routes_are_still_registered() {
        let router = Router::new().get("/internal", Documented).hidden();
        assert_eq!(router.len(), 1);
        assert!(router.entries()[0].spec.hidden);
        assert!(router.describe()[0].hidden);
    }

    // ── composition ───────────────────────────────────────────────────────

    #[test]
    fn nesting_composes_paths() {
        let inner = Router::new()
            .get("/users", Documented)
            .get("/users/{id}", Documented);
        let router = Router::new().nest("/api/v1", inner);

        let paths: Vec<&str> = router
            .entries()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(paths, ["/api/v1/users", "/api/v1/users/{id}"]);
    }

    #[test]
    fn nesting_pushes_metadata_down() {
        let inner = Router::new().get("/users", Documented).tag("users");
        let outer = Router::new()
            .get("/healthz", plain)
            .security(SecurityRequirement::bearer("jwt"))
            .nest("/api/v1", inner);

        let nested = outer
            .entries()
            .iter()
            .find(|entry| entry.path == "/api/v1/users")
            .expect("the nested route");
        assert_eq!(
            nested.spec.tags,
            ["users"],
            "the inner router's own tag survives"
        );
        assert_eq!(
            nested.metadata.security.len(),
            1,
            "the outer router's security requirement reaches the nested route"
        );
    }

    #[test]
    fn merging_keeps_paths_and_does_not_push_metadata_down() {
        let inner = Router::new().get("/healthz", plain);
        let outer = Router::new()
            .get("/users", Documented)
            .tag("users")
            .merge(inner);

        let paths: Vec<&str> = outer
            .entries()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(paths, ["/users", "/healthz"]);
        assert!(
            outer.entries()[1].spec.tags.is_empty(),
            "a merged router is a sibling, not a subtree"
        );
    }

    #[test]
    fn metadata_after_a_merge_reaches_the_merged_routes() {
        let router = Router::new()
            .merge(Router::new().get("/healthz", plain))
            .tag("ops");
        assert_eq!(router.entries()[0].spec.tags, ["ops"]);
    }

    #[test]
    fn nesting_under_a_catch_all_is_a_boot_problem_not_a_panic() {
        let router = Router::new().nest("/files/{*rest}", Router::new().get("/x", plain));
        assert!(router.is_empty());
        assert_eq!(router.conflicts().len(), 1);
    }

    #[test]
    fn an_absorbed_fallback_is_adopted_only_when_there_is_none() {
        let inner = Router::new().fallback(plain);
        let router = Router::new().merge(inner);
        assert!(router.fallback_handler().is_some());

        let outer_first = Router::new()
            .fallback(Documented)
            .merge(Router::new().fallback(plain));
        assert_eq!(
            outer_first.fallback_handler().map(|handler| handler.name()),
            Some("documented")
        );
    }

    // ── conflicts ─────────────────────────────────────────────────────────

    #[test]
    fn identical_routes_conflict() {
        let router = Router::new()
            .get("/users", Documented)
            .get("/users", Documented);
        let conflicts = router.conflicts();
        assert_eq!(conflicts.len(), 1);
        match &conflicts[0] {
            BootError::RouteConflict {
                method,
                first_path,
                second_path,
                reason,
                first,
                ..
            } => {
                assert_eq!(*method, "GET");
                assert_eq!(first_path, "/users");
                assert_eq!(second_path, "/users");
                assert_eq!(*reason, ConflictReason::Identical);
                assert!(first.is_some(), "the report must name the first location");
            }
            other => panic!("expected a route conflict, got {other:?}"),
        }
    }

    #[test]
    fn parameters_that_differ_only_in_name_conflict() {
        let router = Router::new()
            .get("/users/{id}", Documented)
            .get("/users/{user_id}", Documented);
        let conflicts = router.conflicts();
        assert_eq!(conflicts.len(), 1);
        assert!(matches!(
            conflicts[0],
            BootError::RouteConflict {
                reason: ConflictReason::ParameterNameMismatch,
                ..
            }
        ));
    }

    #[test]
    fn different_methods_and_static_alternatives_do_not_conflict() {
        let router = Router::new()
            .get("/users", Documented)
            .post("/users", Documented)
            .get("/users/me", Documented)
            .get("/users/{id}", Documented);
        assert!(router.conflicts().is_empty());
    }

    #[test]
    fn conflicts_survive_nesting() {
        let router = Router::new()
            .nest("/api", Router::new().get("/users", Documented))
            .nest("/api", Router::new().get("/users", Documented));
        assert_eq!(router.conflicts().len(), 1);
    }

    // ── introspection ─────────────────────────────────────────────────────

    #[test]
    fn describe_reports_one_row_per_route() {
        let router = Router::new()
            .get("/users", Documented)
            .post("/users", plain)
            .tag("users");

        let rows = router.describe();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].handler, "documented");
        assert_eq!(rows[0].operation_id.as_deref(), Some("documented"));
        assert_eq!(rows[0].tags, ["users"]);
        assert!(rows[0].documented);
        assert_eq!(rows[0].source.map(|source| source.line), Some(7));
        assert_eq!(rows[1].handler, "<undocumented>");
        assert!(!rows[1].documented);
    }

    #[test]
    fn describe_reports_security_scheme_names() {
        let router = Router::new()
            .get("/users", Documented)
            .security(SecurityRequirement::scopes("oauth", ["read:users"]));
        assert_eq!(router.describe()[0].security, ["oauth"]);
    }

    #[test]
    fn timeouts_and_layers_are_reported_per_route() {
        let router = Router::new()
            .get("/slow", Documented)
            .timeout(Duration::from_secs(1))
            .get("/fast", Documented);
        assert_eq!(router.describe()[0].layers, ["timeout"]);
        assert!(router.describe()[1].layers.is_empty());
    }

    // ── the authoritative description ─────────────────────────────────────

    #[test]
    fn describe_re_runs_the_handler_and_the_metadata() {
        let router = Router::new().get("/users", Documented).tag("users");
        let mut builder =
            OperationBuilder::new(SchemaGenerator::new(crate::COMPONENTS_SCHEMAS_PREFIX));
        router.entries()[0].describe(&mut builder);
        let spec = builder.into_spec();
        assert_eq!(spec.summary.as_deref(), Some("A documented operation."));
        assert_eq!(spec.tags, ["users"]);
    }

    // ── into_axum ─────────────────────────────────────────────────────────

    /// Send one request through the composed Axum router.
    async fn oneshot(router: &axum::Router<()>, method: http::Method, path: &str) -> Response {
        use tower::ServiceExt;
        let request = http::Request::builder()
            .method(method)
            .uri(path)
            .body(axum::body::Body::empty())
            .expect("a valid request");
        router
            .clone()
            .into_service::<axum::body::Body>()
            .oneshot(request)
            .await
            .expect("the router is infallible")
    }

    #[tokio::test]
    async fn into_axum_serves_exactly_the_registered_routes() {
        let router = Router::new()
            .get("/users", Documented)
            .post("/users", Documented)
            .get("/users/{id}", Documented)
            .into_axum();

        // A matched route reaches the handler service, which reports the
        // missing application state rather than 404ing.
        for (method, path) in [
            (http::Method::GET, "/users"),
            (http::Method::POST, "/users"),
            (http::Method::GET, "/users/42"),
        ] {
            let response = oneshot(&router, method.clone(), path).await;
            assert_eq!(
                response.status(),
                http::StatusCode::INTERNAL_SERVER_ERROR,
                "{method} {path} should have matched a route"
            );
        }

        // An unregistered path falls through to the framework's 404.
        let response = oneshot(&router, http::Method::GET, "/nope").await;
        assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(crate::error::problem::PROBLEM_CONTENT_TYPE)
        );

        // A registered path with an unregistered method is a 405 that says
        // which methods do work.
        let response = oneshot(&router, http::Method::DELETE, "/users").await;
        assert_eq!(response.status(), http::StatusCode::METHOD_NOT_ALLOWED);
        let allow = response
            .headers()
            .get(http::header::ALLOW)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(allow.contains("GET"), "Allow was `{allow}`");
        assert!(allow.contains("POST"), "Allow was `{allow}`");
    }

    /// Parse a router-produced response as the problem document it claims to
    /// be. Failing to parse is the assertion: an empty body is not RFC 9457.
    async fn problem_of(response: Response) -> crate::error::Problem {
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(crate::error::problem::PROBLEM_CONTENT_TYPE)
        );
        let bytes = body_of(response).await;
        serde_json::from_slice(&bytes).expect("the router answers RFC 9457")
    }

    /// The whole response body, for the tests that assert on bytes.
    async fn body_of(response: Response) -> bytes::Bytes {
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("a complete body")
    }

    #[tokio::test]
    async fn the_fallback_404_carries_the_same_type_uri_as_error_not_found() {
        // The whole point: a detached router has no configuration to read, but
        // a `type` URI is a constant of the kind, so it can still emit the one
        // an `Error` would. A test written against the slug passes on both.
        let router = Router::new().get("/users", Documented).into_axum();
        let problem = problem_of(oneshot(&router, http::Method::GET, "/nope").await).await;

        assert_eq!(problem.status, 404);
        assert_eq!(problem.type_uri, crate::Error::not_found("post").type_uri());
        assert_eq!(problem.type_uri, "https://moso.rs/errors/not-found");
        assert_eq!(problem.title, crate::Error::not_found("post").title());
        assert_eq!(problem.detail.as_deref(), Some("no route matches /nope"));
    }

    #[tokio::test]
    async fn the_default_405_is_a_problem_document_and_keeps_its_allow_header() {
        let router = Router::new()
            .get("/users", Documented)
            .post("/users", Documented)
            .into_axum();
        let response = oneshot(&router, http::Method::DELETE, "/users").await;

        assert_eq!(response.status(), http::StatusCode::METHOD_NOT_ALLOWED);
        let allow = response
            .headers()
            .get(http::header::ALLOW)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(allow.contains("GET"), "Allow was `{allow}`");
        assert!(allow.contains("POST"), "Allow was `{allow}`");

        let problem = problem_of(response).await;
        assert_eq!(problem.status, 405);
        assert_eq!(
            problem.type_uri,
            crate::Error::method_not_allowed(&[http::Method::GET]).type_uri()
        );
        assert_eq!(
            problem.detail.as_deref(),
            Some("this path does not accept DELETE; see the `Allow` header")
        );
    }

    #[tokio::test]
    async fn a_supplied_method_not_allowed_handler_wins_and_still_gets_allow() {
        let router = Router::new()
            .get("/users", Documented)
            .method_not_allowed(Documented)
            .into_axum();
        let response = oneshot(&router, http::Method::DELETE, "/users").await;

        // Detached, the caller's handler reports the missing application — so
        // reaching a 500 rather than the framework's 405 proves it ran.
        assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response
                .headers()
                .get(http::header::ALLOW)
                .and_then(|value| value.to_str().ok()),
            Some("GET,HEAD"),
            "the `Allow` header is added either way"
        );
    }

    #[tokio::test]
    async fn into_axum_keeps_nested_paths() {
        let router = Router::new()
            .nest("/api/v1", Router::new().get("/users", Documented))
            .into_axum();
        assert_eq!(
            oneshot(&router, http::Method::GET, "/api/v1/users")
                .await
                .status(),
            http::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            oneshot(&router, http::Method::GET, "/users").await.status(),
            http::StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn conflicting_routes_do_not_panic_the_axum_build() {
        // `conflicts()` is what reports these; building must still succeed, so
        // that boot can print every problem instead of dying on the first.
        let router = Router::new()
            .get("/users", Documented)
            .get("/users", Documented);
        assert_eq!(router.conflicts().len(), 1);
        let axum_router = router.into_axum();
        assert_eq!(
            oneshot(&axum_router, http::Method::GET, "/users")
                .await
                .status(),
            http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn two_parameter_spellings_do_not_panic_the_axum_build_either() {
        // `matchit` refuses `/users/{id}` alongside `/users/{user_id}` — same
        // shape, different name — by panicking inside `Router::route`. Grouping
        // by path *as written* would reach that panic and take the whole boot
        // report with it, which is the one failure mode the report exists to
        // prevent.
        let router = Router::new()
            .get("/users/{id}", Documented)
            .get("/users/{user_id}", Documented);

        let problems = router.conflicts();
        assert_eq!(problems.len(), 1);
        assert!(matches!(
            problems[0],
            BootError::RouteConflict {
                reason: crate::error::boot::ConflictReason::ParameterNameMismatch,
                ..
            }
        ));

        // The first spelling is the one that survives, and the second is gone
        // rather than shadowing it.
        let axum_router = router.into_axum();
        assert_eq!(
            oneshot(&axum_router, http::Method::GET, "/users/7")
                .await
                .status(),
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "the route is registered; without state it answers 500, not 404"
        );
    }

    #[tokio::test]
    async fn layers_wrap_the_route_service() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static SEEN: AtomicUsize = AtomicUsize::new(0);

        #[derive(Clone)]
        struct Counting(Route);

        impl Service<Request> for Counting {
            type Response = Response;
            type Error = Infallible;
            type Future = BoxFuture<'static, Result<Response, Infallible>>;

            fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
                self.0.poll_ready(cx)
            }

            fn call(&mut self, req: Request) -> Self::Future {
                SEEN.fetch_add(1, Ordering::SeqCst);
                let ready = self.0.clone();
                let mut inner = core::mem::replace(&mut self.0, ready);
                Box::pin(async move { inner.call(req).await })
            }
        }

        struct CountingLayer;

        impl CustomLayer for CountingLayer {
            fn name(&self) -> &'static str {
                "counting"
            }

            fn apply(&self, service: Route) -> Route {
                Route::new(Counting(service))
            }
        }

        let mut router = Router::new().get("/users", Documented);
        router.push_layer(Arc::new(CountingLayer));
        assert_eq!(router.describe()[0].layers, ["counting"]);

        let axum_router = router.into_axum();
        let _ = oneshot(&axum_router, http::Method::GET, "/users").await;
        assert_eq!(SEEN.load(Ordering::SeqCst), 1);
    }

    // ── static files ──────────────────────────────────────────────────────

    #[test]
    fn traversal_attempts_are_refused() {
        assert_eq!(safe_relative_path("/app.js"), Some("app.js".to_owned()));
        assert_eq!(
            safe_relative_path("/assets//./app.js"),
            Some("assets/app.js".to_owned())
        );
        assert_eq!(safe_relative_path("/../secrets"), None);
        assert_eq!(safe_relative_path("/assets/../../etc/passwd"), None);
        assert_eq!(safe_relative_path("/%2e%2e/secrets"), None);
        assert_eq!(safe_relative_path("/a\\b"), None);
        assert_eq!(safe_relative_path("/"), Some(String::new()));
    }

    #[test]
    fn fingerprinted_names_are_cached_forever_and_others_are_not() {
        assert!(is_fingerprinted("app.4f3a9c1e.js"));
        assert!(!is_fingerprinted("index.html"));
        assert!(!is_fingerprinted("app.min.js"));
        assert_eq!(
            cache_control_for("app.4f3a9c1e.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            cache_control_for("index.html"),
            "public, max-age=0, must-revalidate"
        );
    }

    #[test]
    fn content_codings_are_parsed_with_their_q_values() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::ACCEPT_ENCODING,
            http::HeaderValue::from_static("gzip, br;q=0.8"),
        );
        assert!(accepts_encoding(&headers, "br"));
        assert!(accepts_encoding(&headers, "gzip"));
        assert!(!accepts_encoding(&headers, "zstd"));

        headers.insert(
            http::header::ACCEPT_ENCODING,
            http::HeaderValue::from_static("br;q=0"),
        );
        assert!(!accepts_encoding(&headers, "br"));
    }

    static EMBEDDED: &[EmbeddedFile] = &[
        EmbeddedFile {
            path: "index.html",
            bytes: b"<!doctype html>",
            content_type: "text/html; charset=utf-8",
            etag: "\"index\"",
        },
        EmbeddedFile {
            path: "app.4f3a9c1e.js",
            bytes: b"console.log(1)",
            content_type: "text/javascript",
            etag: "\"app\"",
        },
        EmbeddedFile {
            path: "app.4f3a9c1e.js.br",
            bytes: b"compressed",
            content_type: "text/javascript",
            etag: "\"app\"",
        },
    ];

    fn parts_for(
        path: &str,
        headers: Vec<(http::HeaderName, &'static str)>,
    ) -> http::request::Parts {
        let mut builder = http::Request::builder().uri(path);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        builder
            .body(axum::body::Body::empty())
            .expect("a valid request")
            .into_parts()
            .0
    }

    #[tokio::test]
    async fn embedded_files_are_served_with_their_etag_and_cache_policy() {
        let source = StaticSource::embedded(EMBEDDED);

        let response = serve_static(&source, &parts_for("/app.4f3a9c1e.js", Vec::new())).await;
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(
            response.headers()[http::header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(response.headers()[http::header::ETAG], "\"app\"");

        // The mount root serves the index.
        let response = serve_static(&source, &parts_for("/", Vec::new())).await;
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(
            response.headers()[http::header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );

        // A conditional request is answered 304.
        let response = serve_static(
            &source,
            &parts_for(
                "/app.4f3a9c1e.js",
                vec![(http::header::IF_NONE_MATCH, "\"app\"")],
            ),
        )
        .await;
        assert_eq!(response.status(), http::StatusCode::NOT_MODIFIED);

        // A precompressed sibling is preferred when it is acceptable.
        let response = serve_static(
            &source,
            &parts_for(
                "/app.4f3a9c1e.js",
                vec![(http::header::ACCEPT_ENCODING, "br")],
            ),
        )
        .await;
        assert_eq!(response.headers()[http::header::CONTENT_ENCODING], "br");
        assert_eq!(response.headers()[http::header::VARY], "accept-encoding");

        // Anything else is a 404 problem document.
        let response = serve_static(&source, &parts_for("/missing.js", Vec::new())).await;
        assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn embedded_mounts_fall_back_for_client_side_routes() {
        let source = StaticSource::Embedded {
            files: EMBEDDED,
            fallback: Some("index.html"),
        };
        let response = serve_static(&source, &parts_for("/settings/profile", Vec::new())).await;
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(response.headers()[http::header::ETAG], "\"index\"");
    }

    #[tokio::test]
    async fn static_mounts_answer_only_get_and_head() {
        let source = StaticSource::embedded(EMBEDDED);
        let mut parts = parts_for("/index.html", Vec::new());
        parts.method = http::Method::POST;
        let response = serve_static(&source, &parts).await;
        assert_eq!(response.status(), http::StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[http::header::ALLOW], "GET, HEAD");
    }

    #[test]
    fn spa_sources_carry_a_fallback() {
        match StaticSource::spa("dist", "index.html") {
            StaticSource::Dir { fallback, .. } => {
                assert_eq!(fallback.as_deref(), Some("index.html"))
            }
            StaticSource::Embedded { .. } => panic!("expected a directory source"),
        }
    }

    #[test]
    fn static_mounts_are_recorded_with_their_prefix() {
        let router = Router::new().static_files("/assets", StaticSource::embedded(EMBEDDED));
        assert_eq!(router.static_mounts().len(), 1);
        assert_eq!(router.static_mounts()[0].0, "/assets");
    }

    #[tokio::test]
    async fn an_embedded_mount_answers_a_real_request_through_the_composed_router() {
        // End to end rather than through `serve_static`: the mount has to be
        // routed, the prefix stripped and the response returned by the same
        // Axum router an application serves.
        let router = Router::new()
            .static_files("/assets", StaticSource::embedded(EMBEDDED))
            .into_axum();

        let response = oneshot(&router, http::Method::GET, "/assets/app.4f3a9c1e.js").await;
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(response.headers()[http::header::ETAG], "\"app\"");
        assert_eq!(body_of(response).await.as_ref(), b"console.log(1)");

        // A HEAD is the same response without the bytes.
        let response = oneshot(&router, http::Method::HEAD, "/assets/app.4f3a9c1e.js").await;
        assert_eq!(response.status(), http::StatusCode::OK);
        assert!(body_of(response).await.is_empty());

        // And a path the mount does not hold is the framework's 404 problem.
        let response = oneshot(&router, http::Method::GET, "/assets/missing.js").await;
        assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
        assert_eq!(problem_of(response).await.status, 404);
    }

    #[tokio::test]
    async fn a_directory_mount_serves_a_real_file_and_refuses_a_real_traversal() {
        // The root is this crate's `src`, so `../Cargo.toml` is a file that
        // genuinely exists one level outside it. If the traversal check were
        // wrong the request would succeed, which is what makes this worth
        // asserting rather than a synthetic path.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let router = Router::new()
            .static_files("/assets", StaticSource::dir(root))
            .into_axum();

        let response = oneshot(&router, http::Method::GET, "/assets/router.rs").await;
        assert_eq!(response.status(), http::StatusCode::OK);
        assert!(
            body_of(response).await.starts_with(b"//! The route table"),
            "the mount served this very file"
        );

        for attempt in [
            "/assets/../Cargo.toml",
            "/assets/%2e%2e/Cargo.toml",
            "/assets/../../moso-core/Cargo.toml",
        ] {
            let response = oneshot(&router, http::Method::GET, attempt).await;
            assert_eq!(
                response.status(),
                http::StatusCode::NOT_FOUND,
                "`{attempt}` escaped the mount root"
            );
            let body = body_of(response).await;
            assert!(
                !body.windows(6).any(|window| window == b"[lints"),
                "`{attempt}` leaked a manifest"
            );
        }
    }
}
