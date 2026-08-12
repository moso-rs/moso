#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "Procedural macros for Moso."]
//!
//! You probably want [`moso`] — nothing here is imported directly, and every
//! example below is written the way a user writes it, through the facade. The
//! expansion of each macro is printed in full in
//! `docs/06-reference/62-macro-reference.md`.
//!
//! Nothing in this crate is imported directly. Every macro is re-exported from
//! the [`moso`] facade, and every macro *expands* to paths under
//! `::moso::__private::*` — never to `::moso_core::…`, never to
//! `::moso_schema::…`. That indirection is what lets the runtime crates be
//! split, renamed or refactored without changing a single byte of generated
//! code, and it is why this crate depends on `syn`, `quote`, `proc-macro2`,
//! `darling` and `heck` and on no Moso crate at all.
//!
//! # No magic that cannot be printed
//!
//! Every expansion is written out in `docs/06-reference/62-macro-reference.md`
//! and is reproducible with:
//!
//! ```text
//! cargo expand --package blog routes::posts
//! moso check --expand src/routes/posts.rs
//! ```
//!
//! Three rules hold across all of them:
//!
//! 1. Generated identifiers are prefixed `__moso_` and carry `#[doc(hidden)]`,
//!    except for the types a user is expected to name — `TenantLayer`,
//!    `__moso_op_create`'s public alias, and so on.
//! 2. An unknown attribute key is a compile error with a Levenshtein
//!    suggestion, never a silent no-op.
//! 3. One user mistake produces exactly **one** error, plus a well-typed
//!    placeholder so the rest of the module still resolves.
//!
//! # The macros
//!
//! ## `#[endpoint]` — an operation, documented by construction
//!
//! ```
//! use moso::prelude::*;
//! # /// A database handle.
//! # #[derive(Default)] pub struct Db;
//! # /// Who the request acts as.
//! # #[derive(Clone)] pub struct CurrentUser { pub id: u64 }
//! # impl Dependency for CurrentUser {
//! #     const PROVIDER_REQ: &'static [moso::ProviderReq] = &[];
//! #     async fn resolve(_: &RequestCtx) -> Result<Self> { Ok(CurrentUser { id: 1 }) }
//! # }
//! /// A user, as the API accepts one.
//! #[derive(Schema)]
//! pub struct CreateUser {
//!     /// Public handle.
//!     pub username: String,
//! }
//!
//! /// A user, as the API returns one.
//! #[derive(Schema)]
//! pub struct UserOut {
//!     /// Stable identifier.
//!     pub id: u64,
//!     /// Public handle.
//!     pub username: String,
//! }
//!
//! /// Create a user.
//! ///
//! /// Sends a welcome email asynchronously.
//! #[endpoint]
//! async fn create(
//!     Inject(db): Inject<Db>,
//!     Depends(actor): Depends<CurrentUser>,
//!     Json(body): Json<CreateUser>,
//! ) -> Result<Created<UserOut>> {
//!     let _ = db;
//!     let user = UserOut { id: actor.id, username: body.username };
//!     Ok(Created::at(format!("/users/{}", user.id), user))
//! }
//! # fn main() {
//! #     assert_eq!(<__moso_op_create as moso::Endpoint>::NAME, "create");
//! # }
//! ```
//!
//! The first doc line becomes the operation's `summary`, the rest its
//! `description`, and the parameter and return types become the request body
//! schema and the `201` response schema.
//!
//! Emits an `Endpoint` impl carrying the summary, description, operation id and
//! source location; a `HandlerFn` impl that extracts each parameter in order;
//! and the assertion block that puts a bound failure's span on the user's
//! parameter rather than on generated tokens.
//!
//! Arguments: `operation_id`, `tag`, `hidden`, `deprecated`,
//! `response(status, description)`, `example(request = …, response = …)`,
//! `errors = Type`.
//!
//! ## `routes!` — tabular registration
//!
//! ```
//! use moso::prelude::*;
//! # /// A user.
//! # #[derive(Schema)] pub struct UserOut { /// id
//! #     pub id: u64 }
//! # /// List users.
//! # #[endpoint] async fn list() -> Result<Json<Vec<UserOut>>> { Ok(Json(vec![])) }
//! # /// Create a user.
//! # #[endpoint] async fn create() -> Result<Created<UserOut>> {
//! #     Ok(Created::at("/users/1", UserOut { id: 1 })) }
//! # /// Show a user.
//! # #[endpoint] async fn show(Path(id): Path<u64>) -> Result<Json<UserOut>> {
//! #     Ok(Json(UserOut { id })) }
//! /// Everything this module serves.
//! pub fn router() -> Router {
//!     moso::routes! {
//!         GET    "/users"      => list,
//!         POST   "/users"      => create,
//!         GET    "/users/{id}" => show,
//!     }
//!     .tag("users")
//! }
//! # fn main() { assert_eq!(router().len(), 3); }
//! ```
//!
//! ## `ep!` — one route, where a table would be noise
//!
//! ```
//! use moso::prelude::*;
//! # /// Liveness.
//! # #[endpoint] async fn healthz() -> Result<NoContent> { Ok(NoContent) }
//! # fn main() {
//! let router = Router::new().get("/healthz", moso::ep!(healthz));
//! assert_eq!(router.len(), 1);
//! # }
//! ```
//!
//! ## `#[middleware]` — the function-shaped Tower layer
//!
//! ```
//! use moso::prelude::*;
//! use moso::deps::http::header::HOST;
//! use moso::middleware::Next;
//! use moso::{Request, Response};
//! # /// One customer's slice of the system.
//! # #[derive(Clone)] pub struct Tenant(String);
//! # impl Tenant {
//! #     fn from_host(host: &str) -> Option<Self> {
//! #         host.split('.').next().map(|s| Tenant(s.to_owned()))
//! #     }
//! # }
//! /// Resolve the tenant from the `Host` header.
//! #[moso::middleware]
//! async fn tenant(mut req: Request, next: Next) -> Result<Response> {
//!     let host = req.headers().get(HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
//!     let tenant = Tenant::from_host(host).ok_or_else(|| Error::not_found("tenant"))?;
//!     req.extensions_mut().insert(tenant);
//!     Ok(next.run(req).await)
//! }
//! # fn main() {
//! // …then, anywhere the stack is configured:
//! let router = Router::new().layer(TenantLayer::new());
//! assert_eq!(TenantLayer::NAME, "tenant");
//! # let _ = router;
//! # }
//! ```
//!
//! Generates a `TenantLayer` / `TenantService<S>` pair. Returning `Err`
//! short-circuits with a problem document. Parameters before `req` are
//! extracted first, so `Inject<Db>` works and its `PROVIDER_REQ` participates
//! in boot validation. `Depends<T>` is a compile error: middleware runs before
//! extraction, so request dependencies do not exist yet.
//!
//! ## `#[derive(Schema)]` — serde, validation and JSON Schema from one source
//!
//! ```
//! use moso::prelude::*;
//!
//! /// A user, as the API accepts one.
//! #[derive(Schema)]
//! pub struct CreateUser {
//!     /// Public handle.
//!     #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
//!     pub username: String,
//!     /// Contact address.
//!     pub email: Email,
//!     /// Optional age, in years.
//!     #[schema(range = 13..=130)]
//!     pub age: Option<u8>,
//! }
//! # fn main() {
//! // The constraint is enforced …
//! let bad = serde_json::from_str::<CreateUser>(
//!     r#"{"username":"ab","email":"a@b.example","age":null}"#,
//! ).unwrap();
//! let ctx = &mut moso::schema::ValidationCtx::new();
//! assert!(moso::schema::Validate::validate(&bad, ctx).is_err());
//!
//! // … and documented, from the same attribute.
//! let mut generator = moso::schema::SchemaGenerator::default();
//! let node = CreateUser::json_schema(&mut generator);
//! let username = &node.properties["username"];
//! assert_eq!(username.min_length, Some(3));
//! assert_eq!(username.max_length, Some(32));
//! # }
//! ```
//!
//! The runtime check and the documented constraint come from the same
//! attribute, so they cannot disagree.
//!
//! ## `#[derive(Constrained)]` — a validated newtype
//!
//! ```
//! use moso::prelude::*;
//!
//! /// An order number, which cannot exist in an invalid state.
//! #[derive(Constrained, Debug)]
//! #[constrained(inner = String, pattern = r"^ORD-\d{8}$", format = "order-number")]
//! pub struct OrderNumber(String);
//!
//! # fn main() {
//! assert!(OrderNumber::new("ORD-00000042".to_owned()).is_ok());
//! assert!(OrderNumber::new("nope".to_owned()).is_err());
//! # }
//! ```
//!
//! ## `#[derive(Responder)]` — `IntoResponse` + `Describe`
//!
//! ```
//! use moso::prelude::*;
//!
//! /// A user that has just been created.
//! #[derive(Schema, Responder, Debug)]
//! #[responder(status = 201)]
//! pub struct UserCreated {
//!     /// Stable identifier.
//!     pub id: u64,
//!     /// Contact address.
//!     pub email: Email,
//! }
//!
//! # fn main() {
//! let email = "ada@example.com".parse::<Email>().unwrap();
//! let response = UserCreated { id: 7, email }.into_response();
//! assert_eq!(response.status(), 201);
//! # }
//! ```
//!
//! ## `#[derive(Dependency)]` — the "wrap and check" shape
//!
//! ```
//! use moso::prelude::*;
//! # /// Who the request acts as.
//! # #[derive(Clone, Debug)] pub struct CurrentUser { pub is_admin: bool }
//! # impl Dependency for CurrentUser {
//! #     const PROVIDER_REQ: &'static [moso::ProviderReq] = &[];
//! #     async fn resolve(_: &RequestCtx) -> Result<Self> { Ok(CurrentUser { is_admin: true }) }
//! # }
//! /// A `CurrentUser` that has already been proved to be an administrator.
//! #[derive(Dependency, Clone, Debug)]
//! #[depends(from = CurrentUser, check = "is_admin", error = "admin required")]
//! pub struct AdminUser(pub CurrentUser);
//! # fn main() {}
//! ```
//!
//! ## `#[derive(Config)]` — layered typed configuration
//!
//! ```
//! use moso::prelude::*;
//! use std::net::SocketAddr;
//!
//! /// How the mailer is wired.
//! #[derive(Config, Debug)]
//! pub struct MailConfig {
//!     /// SMTP endpoint.
//!     #[config(default = "localhost:25")]
//!     pub smtp: String,
//! }
//!
//! /// Everything this application reads from its environment.
//! #[derive(Config, Debug)]
//! pub struct AppConfig {
//!     /// Where the server listens.
//!     #[config(default = "0.0.0.0:3000")]
//!     pub bind: SocketAddr,
//!     /// Connection string; never logged.
//!     #[config(secret)]
//!     pub database_url: SecretString,
//!     /// Mailer settings, under the `mail` prefix.
//!     #[config(nested)]
//!     pub mail: MailConfig,
//! }
//! # fn main() {
//! let descriptor = <AppConfig as Config>::descriptor();
//! assert!(descriptor.fields.iter().any(|f| f.name == "bind"));
//! # }
//! ```
//!
//! ## `#[derive(Error)]` — status and `type` URI mapping
//!
//! ```
//! use moso::prelude::*;
//!
//! /// The failures this application's domain can produce.
//! #[derive(Debug, moso::Error)]
//! pub enum ShopError {
//!     /// Not enough stock to satisfy the order.
//!     #[error(status = 409, type = "https://shop.example/errors/out-of-stock")]
//!     #[error(detail = "Only {available} left in stock")]
//!     OutOfStock {
//!         /// How many remain.
//!         available: u32,
//!     },
//!     /// The basket does not exist.
//!     #[error(status = 404)]
//!     NoSuchBasket,
//! }
//! # fn main() {
//! let error: Error = ShopError::OutOfStock { available: 2 }.into();
//! assert_eq!(error.status(), 409);
//! assert_eq!(error.detail(), Some("Only 2 left in stock"));
//! # }
//! ```
//!
//! # Not here
//!
//! `#[derive(Entity)]`, `#[derive(Projection)]`, `#[derive(Factory)]` and
//! `#[migration]` live in `moso-orm-macros`; `namespace!` and `#[cached]` in
//! `moso-kv`; `permissions!` and `roles!` in `moso-authz`; `#[job]` in
//! `moso-jobs`; `#[moso::test]` in `moso-test`.
//!
//! [`moso`]: https://docs.rs/moso

