//! Handlers: the three traits, and how a plain `async fn` becomes a route.
//!
//! # Why there are three traits and not one
//!
//! Rust cannot attach an associated type to a `fn` item. `#[endpoint]`
//! therefore leaves the `async fn` exactly as written and emits a **companion
//! unit struct** beside it:
//!
//! ```text
//! #[endpoint]
//! async fn list(Inject(db): Inject<Db>) -> Result<Page<UserOut>> { /* … */ }
//!
//! // also generated:
//! #[doc(hidden)] #[derive(Clone, Copy)] pub struct __moso_op_list;
//! impl Endpoint  for __moso_op_list { /* summary, parameters, responses, … */ }
//! impl HandlerFn for __moso_op_list { /* the extraction glue, one concrete fn */ }
//! ```
//!
//! Both halves are reachable from user code — the `async fn` by its own name,
//! the description through `moso::ep!`:
//!
//! ```
//! use moso::prelude::*;
//! use moso::Endpoint;
//! # /// A database handle.
//! # #[derive(Default)] pub struct Db;
//! # /// A user, as the API returns one.
//! # #[derive(Schema)] pub struct UserOut { /// Stable identifier.
//! #     pub id: u64 }
//! /// List users.
//! #[endpoint]
//! async fn list(Inject(db): Inject<Db>) -> Result<Page<UserOut>> {
//!     let _ = db;
//!     Ok(Page::empty())
//! }
//! # fn main() {
//! assert_eq!(<__moso_op_list as Endpoint>::NAME, "list");
//! assert_eq!(Router::new().get("/users", moso::ep!(list)).len(), 1);
//! # }
//! ```
//!
//! - [`Endpoint`] is the *description*: what the operation looks like in
//!   OpenAPI and which providers it needs.
//! - [`HandlerFn`] is the *glue*: one non-generic `async fn` that runs the
//!   extractors in order and calls the user's function. It compiles once, so a
//!   handler's generic surface does not grow with the number of routes.
//! - [`Handler<M>`] is what [`Router::get`](crate::Router::get) accepts. It
//!   carries `type Endpoint`, which is how the builder chain picks up the
//!   metadata that `#[endpoint]` produced.
//!
//! # The two families of `Handler` impls
//!
//! | Written | `M` | `Handler::Endpoint` | Documents itself |
//! | --- | --- | --- | --- |
//! | `Router::get("/u", ep!(list))` | [`EndpointMarker`] | the generated `__moso_op_list` | fully |
//! | `moso::routes! { GET "/u" => list }` | [`EndpointMarker`] | the generated `__moso_op_list` | fully |
//! | `Router::get("/u", list)` where `list` is a plain `async fn` | `(PartsOnly, …)` | [`UndocumentedEndpoint`] | not at all |
//!
//! The plain-`async fn` impls exist so that a handler *without* `#[endpoint]`
//! still compiles and still serves. What it loses is the OpenAPI operation and
//! the boot-time provider check — an honest trade, stated in
//! [`UndocumentedEndpoint`]'s own documentation and surfaced by
//! `moso routes` as `<undocumented>`.
//!
//! Both families are `#[diagnostic::do_not_recommend]`, so a handler that fails
//! to compile gets the hand-written message rather than a list of eighteen
//! blanket impls.
//!
//! # Arity
//!
//! Impls exist for 0 to 16 parameters. Beyond that the diagnostic says to group
//! related parameters into a struct deriving `Dependency` or `Schema`, which is
//! better design anyway.

use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use moso_openapi::OperationBuilder;

use crate::ctx::RequestCtx;
use crate::di::ProviderReq;
use crate::extract::{Extract, ExtractBody};
use crate::response::IntoResponse;
use crate::{BoxFuture, Request, Response};

/// The largest number of parameters a handler may have.
///
/// Named so the diagnostic and the `#[endpoint]` macro cannot disagree about it.
pub const MAX_HANDLER_PARAMS: usize = 16;

/// The `x-*` member marking an operation that carries no description.
///
/// Written by [`UndocumentedEndpoint::spec`] and read by `moso routes`, which
/// prints such a route as `<undocumented>` and by `moso check`, which suggests
/// adding `#[endpoint]`. It is a specification extension rather than a side
/// table so that it survives into an exported `openapi.json` and can be
/// grepped for in review.
pub const UNDOCUMENTED_EXTENSION: &str = "x-moso-undocumented";

// ---------------------------------------------------------------------------
// Endpoint
// ---------------------------------------------------------------------------

/// The compile-time description of one operation.
///
/// Implemented by the unit struct `#[endpoint]` generates. Everything the
/// OpenAPI document knows about a route comes from here, and everything the
/// boot-time DI check knows comes from [`Endpoint::required_providers`].
///
/// You never write an impl: `#[endpoint]` writes one, and `moso::ep!` is how
/// you name the type it wrote.
///
/// ```
/// use moso::prelude::*;
/// use moso::{Endpoint, ProviderReq};
/// use moso::openapi::OperationBuilder;
/// use moso::schema::SchemaGenerator;
/// # /// A database handle.
/// # #[derive(Default)] pub struct Db;
/// # /// A user, as the API returns one.
/// # #[derive(Schema)] pub struct UserOut { /// Stable identifier.
/// #     pub id: u64 }
/// /// List users.
/// ///
/// /// Newest first.
/// #[endpoint]
/// async fn list(Inject(db): Inject<Db>) -> Result<Json<Vec<UserOut>>> {
///     let _ = db;
///     Ok(Json(vec![]))
/// }
///
/// # fn main() {
/// assert_eq!(<__moso_op_list as Endpoint>::NAME, "list");
///
/// // The provider check `App::build()` runs sees straight through the handler.
/// let required = <__moso_op_list as Endpoint>::required_providers();
/// assert_eq!(required, [ProviderReq::of::<Db>()]);
///
/// // And the document comes out of the same signature.
/// let mut op = OperationBuilder::new(SchemaGenerator::default());
/// <__moso_op_list as Endpoint>::spec(&mut op);
/// let (spec, _) = op.finish();
/// assert_eq!(spec.summary.as_deref(), Some("List users."));
/// assert_eq!(spec.description.as_deref(), Some("Newest first."));
/// # }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not describe an endpoint",
    label = "not an endpoint",
    note = "`Endpoint` is implemented by the type `#[endpoint]` generates beside your handler",
    note = "help: add `#[endpoint]` above the `async fn`, then register it with \
            `moso::routes!` or `Router::get(\"/path\", ep!(handler))`",
    note = "a plain `async fn` can be registered without `#[endpoint]`, but it contributes \
            nothing to the OpenAPI document and is skipped by the boot-time provider check"
)]
pub trait Endpoint: 'static {
    /// The handler's function name, used by `moso routes` and by the boot
    /// report. Not the `operationId`, which is derived from the module path.
    const NAME: &'static str;

    /// Describe the operation: summary, description, `operationId`, source
    /// location, then one `describe` call per parameter, then the response
    /// type's.
    ///
    /// Called once per route at `App::build()`, never per request.
    fn spec(b: &mut OperationBuilder);

    /// Every provider this operation's parameters need, transitively.
    ///
    /// The union of each parameter type's `PROVIDER_REQ`, concatenated by
    /// [`concat_reqs!`](crate::concat_reqs).
    fn required_providers() -> &'static [ProviderReq];
}

/// The endpoint description of a handler registered without `#[endpoint]`.
///
/// It contributes no summary, no parameters, no responses and no provider
/// requirements, and marks the operation so `moso routes` can report it. This
/// is the honest behaviour: a plain `async fn` genuinely carries no metadata,
/// and inventing some would be worse than admitting it.
///
/// To document such a route, add `#[endpoint]` to the function.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UndocumentedEndpoint;

impl Endpoint for UndocumentedEndpoint {
    const NAME: &'static str = "<undocumented>";

    fn spec(b: &mut OperationBuilder) {
        b.extension(UNDOCUMENTED_EXTENSION, serde_json::Value::Bool(true));
    }

    fn required_providers() -> &'static [ProviderReq] {
        &[]
    }
}

// ---------------------------------------------------------------------------
// HandlerFn
// ---------------------------------------------------------------------------