use proc_macro::TokenStream;
use syn::DeriveInput;

// ---------------------------------------------------------------------------
// Modules
// ---------------------------------------------------------------------------
//
// One module per macro family, plus `util` for the attribute plumbing they
// share. The crate root is the only file that names `proc_macro::TokenStream`:
// every module works in `proc_macro2` tokens so that it can be unit-tested
// outside macro expansion.
//
// The contract each module implements:
//
// | Module | Entry point |
// | --- | --- |
// | `endpoint` | `expand(attr: TokenStream2, item: TokenStream2) -> TokenStream2` |
// | `middleware` | `expand(attr: TokenStream2, item: TokenStream2) -> TokenStream2` |
// | `routes` | `expand_routes(TokenStream2)`, `expand_ep(TokenStream2)` |
// | `schema` | `derive_schema(DeriveInput)`, `derive_constrained(DeriveInput)` |
// | `config` | `expand(DeriveInput)` |
// | `error` | `expand(DeriveInput)` |
// | `responder` | `expand(DeriveInput)` |
// | `dependency` | `expand(DeriveInput)` |
//
// Derives take a parsed `DeriveInput` because the parse is identical for all of
// them and a failure there has nothing macro-specific to say; attribute and
// function-like macros take raw tokens because they must re-emit the user's
// item even when it does not parse.