/// The extraction glue: one concrete, non-generic future per handler.
///
/// `#[endpoint]` generates the implementation. It splits the request, runs each
/// non-body extractor against the parts in declaration order, runs the single
/// body extractor (if any) against the reassembled request, calls the user's
/// function, and converts the result with [`IntoResponse`]. An extractor that
/// fails short-circuits: its [`Error`](crate::Error) becomes the response and
/// no later extractor runs.
///
/// Being non-generic is the point. However many routes an application has, this
/// code is monomorphised once per handler and never per call site.
///
/// ```
/// use moso::prelude::*;
/// use moso::deps::tower::ServiceExt;
/// use moso::response::NoContent;
/// use moso::{HandlerFn, Request};
/// # /// Everything this application reads from its environment.
/// # #[derive(Config, Clone, Debug)] pub struct AppConfig {
/// #     /// Service name.
/// #     #[config(default = "probe")] pub name: String }
/// /// Answer with nothing at all.
/// #[endpoint]
/// async fn ping() -> Result<NoContent> { Ok(NoContent) }
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> Result<()> {
/// // Naming the bound proves `#[endpoint]` wrote the impl …
/// fn is_handler_fn<T: HandlerFn>() {}
/// is_handler_fn::<__moso_op_ping>();
///
/// // … and the router is what actually calls it.
/// let service = App::new(AppConfig { name: "probe".to_owned() })
///     .mount(moso::routes! { GET "/ping" => ping })
///     .build()?
///     .into_service();
///
/// let request = Request::builder()
///     .uri("/ping")
///     .body(axum::body::Body::empty())
///     .unwrap();
/// let response = service.oneshot(request).await.unwrap();
///
/// assert_eq!(response.status(), 204);
/// # Ok(())
/// # }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no handler body",
    label = "no `HandlerFn` implementation",
    note = "`HandlerFn` is generated by `#[endpoint]` alongside `Endpoint`; the two always \
            appear together",
    note = "help: if you wrote this type by hand, implement `HandlerFn` for it, or register \
            the underlying `async fn` directly"
)]
pub trait HandlerFn: Send + Sync + 'static {
    /// Run the handler for one request.
    fn invoke(req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response>;
}

/// Box the future a generated `HandlerFn::invoke` produces.
///
/// `Box::pin(async move { … })` compiles to exactly the same thing. It also
/// produces, when the future is not `Send`, the single worst error shape in
/// async Rust — a coercion failure that ends in
///
/// ```text
/// = note: required for the cast from `Pin<Box<{async block@src/routes/users.rs:14:1: 14:12}>>`
///         to `Pin<Box<dyn Future<Output = Response> + Send>>`
/// ```
///
/// which is 190 characters of types the reader did not write, appended to an
/// error whose caret is on the `#[endpoint]` attribute. Requiring the bound
/// through a named trait instead removes that note entirely and lets
/// `#[endpoint]` put the caret on the handler's own name.
///
/// # What rustc keeps
///
/// The blanket impl deliberately carries **no**
/// `#[diagnostic::do_not_recommend]`. For an auto trait on a coroutine rustc
/// reports the nested obligation, so the message a user sees for a `!Send`
/// future is rustc's own — "future is not `Send` as this value is used across an
/// await", with the binding, its type and the offending `.await` all
/// underlined. That is more useful than anything an attribute can say, and
/// `do_not_recommend` would suppress it in favour of a generic line naming an
/// unnameable async-block type. Measured on 1.97.1; `tests/ui/handler/
/// non_send_future.rs` is the snapshot that will notice if it changes, and
/// `xtask/allow/diagnostics.toml` records the exemption so that
/// `xtask check-diagnostics` reads this as a decision rather than an omission.
///
/// The message below therefore covers the *other* ways this bound fails: a
/// handler future that borrows from the request (not `'static`), or one whose
/// output is not a `Response`.
#[diagnostic::on_unimplemented(
    message = "this handler's future cannot be stored by a route",
    label = "not a storable handler future",
    note = "a route stores one boxed future per request, so a handler's future must be `Send`, \
            `'static`, and produce a `Response`",
    note = "help: replace `Rc<T>` with `Arc<T>`, and `RefCell<T>` with `Mutex<T>`",
    note = "help: do not hold a borrow of the request across an `.await` — clone or copy the \
            value out first",
    note = "help: for a `MutexGuard`, clone what you need and drop the guard before awaiting"
)]
pub trait HandlerFuture: Sized {
    /// Erase the future's type so a route can store one shape.
    fn box_handler_future(self) -> BoxFuture<'static, Response>;
}