mod authz;
mod config;
mod dependency;
mod endpoint;
mod error;
mod job;
mod middleware;
mod path;
mod responder;
mod routes;
mod schema;

// `util/attrs.rs` is a file module with no `util/mod.rs` beside it, so the
// parent is declared inline here. Switch to a plain `mod util;` the day
// `util/mod.rs` gains contents of its own.
mod util {
    //! Helpers shared by every macro in this crate.
    pub(crate) mod attrs;
}

/// Parse a derive input once, so that every derive body can assume it is valid.
///
/// A `DeriveInput` that does not parse is a syntax error in the user's type,
/// which rustc has already described better than any macro could; forwarding it
/// keeps the span and avoids a second, derived complaint.
fn derive(input: TokenStream, expand: fn(DeriveInput) -> proc_macro2::TokenStream) -> TokenStream {
    match syn::parse::<DeriveInput>(input) {
        Ok(parsed) => expand(parsed).into(),
        Err(error) => error.to_compile_error().into(),
    }
}

// ---------------------------------------------------------------------------
// Attribute macros
// ---------------------------------------------------------------------------

/// Turn an `async fn` into a documented operation.
///
/// ```
/// use moso::prelude::*;
/// # /// A database handle.
/// # #[derive(Default)] pub struct Db;
/// # /// A user, as the API accepts one.
/// # #[derive(Schema)] pub struct CreateUser { /// Public handle.
/// #     pub username: String }
/// # /// A user, as the API returns one.
/// # #[derive(Schema)] pub struct UserOut { /// Identifier.
/// #     pub id: u64 }
/// /// Create a user.
/// #[endpoint]
/// async fn create(Inject(db): Inject<Db>, Json(body): Json<CreateUser>)
///     -> Result<Created<UserOut>>
/// {
///     let _ = (db, body);
///     Ok(Created::at("/users/1", UserOut { id: 1 }))
/// }
/// # fn main() {
/// #     assert_eq!(<__moso_op_create as moso::Endpoint>::NAME, "create");
/// # }
/// ```
///
/// The doc comment becomes the operation's summary and description, each
/// parameter contributes its OpenAPI shape and its provider requirements, and
/// the return type contributes its responses.
///
/// # Arguments
///
/// `operation_id`, `tag`, `hidden`, `deprecated`,
/// `response(status, description)`, `example(request = …, response = …)`,
/// `errors = Type`. All optional; the common case is a bare `#[endpoint]`.
///
/// # Errors it reports itself
///
/// A body extractor that is not last, more than one body extractor, more than
/// 16 parameters, a function that is not `async`, a `self` parameter, and
/// generic parameters — each with a span on the offending token and a `help:`
/// line that can be pasted.
#[proc_macro_attribute]
pub fn endpoint(args: TokenStream, item: TokenStream) -> TokenStream {
    endpoint::expand(args.into(), item.into()).into()
}

/// Turn an `async fn` into a named Tower layer.
///
/// ```
/// use moso::prelude::*;
/// use moso::middleware::Next;
/// use moso::{Request, Response};
/// # /// One customer's slice of the system.
/// # #[derive(Clone)] pub struct Tenant(String);
/// # impl Tenant {
/// #     fn from_host(h: &moso::deps::http::HeaderMap) -> Result<Self> {
/// #         let _ = h;
/// #         Ok(Tenant("acme".to_owned()))
/// #     }
/// # }
/// /// Resolve the tenant from the request's `Host` header.
/// #[moso::middleware]
/// async fn tenant(mut req: Request, next: Next) -> Result<Response> {
///     let tenant = Tenant::from_host(req.headers())?;
///     req.extensions_mut().insert(tenant);
///     Ok(next.run(req).await)
/// }
/// # fn main() { assert_eq!(TenantLayer::NAME, "tenant"); }
/// ```
///
/// Generates `TenantLayer` and `TenantService<S>`: both `Clone`, both named
/// after the function, and boxed once at registration so the service does not
/// monomorphise per route. Register the layer anywhere a `tower::Layer` is
/// accepted:
///
/// ```
/// use moso::prelude::*;
/// use moso::middleware::{Next, Slot};
/// use moso::{MiddlewareStack, Request, Response};
/// # /// Stamp a header on the way out.
/// # #[moso::middleware]
/// # async fn tenant(req: Request, next: Next) -> Result<Response> { Ok(next.run(req).await) }
/// # /// Log in.
/// # #[endpoint] async fn login() -> Result<NoContent> { Ok(NoContent) }
/// # fn main() {
/// let router = Router::new()
///     .post("/auth/login", moso::ep!(login))
///     .layer(TenantLayer::new());
///
/// let mut stack = MiddlewareStack::default();
/// stack.insert_after(Slot::Trace, TenantLayer::NAME, TenantLayer::new());
/// # let _ = router;
/// # }
/// ```
///
/// # Parameters
///
/// The last two must be `req: Request` and `next: Next`. Anything before them
/// is extracted with `Extract` before the function runs, so `Inject<Db>` works
/// and its `PROVIDER_REQ` is folded into `TenantLayer::PROVIDER_REQ` for boot
/// validation.
///
/// `Depends<T>` is rejected: middleware runs before extraction, so request
/// dependencies are not available yet. So are body extractors, which would
/// consume the body before the handler could read it.
///
/// # Arguments
///
/// | Key | Effect |
/// | --- | --- |
/// | `name = "…"` | the name `moso middleware` prints; defaults to the function's |
/// | `vis = "…"` | visibility of the generated types |
/// | `layer = "…"` | rename the `…Layer` type |
/// | `service = "…"` | rename the `…Service` type |
///
/// The generated types take the function's visibility, widened to
/// `pub(crate)` when the function is private — a middleware is almost always
/// registered from a different module than the one that defines it.
#[proc_macro_attribute]
pub fn middleware(args: TokenStream, item: TokenStream) -> TokenStream {
    middleware::expand(args.into(), item.into()).into()
}