impl<F: Future<Output = Response> + Send + 'static> HandlerFuture for F {
    fn box_handler_future(self) -> BoxFuture<'static, Response> {
        Box::pin(self)
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Anything [`Router::get`](crate::Router::get) and friends accept.
///
/// `M` is the usual marker type parameter; it exists so that several blanket
/// impls can coexist without overlapping, and is always inferred. You never
/// name it.
///
/// Three families implement it, and they do not overlap: the `__moso_op_*` type
/// `#[endpoint]` generates (which carries the full OpenAPI description), a plain
/// `async fn` whose parameters are all [`Extract`], and a plain `async fn`
/// whose last parameter is [`ExtractBody`].
///
/// ```
/// use moso::prelude::*;
/// use moso::extract::RequestId;
/// use moso::response::NoContent;
///
/// /// Liveness, documented.
/// #[endpoint]
/// async fn healthz() -> Result<NoContent> { Ok(NoContent) }
///
/// /// Liveness, undocumented — a plain `async fn` is a handler too.
/// async fn ping(_id: RequestId) -> Result<NoContent> { Ok(NoContent) }
///
/// # fn main() {
/// let router = Router::new()
///     .get("/healthz", moso::ep!(healthz))   // full OpenAPI metadata
///     .get("/ping", ping);                   // registered, but undocumented
///
/// assert_eq!(router.len(), 2);
/// assert_eq!(router.entries()[0].spec.summary.as_deref(), Some("Liveness, documented."));
/// assert!(router.entries()[1].spec.summary.is_none());
/// # }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a valid Moso handler",
    label = "not a handler",
    note = "a handler is an `async fn` whose parameters all implement `Extract`, with at most \
            one `ExtractBody` parameter, which must be last",
    note = "its return type must implement `IntoResponse` — usually `Result<T>` where `T` is a \
            response type such as `Json<T>`, `Created<T>`, `Page<T>` or `NoContent`",
    note = "if a parameter is your own type, add `#[derive(moso::Dependency)]` to it and take \
            it as `Depends<YourType>`",
    note = "if the return type is your own type, add `#[derive(moso::Schema)]` for a 200 JSON \
            body, or `#[derive(moso::Responder)]` to control the status and headers",
    note = "handlers take at most 16 parameters; group related ones into a struct",
    note = "run `moso check` for a diagnosis of this specific handler"
)]
pub trait Handler<M>: Clone + Send + Sync + 'static {
    /// How this handler describes itself to the OpenAPI document.
    ///
    /// [`UndocumentedEndpoint`] for a plain `async fn`; the generated
    /// `__moso_op_*` type for anything registered through `#[endpoint]`.
    type Endpoint: Endpoint;

    /// Serve one request.
    fn call(self, req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response>;
}

/// Marker for the `T: Endpoint + HandlerFn` family — the documented handlers.
#[derive(Debug, Clone, Copy)]
pub struct EndpointMarker;

/// Marker for plain `async fn` handlers whose parameters are all [`Extract`].
#[derive(Debug, Clone, Copy)]
pub struct PartsOnly;

/// Marker for plain `async fn` handlers whose last parameter is [`ExtractBody`].
#[derive(Debug, Clone, Copy)]
pub struct WithBody;