// ---------------------------------------------------------------------------
// Function-like macros
// ---------------------------------------------------------------------------

/// Register a table of routes.
///
/// ```
/// use moso::prelude::*;
/// # /// A user.
/// # #[derive(Schema)] pub struct UserOut { /// Identifier.
/// #     pub id: u64 }
/// # /// List users.
/// # #[endpoint] async fn list() -> Result<Json<Vec<UserOut>>> { Ok(Json(vec![])) }
/// # /// Create a user.
/// # #[endpoint] async fn create() -> Result<Created<UserOut>> {
/// #     Ok(Created::at("/users/1", UserOut { id: 1 })) }
/// # /// Show a user.
/// # #[endpoint] async fn show(Path(id): Path<u64>) -> Result<Json<UserOut>> {
/// #     Ok(Json(UserOut { id })) }
/// # /// Delete a user.
/// # #[endpoint] async fn destroy(Path(_id): Path<u64>) -> Result<NoContent> { Ok(NoContent) }
/// /// Everything this module serves.
/// pub fn router() -> Router {
///     moso::routes! {
///         GET    "/users"      => list,
///         POST   "/users"      => create,
///         GET    "/users/{id}" => show,
///         DELETE "/users/{id}" => destroy,
///     }
///     .tag("users")
/// }
/// # fn main() { assert_eq!(router().len(), 4); }
/// ```
///
/// Path literals go through `route_path!`, so `:id`, `*rest`, an unbalanced
/// brace or a catch-all that is not last is a compile error at the literal
/// rather than a panic at boot.
#[proc_macro]
pub fn routes(input: TokenStream) -> TokenStream {
    routes::expand_routes(input.into()).into()
}

/// Register one route, where a table would be noise.
///
/// ```
/// use moso::prelude::*;
/// # /// Liveness.
/// # #[endpoint] async fn healthz() -> Result<NoContent> { Ok(NoContent) }
/// # /// Receive a Stripe event.
/// # #[endpoint] async fn stripe() -> Result<NoContent> { Ok(NoContent) }
/// # fn main() {
/// let router = Router::new()
///     .get("/healthz", moso::ep!(healthz))
///     .post("/webhooks/stripe", moso::ep!(stripe));
/// assert_eq!(router.len(), 2);
/// # }
/// ```
#[proc_macro]
pub fn ep(input: TokenStream) -> TokenStream {
    routes::expand_ep(input.into()).into()
}

/// Declare the application's permission registry.
///
/// ```
/// # #[cfg(feature = "never")]
/// moso::permissions! {
///     /// Posts
///     posts.read    = "View posts",
///     posts.publish = "Publish posts",
///
///     /// Administration
///     admin.access  = "Access the admin panel",
/// }
/// ```
///
/// Generates `pub enum Perm` with `ALL`, `as_str`, `description`, `group` and
/// `parse`, plus the `Permission` impl the authorization layer needs. A
/// permission is a compile-time constant: a typo in `#[requires]` is a compile
/// error, not a silent `false`.
///
/// Declaration order is bit order, so **reordering the list changes what a
/// stored `PermSet` means**. Add at the end; the registry fingerprint catches
/// the mistake across a process boundary either way.
///
/// # Errors it reports itself
///
/// A duplicate name (naming both declarations), a name without a dot, a
/// description that is not a string literal, an empty registry, and — through a
/// `const` assertion — more than 256 permissions.
///
/// Requires the facade's `authz` feature.
#[proc_macro]
pub fn permissions(input: TokenStream) -> TokenStream {
    authz::permissions(input.into()).into()
}

/// Declare the application's static roles, with inheritance.
///
/// ```
/// # #[cfg(feature = "never")]
/// moso::roles! {
///     /// Read-only access.
///     Viewer = [posts.read],
///     /// Writes and publishes.
///     Editor = Viewer + [posts.publish],
///     /// Everything.
///     Admin  = Editor + [admin.access],
/// }
/// ```
///
/// Inheritance is flattened at expansion time, so a role's permissions are a
/// `const PermSet` and resolving one costs a copy of four words. A cycle is a
/// compile error naming both roles and printing the path; an unknown parent is
/// a compile error with a "did you mean".
///
/// Customer-defined roles stored in a table are `RoleSource`'s job and are not
/// bounded by the 64-role cap this enforces.
///
/// Requires the facade's `authz` feature.
#[proc_macro]
pub fn roles(input: TokenStream) -> TokenStream {
    authz::roles(input.into()).into()
}

/// Require permissions before a handler's body runs.
///
/// ```text
/// #[requires(Perm::PostsCreate)]
/// #[endpoint]
/// async fn create(Json(body): Json<CreatePost>) -> Result<Created<PostOut>> { … }
/// ```
///
/// The check happens before the body, contributes a `security` requirement and
/// a 403 naming each permission to the OpenAPI document, and declares the
/// endpoint's intent so `moso check --authz` can tell "considered" from
/// "forgotten".
///
/// # Arguments
///
/// A comma-separated list of permissions, each an enum variant
/// (`Perm::PostsCreate`, compile-checked) or a wire name (`"posts.create"`,
/// checked against the registry at boot with a suggestion). `any(..)` means one
/// is enough; the bare word `audit` records the *allows* as well as the denials.
///
/// # It goes above `#[endpoint]`
///
/// Rust expands the outermost attribute first. `#[endpoint]` generates its
/// extraction glue from the signature it sees, so a check added afterwards
/// would never run — which is why `#[requires]` refuses to expand unless
/// `#[endpoint]` is still below it, and says so.
///
/// Requires the facade's `authz` feature.
#[proc_macro_attribute]
pub fn requires(args: TokenStream, item: TokenStream) -> TokenStream {
    authz::requires(args.into(), item.into()).into()
}

/// Declare that an endpoint deliberately needs no authorization.
///
/// ```text
/// #[public]
/// #[endpoint]
/// async fn healthz() -> Result<NoContent> { Ok(NoContent) }
/// ```
///
/// Deny-by-default is only provable if "nothing declared" is distinguishable
/// from "declared open". This is that distinction: `moso check --authz` reports
/// every endpoint with no `#[requires]`, no `Authorized<..>` parameter and no
/// `#[public]`, and `lints.missing_authz = "deny"` turns the report into a
/// build failure.
///
/// Like `#[requires]`, it goes above `#[endpoint]`.
///
/// Requires the facade's `authz` feature.
#[proc_macro_attribute]
pub fn public(args: TokenStream, item: TokenStream) -> TokenStream {
    authz::public(args.into(), item.into()).into()
}