#[diagnostic::do_not_recommend]
impl<E> Handler<EndpointMarker> for E
where
    E: Endpoint + HandlerFn + Clone,
{
    type Endpoint = E;

    fn call(self, req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response> {
        <E as HandlerFn>::invoke(req, ctx)
    }
}

macro_rules! impl_handler_parts {
    ( $($ty:ident),* ) => {
        #[diagnostic::do_not_recommend]
        #[allow(
            non_snake_case,
            unused_variables,
            unused_mut,
            reason = "the macro reuses each type parameter's name as its binding, which is \
                      what keeps the expansion readable; the zero-parameter arm leaves \
                      `parts` unused"
        )]
        impl<F, Fut, Res, $($ty,)*> Handler<(PartsOnly, $($ty,)*)> for F
        where
            F: FnOnce($($ty,)*) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = Res> + Send + 'static,
            Res: IntoResponse,
            $( $ty: Extract + 'static, )*
        {
            type Endpoint = UndocumentedEndpoint;

            fn call(self, req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response> {
                Box::pin(async move {
                    let (mut parts, _body) = req.into_parts();
                    $(
                        let $ty = match <$ty as Extract>::extract(&mut parts, &ctx).await {
                            Ok(value) => value,
                            Err(error) => return error.into_response(),
                        };
                    )*
                    self($($ty,)*).await.into_response()
                })
            }
        }
    };
}

macro_rules! impl_handler_with_body {
    ( $($ty:ident),* ) => {
        #[diagnostic::do_not_recommend]
        #[allow(
            non_snake_case,
            unused_variables,
            unused_mut,
            reason = "as `impl_handler_parts`: the macro reuses each type parameter's name \
                      as its binding, and the body-only arm leaves `parts` unused"
        )]
        impl<F, Fut, Res, $($ty,)* TB> Handler<(WithBody, $($ty,)* TB)> for F
        where
            F: FnOnce($($ty,)* TB) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = Res> + Send + 'static,
            Res: IntoResponse,
            $( $ty: Extract + 'static, )*
            TB: ExtractBody + 'static,
        {
            type Endpoint = UndocumentedEndpoint;

            fn call(self, req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response> {
                Box::pin(async move {
                    let (mut parts, body) = req.into_parts();
                    $(
                        let $ty = match <$ty as Extract>::extract(&mut parts, &ctx).await {
                            Ok(value) => value,
                            Err(error) => return error.into_response(),
                        };
                    )*
                    let request = Request::from_parts(parts, body);
                    let TB = match <TB as ExtractBody>::extract_body(request, &ctx).await {
                        Ok(value) => value,
                        Err(error) => return error.into_response(),
                    };
                    self($($ty,)* TB).await.into_response()
                })
            }
        }
    };
}

impl_handler_parts!();
impl_handler_parts!(T1);
impl_handler_parts!(T1, T2);
impl_handler_parts!(T1, T2, T3);
impl_handler_parts!(T1, T2, T3, T4);
impl_handler_parts!(T1, T2, T3, T4, T5);
impl_handler_parts!(T1, T2, T3, T4, T5, T6);
impl_handler_parts!(T1, T2, T3, T4, T5, T6, T7);
impl_handler_parts!(T1, T2, T3, T4, T5, T6, T7, T8);
impl_handler_parts!(T1, T2, T3, T4, T5, T6, T7, T8, T9);
impl_handler_parts!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10);
impl_handler_parts!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11);
impl_handler_parts!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12);
impl_handler_parts!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13);
impl_handler_parts!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14);
impl_handler_parts!(
    T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15
);
impl_handler_parts!(
    T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16
);

impl_handler_with_body!();
impl_handler_with_body!(T1);
impl_handler_with_body!(T1, T2);
impl_handler_with_body!(T1, T2, T3);
impl_handler_with_body!(T1, T2, T3, T4);
impl_handler_with_body!(T1, T2, T3, T4, T5);
impl_handler_with_body!(T1, T2, T3, T4, T5, T6);
impl_handler_with_body!(T1, T2, T3, T4, T5, T6, T7);
impl_handler_with_body!(T1, T2, T3, T4, T5, T6, T7, T8);
impl_handler_with_body!(T1, T2, T3, T4, T5, T6, T7, T8, T9);
impl_handler_with_body!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10);
impl_handler_with_body!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11);
impl_handler_with_body!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12);
impl_handler_with_body!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13);
impl_handler_with_body!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14);
impl_handler_with_body!(
    T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15
);

// ---------------------------------------------------------------------------
// Erasure
// ---------------------------------------------------------------------------

/// A handler with its marker type erased, as the route table stores it.
///
/// Rule A2 of the compile-time architecture: erase early. A handler becomes one
/// of these at registration, so the generic surface of route composition does
/// not grow with the size of the application.
///
/// Never implemented by hand: a blanket impl covers every [`Handler`], and
/// [`boxed`] is what performs the erasure.
///
/// ```
/// use moso::prelude::*;
/// use moso::handler::boxed;
/// use moso::response::NoContent;
///
/// /// Liveness.
/// #[endpoint]
/// async fn healthz() -> Result<NoContent> { Ok(NoContent) }
///
/// # fn main() {
/// let erased = boxed(moso::ep!(healthz));
///
/// // The description survives erasure — which is why the document still works.
/// assert_eq!(erased.name(), "healthz");
/// assert!(erased.required_providers().is_empty());
/// # }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be stored in the route table",
    note = "this is an internal trait; implement `Handler` instead"
)]
pub trait ErasedHandler: Send + Sync + 'static {
    /// Serve one request.
    fn call_erased(&self, req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response>;

    /// Describe the operation. Forwards to `Handler::Endpoint::spec`.
    fn describe(&self, b: &mut OperationBuilder);

    /// The providers this handler needs.
    fn required_providers(&self) -> &'static [ProviderReq];

    /// The handler's name, for `moso routes` and the boot report.
    fn name(&self) -> &'static str;
}