/// Turn an `async fn` into a background job.
///
/// ```text
/// #[job(
///     queue = "mail",
///     retries = 5,
///     backoff = "exponential(30s, max = 1h)",
///     timeout = "2m",
///     unique_for = "10m",
/// )]
/// pub async fn send_welcome_email(
///     args: SendWelcome,               // the payload: any Serialize + DeserializeOwned
///     Inject(db): Inject<Db>,          // the same DI a handler uses
///     Inject(mail): Inject<dyn Mailer>,
///     ctx: JobCtx,                     // attempt number, job id, cancellation, heartbeat
/// ) -> Result<()> {
///     let user = User::find(args.user_id).fetch_one(&db).await?;
///     mail.send(WelcomeEmail { user: &user }).await?;
///     Ok(())
/// }
/// ```
///
/// Generates `SendWelcomeEmail`: a unit struct implementing `Job`, with the
/// attribute's values as its associated constants. Enqueue it with
/// `SendWelcomeEmail::enqueue(&jobs, args)`, or inside a transaction with
/// `tx.enqueue(SendWelcomeEmail, args)` — which is the headline feature, because
/// the job row then commits and rolls back with the work that caused it.
///
/// The function is left exactly as written, so it stays directly callable from
/// a test.
///
/// # Parameters
///
/// The payload comes first, then any number of `Inject(..)`, then an optional
/// `ctx: JobCtx`. There is no request here, so there is nothing to extract:
/// `Json`, `Query`, `Path` and `Depends` are rejected with a message saying the
/// value belongs in the payload, which is the only thing the queue row carries.
///
/// # Arguments
///
/// | Key | Effect |
/// | --- | --- |
/// | `name = "…"` | the **wire** name; defaults to the function's, and pins it when the function moves |
/// | `type_name = "…"` | rename the generated struct |
/// | `queue = "…"` | which queue; defaults to `default` |
/// | `retries = 5` | the retry budget |
/// | `backoff = "…"` | `immediate`, `fixed(30s)`, `linear(30s, max = 1h)`, `exponential(30s, max = 1h)` |
/// | `timeout = "2m"` | how long one attempt may take |
/// | `unique_for = "10m"` | deduplicate identical payloads |
/// | `priority = "high"` | `low`, `normal`, `high`, `critical` |
/// | `serial` | jobs sharing a `unique_key` run strictly one at a time |
///
/// Durations and the backoff spec are parsed **at expansion time**, so
/// `timeout = "2 minuts"` is a compile error on the attribute rather than a
/// surprise at the first retry.
///
/// The generated type takes the function's visibility, widened to `pub(crate)`
/// when the function is private — a job is almost always registered from a
/// different module than the one that defines it.
///
/// Requires the facade's `jobs` feature.
#[proc_macro_attribute]
pub fn job(args: TokenStream, item: TokenStream) -> TokenStream {
    job::expand(args.into(), item.into()).into()
}

// ---------------------------------------------------------------------------
// Derives
// ---------------------------------------------------------------------------

/// Serde, validation and JSON Schema from one set of attributes.
///
/// ```
/// use moso::prelude::*;
///
/// /// A user, as the API accepts one.
/// #[derive(Schema)]
/// pub struct CreateUser {
///     /// Public handle.
///     #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
///     pub username: String,
///     /// Contact address.
///     pub email: Email,
///     /// Optional age, in years.
///     #[schema(range = 13..=130)]
///     pub age: Option<u8>,
/// }
/// # fn main() {
/// let user: CreateUser = serde_json::from_str(
///     r#"{"username":"ada","email":"ada@example.com","age":36}"#,
/// ).unwrap();
/// let ctx = &mut moso::schema::ValidationCtx::new();
/// assert!(moso::schema::Validate::validate(&user, ctx).is_ok());
/// # }
/// ```
///
/// Generates `Serialize`, `Deserialize`, `Validate`, `Schema`, `Debug` (which
/// redacts `#[schema(secret)]` fields), and the `IntoResponse` + `Describe`
/// pair that lets the type be returned from a handler directly.
///
/// The runtime check and the documented constraint are generated from the
/// *same* attribute, so they cannot disagree.
#[proc_macro_derive(Schema, attributes(schema))]
pub fn derive_schema(input: TokenStream) -> TokenStream {
    derive(input, schema::derive_schema)
}

/// A newtype that cannot hold an invalid value.
///
/// ```
/// use moso::prelude::*;
///
/// /// An order number, which cannot exist in an invalid state.
/// #[derive(Constrained, Debug)]
/// #[constrained(inner = String, pattern = r"^ORD-\d{8}$", format = "order-number")]
/// pub struct OrderNumber(String);
/// # fn main() {
/// assert!(OrderNumber::new("ORD-00000042".to_owned()).is_ok());
/// // `Deserialize` routes through the constructor, so a bad value never exists.
/// assert!(serde_json::from_str::<OrderNumber>(r#""nope""#).is_err());
/// # }
/// ```
///
/// The constructor validates, `Deserialize` routes through the constructor, and
/// the JSON Schema carries the same constraint — so "parse, don't validate"
/// costs one line.
#[proc_macro_derive(Constrained, attributes(constrained))]
pub fn derive_constrained(input: TokenStream) -> TokenStream {
    derive(input, schema::derive_constrained)
}

/// `IntoResponse` + `Describe` with a status and headers you choose.
///
/// ```
/// use moso::prelude::*;
///
/// /// A user that has just been created.
/// #[derive(Schema, Responder)]
/// #[responder(status = 201, header(location = "self.url"))]
/// pub struct UserCreated {
///     /// Where the new user can be fetched; sent as `Location`, not in the body.
///     #[serde(skip)]
///     pub url: String,
///     /// Stable identifier.
///     pub id: u64,
///     /// Contact address.
///     pub email: Email,
/// }
/// # fn main() {
/// let created = UserCreated {
///     url: "/users/7".to_owned(),
///     id: 7,
///     email: "ada@example.com".parse().unwrap(),
/// };
/// let response = created.into_response();
/// assert_eq!(response.status(), 201);
/// assert_eq!(response.headers()["location"], "/users/7");
/// # }
/// ```
///
/// Use it when the response needs a status other than 200 or a header derived
/// from the body. For a plain 200 JSON body, `#[derive(Schema)]` is enough.
#[proc_macro_derive(Responder, attributes(responder, serde))]
pub fn derive_responder(input: TokenStream) -> TokenStream {
    derive(input, responder::expand)
}

/// A request-scoped dependency, for the "wrap and check" shape.
///
/// ```
/// use moso::prelude::*;
/// # /// Who the request acts as.
/// # #[derive(Clone, Debug)] pub struct CurrentUser { pub is_admin: bool }
/// # impl Dependency for CurrentUser {
/// #     const PROVIDER_REQ: &'static [moso::ProviderReq] = &[];
/// #     async fn resolve(_: &RequestCtx) -> Result<Self> { Ok(CurrentUser { is_admin: true }) }
/// # }
/// /// A `CurrentUser` already proved to be an administrator.
/// #[derive(Dependency, Clone)]
/// #[depends(from = CurrentUser, check = "is_admin", error = "admin required")]
/// pub struct AdminUser(pub CurrentUser);
/// # fn main() {}
/// ```
///
/// Equivalent to a hand-written `Dependency` impl that resolves `CurrentUser`
/// from the request cache, runs the check and returns 403 with the given
/// message — including the `PROVIDER_REQ` the source dependency declares, so
/// boot validation still sees through the wrapper.
#[proc_macro_derive(Dependency, attributes(depends))]
pub fn derive_dependency(input: TokenStream) -> TokenStream {
    derive(input, dependency::expand)
}

/// Layered typed configuration with a boot-time report.
///
/// ```
/// use moso::prelude::*;
/// use std::net::SocketAddr;
/// # /// How the mailer is wired.
/// # #[derive(Config, Debug)] pub struct MailConfig {
/// #     /// SMTP endpoint.
/// #     #[config(default = "localhost:25")] pub smtp: String }
/// /// Everything this application reads from its environment.
/// #[derive(Config, Debug)]
/// pub struct AppConfig {
///     /// Where the server listens.
///     #[config(default = "0.0.0.0:3000")]
///     pub bind: SocketAddr,
///     /// Connection string; never logged.
///     #[config(secret)]
///     pub database_url: SecretString,
///     /// Mailer settings, under the `mail` prefix.
///     #[config(nested)]
///     pub mail: MailConfig,
/// }
/// # fn main() {
/// let descriptor = <AppConfig as Config>::descriptor();
/// assert!(descriptor.fields.iter().any(|f| f.name == "database_url" && f.secret));
/// # }
/// ```
///
/// Every field becomes a `FieldDescriptor`, so a missing or unparseable value
/// is reported with its key, its source and its default — all of them at once,
/// rather than one panic per run.
#[proc_macro_derive(Config, attributes(config))]
pub fn derive_config(input: TokenStream) -> TokenStream {
    derive(input, config::expand)
}

/// Map an error enum onto RFC 9457 problem documents.
///
/// ```
/// use moso::prelude::*;
/// # /// The payment gateway said no.
/// # #[derive(Debug)] pub struct PaymentError;
/// # impl std::fmt::Display for PaymentError {
/// #     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("declined") }
/// # }
/// # impl std::error::Error for PaymentError {}
/// /// The failures this application's domain can produce.
/// #[derive(Debug, moso::Error)]
/// pub enum ShopError {
///     /// Not enough stock to satisfy the order.
///     #[error(status = 409, type = "https://shop.example/errors/out-of-stock")]
///     #[error(detail = "Only {available} left in stock")]
///     OutOfStock {
///         /// How many remain.
///         available: u32,
///     },
///
///     /// The payment could not be taken.
///     #[error(status = 500)]
///     Payment(#[from] PaymentError),
/// }
/// # fn main() {
/// let error: Error = ShopError::OutOfStock { available: 2 }.into();
/// assert_eq!(error.status(), 409);
/// // `?` works from any handler, because `From` reaches `moso::Error`.
/// let from_source: ShopError = PaymentError.into();
/// assert_eq!(Error::from(from_source).status(), 500);
/// # }
/// ```
///
/// Generates `Display`, `std::error::Error`, `Into<moso::Error>` and the
/// status/`type` mapping the problem renderer reads — so `?` works from any
/// handler and the document stays truthful.
#[proc_macro_derive(Error, attributes(error, from, source))]
pub fn derive_error(input: TokenStream) -> TokenStream {
    derive(input, error::expand)
}