/// Bridges a `Handler<M>` to [`ErasedHandler`] by remembering `M`.
///
/// The `PhantomData` is over `fn() -> M` so the adapter is `Send + Sync`
/// whatever `M` is — markers are uninhabited-ish tag types and carry no data.
pub struct HandlerAdapter<H, M> {
    handler: H,
    marker: PhantomData<fn() -> M>,
}

impl<H, M> HandlerAdapter<H, M>
where
    H: Handler<M>,
    M: 'static,
{
    /// Wrap a handler.
    pub fn new(handler: H) -> Self {
        Self {
            handler,
            marker: PhantomData,
        }
    }
}

impl<H: Clone, M> Clone for HandlerAdapter<H, M> {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
            marker: PhantomData,
        }
    }
}

impl<H, M> ErasedHandler for HandlerAdapter<H, M>
where
    H: Handler<M>,
    M: 'static,
{
    fn call_erased(&self, req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response> {
        self.handler.clone().call(req, ctx)
    }

    fn describe(&self, b: &mut OperationBuilder) {
        <H::Endpoint as Endpoint>::spec(b);
    }

    fn required_providers(&self) -> &'static [ProviderReq] {
        <H::Endpoint as Endpoint>::required_providers()
    }

    fn name(&self) -> &'static str {
        <H::Endpoint as Endpoint>::NAME
    }
}

/// A shared, type-erased handler. What a [`RouteEntry`](crate::RouteEntry) holds.
pub type BoxedHandler = Arc<dyn ErasedHandler>;

/// Erase a handler for storage in the route table.
pub fn boxed<H, M>(handler: H) -> BoxedHandler
where
    H: Handler<M>,
    M: 'static,
{
    Arc::new(HandlerAdapter::new(handler))
}

// ---------------------------------------------------------------------------
// concat_reqs!
// ---------------------------------------------------------------------------

/// Concatenate `const` slices of [`ProviderReq`] into one `const` slice.
///
/// Used by `#[endpoint]` to build [`Endpoint::required_providers`] from the
/// parameters' `PROVIDER_REQ` constants. Everything happens in `const`
/// evaluation, so a handler's provider requirements cost nothing at runtime and
/// stay available to a future ahead-of-time analysis.
///
/// ```
/// # use moso_core::di::ProviderReq;
/// const A: &[ProviderReq] = &[ProviderReq::of::<String>()];
/// const B: &[ProviderReq] = &[ProviderReq::of::<u32>()];
/// const BOTH: &[ProviderReq] = moso_core::concat_reqs!(A, B);
/// assert_eq!(BOTH.len(), 2);
/// ```
#[macro_export]
macro_rules! concat_reqs {
    () => { &[] as &'static [$crate::di::ProviderReq] };
    ( $($slice:expr),+ $(,)? ) => {{
        const __MOSO_LEN: usize = 0 $( + $slice.len() )+;
        const __MOSO_REQS: [$crate::di::ProviderReq; __MOSO_LEN] = {
            let mut out = [$crate::di::ProviderReq::of::<()>(); __MOSO_LEN];
            let mut at = 0usize;
            $({
                let source = $slice;
                let mut i = 0usize;
                while i < source.len() {
                    out[at] = source[i];
                    at += 1;
                    i += 1;
                }
            })+
            out
        };
        &__MOSO_REQS as &'static [$crate::di::ProviderReq]
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{RequestId, Text};
    use crate::response::NoContent;

    fn assert_handler<H, M>(_handler: H)
    where
        H: Handler<M>,
        M: 'static,
    {
    }

    async fn zero_arity() -> NoContent {
        NoContent
    }

    async fn parts_only(_id: RequestId) -> NoContent {
        NoContent
    }

    async fn parts_and_body(_id: RequestId, _body: Text) -> NoContent {
        NoContent
    }

    async fn body_only(_body: Text) -> crate::Result<NoContent> {
        Ok(NoContent)
    }

    #[test]
    fn plain_async_fns_resolve_to_a_handler_impl() {
        assert_handler(zero_arity);
        assert_handler(parts_only);
        assert_handler(parts_and_body);
        assert_handler(body_only);
    }

    #[derive(Clone, Copy, Default)]
    struct FakeOp;

    impl Endpoint for FakeOp {
        const NAME: &'static str = "fake";

        fn spec(b: &mut OperationBuilder) {
            let _ = b;
        }

        fn required_providers() -> &'static [ProviderReq] {
            &[]
        }
    }

    impl HandlerFn for FakeOp {
        fn invoke(req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response> {
            let _ = (req, ctx);
            Box::pin(async move { Response::new(axum::body::Body::empty()) })
        }
    }

    #[test]
    fn generated_endpoint_types_resolve_and_carry_their_metadata() {
        assert_handler(FakeOp);
        fn endpoint_of<H: Handler<M>, M>(_h: H) -> &'static str {
            <H::Endpoint as Endpoint>::NAME
        }
        assert_eq!(endpoint_of(FakeOp), "fake");
        assert_eq!(endpoint_of(zero_arity as fn() -> _), "<undocumented>");
    }

    #[test]
    fn handlers_erase_into_the_route_table() {
        let erased = boxed(FakeOp);
        assert_eq!(erased.name(), "fake");
        assert!(erased.required_providers().is_empty());
    }

    #[test]
    fn undocumented_endpoint_needs_no_providers() {
        assert!(UndocumentedEndpoint::required_providers().is_empty());
        assert_eq!(UndocumentedEndpoint::NAME, "<undocumented>");
    }

    #[test]
    fn undocumented_endpoints_mark_themselves() {
        let mut op = OperationBuilder::new(moso_openapi::SchemaGenerator::default());
        UndocumentedEndpoint::spec(&mut op);
        let spec = op.into_spec();
        assert_eq!(
            spec.extensions.get(UNDOCUMENTED_EXTENSION),
            Some(&serde_json::Value::Bool(true))
        );
        // Nothing else is invented: no summary, no parameters, no responses.
        assert!(spec.summary.is_none());
        assert!(spec.parameters.is_empty());
        assert!(spec.responses.is_empty());
    }

    #[allow(clippy::too_many_arguments, reason = "the point of the test")]
    async fn sixteen(
        _p1: RequestId,
        _p2: RequestId,
        _p3: RequestId,
        _p4: RequestId,
        _p5: RequestId,
        _p6: RequestId,
        _p7: RequestId,
        _p8: RequestId,
        _p9: RequestId,
        _p10: RequestId,
        _p11: RequestId,
        _p12: RequestId,
        _p13: RequestId,
        _p14: RequestId,
        _p15: RequestId,
        _p16: RequestId,
    ) -> NoContent {
        NoContent
    }

    #[allow(clippy::too_many_arguments, reason = "the point of the test")]
    async fn fifteen_and_a_body(
        _p1: RequestId,
        _p2: RequestId,
        _p3: RequestId,
        _p4: RequestId,
        _p5: RequestId,
        _p6: RequestId,
        _p7: RequestId,
        _p8: RequestId,
        _p9: RequestId,
        _p10: RequestId,
        _p11: RequestId,
        _p12: RequestId,
        _p13: RequestId,
        _p14: RequestId,
        _p15: RequestId,
        _body: Text,
    ) -> NoContent {
        NoContent
    }

    #[test]
    fn the_arity_limit_is_reached_but_not_exceeded() {
        assert_eq!(MAX_HANDLER_PARAMS, 16);
        assert_handler(sixteen);
        assert_handler(fifteen_and_a_body);
    }

    #[test]
    fn concat_reqs_joins_const_slices() {
        const A: &[ProviderReq] = &[ProviderReq::of::<String>()];
        const B: &[ProviderReq] = &[ProviderReq::of::<u32>(), ProviderReq::of::<u8>()];
        const ALL: &[ProviderReq] = concat_reqs!(A, B);
        assert_eq!(ALL.len(), 3);
        assert_eq!(ALL[0].id(), core::any::TypeId::of::<String>());
        assert_eq!(ALL[2].id(), core::any::TypeId::of::<u8>());
    }

    #[test]
    fn concat_reqs_handles_the_empty_case() {
        const NONE: &[ProviderReq] = concat_reqs!();
        assert!(NONE.is_empty());
    }
}
