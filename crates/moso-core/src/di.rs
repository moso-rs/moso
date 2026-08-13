//! Dependency injection, in two tiers.
//!
//! | | [`Inject<T>`] (provider) | [`Depends<T>`] (dependency) |
//! | --- | --- | --- |
//! | Lifetime | application | one request |
//! | Registered | `App::provide(..)` | `impl Dependency for T` |
//! | Resolution | type-map lookup + `Arc` clone | async fn, memoised per request |
//! | Can fail | **no** — boot proved it exists | yes, as a typed [`Error`] |
//! | Documents itself | no | yes: security schemes, 401/403 |
//! | Examples | `Db`, `Kv`, `Mailer`, `Config<AppConfig>` | `CurrentUser`, `Tenant`, `RequestTx` |
//!
//! FastAPI conflates these; Rust lets us separate them, and the separation is
//! what buys the boot guarantee. **[`Inject<T>`] is infallible at the use
//! site**: no `?`, no `expect`, no `FromRef` trait error. If the provider were
//! missing, the application would not have booted.
//!
//! # How the boot check works
//!
//! Every extractor declares [`Extract::PROVIDER_REQ`], a `const` slice of
//! [`ProviderReq`]. `#[endpoint]` concatenates its parameters' slices into
//! [`Endpoint::required_providers`](crate::Endpoint::required_providers).
//! `AppBuilder::build` walks every registered
//! operation and checks each requirement against the frozen provider map,
//! collecting *all* misses into one report.
//!
//! A `Dependency` that itself uses `Inject<Db>` declares that in its own
//! `PROVIDER_REQ`, which `#[derive(Dependency)]` computes from the fields. A
//! hand-written impl that forgets loses the boot check but nothing breaks:
//! `ctx.provider::<T>()` then returns a clear runtime error naming the type and
//! telling the author to declare `PROVIDER_REQ`.
//!
//! # Representation
//!
//! The map is `HashMap<TypeId, Arc<dyn Any + Send + Sync>>`, built once at
//! boot, frozen in an `Arc`, never mutated. Every value is stored as an
//! `Arc<T>` boxed *again* into the `dyn Any`, uniformly for sized and unsized
//! `T`. That uniformity is what makes `provide_dyn::<dyn Mailer>` and
//! `provide(db)` share one lookup path, and it is what makes
//! `Inject<dyn Mailer>` — the key to swapping a `CapturingMailer` into a test —
//! possible at all.
//!
//! ## Why the value is `Arc<Arc<T>>` and not `Arc<T>`
//!
//! The obvious representation is `Arc<T>` unsized straight into
//! `Arc<dyn Any + Send + Sync>` and read back with `Arc::downcast`. It does not
//! work for the case the whole design exists to serve. `Arc::downcast` recovers
//! the *concrete* type behind the `dyn Any`, so an `Arc<SmtpMailer>` stored that
//! way can only ever be read back as `Arc<SmtpMailer>` — never as
//! `Arc<dyn Mailer>`, because `dyn Mailer` is not the concrete type and there is
//! no stable way to re-attach a vtable.
//!
//! Storing `Arc::new(the_arc)` instead makes the *`Arc<T>` handle itself* the
//! concrete value in the map, and `T` may then be `dyn Mailer` just as happily
//! as `SmtpMailer`: `TypeId::of::<dyn Mailer>()` is the key,
//! `downcast_ref::<Arc<dyn Mailer>>()` is the read, and the vtable travels
//! inside the stored handle. One extra allocation per provider at boot, one
//! extra pointer hop per lookup, and `Inject<T>` has exactly one code path.
//!
//! Lookup is therefore: hash a `TypeId`, `downcast_ref` to `Arc<T>` (a vtable
//! pointer comparison), clone the `Arc` (a relaxed increment). No allocation,
//! no lock, no copy of the value.

// The test-override table below is gated on `cfg(any(test, feature = "test"))`,
// per docs/01-http/15-dependency-injection.md. Both halves are real: `cfg(test)`
// selects it for this crate's own tests, and the `test` cargo feature (declared
// in Cargo.toml, re-exported by the facade as `moso/test`) selects it for a
// downstream crate's test suite. A default build has neither, so a production
// application contains no override table and no lookup for one.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::ops::Deref;
use std::sync::Arc;

use moso_openapi::OperationBuilder;

use crate::ctx::RequestCtx;
use crate::error::{Error, Result};
use crate::extract::Extract;

// ---------------------------------------------------------------------------
// ProviderReq
// ---------------------------------------------------------------------------

/// One provider an operation needs, in a form that survives `const` evaluation.
///
/// Both members are function pointers rather than values because
/// `core::any::type_name` is not yet callable in a `const fn`, and
/// [`ProviderReq::of`] **must** be `const`: it is written into
/// `Extract::PROVIDER_REQ`, an associated `const`. Read them through
/// [`ProviderReq::id`] and [`ProviderReq::name`].
///
/// Keeping the whole type const-evaluable is deliberate: a future `moso build`
/// could perform the boot-time DI analysis ahead of time without changing any
/// user-facing API.
///
/// ```
/// use moso::prelude::*;
/// use moso::ProviderReq;
///
/// /// A database handle.
/// #[derive(Default)]
/// pub struct Db;
///
/// /// List users.
/// #[endpoint]
/// async fn list(Inject(db): Inject<Db>) -> Result<moso::response::NoContent> {
///     let _ = db;
///     Ok(moso::response::NoContent)
/// }
///
/// # fn main() {
/// // `const`, so the whole requirement graph is known before `main` runs.
/// const REQUIRED: &[ProviderReq] = &[ProviderReq::of::<Db>()];
/// assert_eq!(REQUIRED[0].id(), std::any::TypeId::of::<Db>());
/// assert!(REQUIRED[0].name().ends_with("Db"));
/// assert!(!REQUIRED[0].optional);
///
/// // Which is exactly what `App::build()` checks the provider map against.
/// assert_eq!(
///     <__moso_op_list as moso::Endpoint>::required_providers()[0].id(),
///     REQUIRED[0].id(),
/// );
/// # }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ProviderReq {
    /// `TypeId::of::<T>` — the map key this requirement resolves against.
    pub type_id: fn() -> TypeId,
    /// `core::any::type_name::<T>` — the name the boot report prints.
    pub type_name: fn() -> &'static str,
    /// Whether a missing provider is tolerable.
    ///
    /// An optional requirement is not checked at boot and yields `None` at
    /// runtime. Batteries use it for genuinely optional integrations; handlers
    /// should not.
    pub optional: bool,
}

impl ProviderReq {
    /// A required provider of type `T`.
    pub const fn of<T: ?Sized + 'static>() -> Self {
        Self {
            type_id: TypeId::of::<T>,
            type_name: core::any::type_name::<T>,
            optional: false,
        }
    }

    /// A provider of type `T` that the application may omit.
    pub const fn optional_of<T: ?Sized + 'static>() -> Self {
        Self {
            type_id: TypeId::of::<T>,
            type_name: core::any::type_name::<T>,
            optional: true,
        }
    }

    /// The map key.
    pub fn id(&self) -> TypeId {
        (self.type_id)()
    }

    /// The fully-qualified type name, for the boot report.
    pub fn name(&self) -> &'static str {
        (self.type_name)()
    }
}

impl PartialEq for ProviderReq {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id() && self.optional == other.optional
    }
}

impl Eq for ProviderReq {}

// ---------------------------------------------------------------------------
// ProviderMap
// ---------------------------------------------------------------------------

/// The frozen, application-lifetime type map.
///
/// Built by [`ProviderMapBuilder`], wrapped in an `Arc`, and never mutated
/// afterwards — which is why lookup needs no lock and why `Inject` can hand out
/// `Arc<T>` freely.
///
/// ```
/// use moso::di::{ProviderMapBuilder, ProviderReq};
///
/// /// A database handle.
/// #[derive(Debug, Default)]
/// pub struct Db;
///
/// let mut builder = ProviderMapBuilder::new();
/// builder.insert(Db::default());
/// let map = builder.build();
///
/// assert!(map.contains::<Db>());
/// assert!(map.contains_req(&ProviderReq::of::<Db>()));
/// assert!(map.get::<Db>().is_some());
///
/// // A type nobody registered is simply absent — which is what `App::build()`
/// // turns into a boot error naming it.
/// assert!(!map.contains::<String>());
/// ```
///
/// An application never builds one: `App::new(cfg).provide(...)` does, once, at
/// boot.
#[derive(Debug, Default)]
pub struct ProviderMap {
    entries: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    names: HashMap<TypeId, &'static str>,
}

impl ProviderMap {
    /// An empty map. Use [`ProviderMapBuilder`] to populate one.
    pub fn new() -> Self {
        Self::default()
    }

    /// The provider registered for `T`, if any.
    ///
    /// Works for `T = dyn Trait` as well as for sized types, because every
    /// value is stored as an `Arc<T>`. A hash, a `downcast_ref` and an `Arc`
    /// clone; no allocation and no lock. See the module header for why the
    /// stored value is `Arc<Arc<T>>`.
    ///
    /// `None` means "nothing is registered under `TypeId::of::<T>()`". A value
    /// registered as `Arc<dyn Mailer>` is **not** reachable as
    /// `get::<SmtpMailer>()` and vice versa: the key is the type you asked for,
    /// which is the same rule `App::build` validates against.
    pub fn get<T: ?Sized + Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        let entry = self.entries.get(&TypeId::of::<T>())?;
        let handle: &Arc<T> = (**entry).downcast_ref::<Arc<T>>()?;
        Some(Arc::clone(handle))
    }

    /// Store `value` under `TypeId::of::<T>()`, replacing anything already
    /// there.
    ///
    /// The single write path: [`ProviderMapBuilder::insert`],
    /// [`insert_arc`](ProviderMapBuilder::insert_arc) and
    /// [`insert_dyn`](ProviderMapBuilder::insert_dyn) all funnel through it, so
    /// the representation is decided in exactly one place.
    fn put<T: ?Sized + Send + Sync + 'static>(&mut self, value: Arc<T>) {
        let id = TypeId::of::<T>();
        self.entries.insert(id, Arc::new(value));
        self.names.insert(id, core::any::type_name::<T>());
    }

    /// Whether a provider is registered for this requirement.
    pub fn contains_req(&self, req: &ProviderReq) -> bool {
        self.entries.contains_key(&req.id())
    }

    /// Whether a provider is registered for `T`.
    pub fn contains<T: ?Sized + 'static>(&self) -> bool {
        self.entries.contains_key(&TypeId::of::<T>())
    }

    /// How many providers are registered.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every registered type name, sorted, for the boot report's
    /// `registered providers:` block.
    pub fn registered_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.names.values().copied().collect();
        names.sort_unstable();
        names
    }
}

/// Accumulates providers during boot, then freezes into a [`ProviderMap`].
///
/// Registration is last-write-wins, which is what makes
/// `TestApp::override_provider` a one-liner.
#[derive(Debug, Default)]
pub struct ProviderMapBuilder {
    map: ProviderMap,
}

impl ProviderMapBuilder {
    /// An empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a concrete value, retrievable as `Inject<T>`.
    ///
    /// Last write wins, which is what makes `TestApp::override_provider` a
    /// one-liner: register the real thing, then register the fake over it.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) -> &mut Self {
        self.map.put(Arc::new(value));
        self
    }

    /// Register an already-shared value, so two providers can alias one object.
    pub fn insert_arc<T: Send + Sync + 'static>(&mut self, value: Arc<T>) -> &mut Self {
        self.map.put(value);
        self
    }

    /// Register a trait object, retrievable as `Inject<dyn Trait>`.
    ///
    /// This is the testability lever: production wires `SmtpMailer`, the test
    /// app wires `CapturingMailer`, and no handler changes.
    ///
    /// Note that the key is `TypeId::of::<dyn Trait>()`, *not* the concrete
    /// type's: `insert_dyn::<dyn Mailer>(Arc::new(SmtpMailer))` makes
    /// `Inject<dyn Mailer>` work and deliberately leaves `Inject<SmtpMailer>`
    /// unregistered, because depending on the concrete type is the thing the
    /// trait object exists to prevent.
    pub fn insert_dyn<T: ?Sized + Send + Sync + 'static>(&mut self, value: Arc<T>) -> &mut Self {
        self.map.put(value);
        self
    }

    /// The map as it stands, for resolving `provide_with` factories that read
    /// providers registered before them.
    pub fn as_map(&self) -> &ProviderMap {
        &self.map
    }

    /// Freeze. The result is shared and never mutated again.
    pub fn build(self) -> Arc<ProviderMap> {
        Arc::new(self.map)
    }
}

// ---------------------------------------------------------------------------
// Inject
// ---------------------------------------------------------------------------

/// An application-lifetime value, resolved from the provider map.
///
/// ```
/// use moso::prelude::*;
/// # /// A user, as the API returns one.
/// # #[derive(Schema)] pub struct UserOut { /// Stable identifier.
/// #     pub id: u64 }
/// /// A database handle, registered once at boot.
/// #[derive(Default)]
/// pub struct Db;
/// impl Db {
///     /// Every user, newest first.
///     async fn all(&self) -> Vec<UserOut> { vec![UserOut { id: 1 }] }
/// }
///
/// /// List users.
/// #[endpoint]
/// async fn list(Inject(db): Inject<Db>) -> Result<Page<UserOut>> {
///     // `db` is an `Arc<Db>`; `&*db` and method calls work through `Deref`.
///     Ok(Page::new(db.all().await))
/// }
/// # fn main() { assert_eq!(Router::new().get("/users", moso::ep!(list)).len(), 1); }
/// ```
///
/// Extraction cannot fail at the use site: `App::build()` refused to start if
/// no provider for `Db` was registered.
///
/// Infallible by construction: `AppBuilder::build` refused to produce an `App`
/// unless every `Inject<T>` in the route table had a provider. Contributes
/// nothing to the OpenAPI document — an injected pool is not part of the API
/// contract — but does contribute a [`ProviderReq`] to the boot check.
pub struct Inject<T: ?Sized + 'static>(pub Arc<T>);

impl<T: ?Sized> Clone for Inject<T> {
    fn clone(&self) -> Self {
        Inject(Arc::clone(&self.0))
    }
}

impl<T: ?Sized> Deref for Inject<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: ?Sized> core::fmt::Debug for Inject<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Inject(..)")
    }
}

impl<T: ?Sized + Send + Sync + 'static> Inject<T> {
    /// The shared value.
    pub fn into_inner(self) -> Arc<T> {
        self.0
    }
}

impl<T: ?Sized + Send + Sync + 'static> Extract for Inject<T> {
    const PROVIDER_REQ: &'static [ProviderReq] = &[ProviderReq::of::<T>()];

    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    async fn extract(_parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        Ok(Inject(ctx.provider::<T>()?))
    }
}

// ---------------------------------------------------------------------------
// Dependency
// ---------------------------------------------------------------------------

/// A value resolved once per request and cached by `TypeId`.
///
/// Two extractors and a guard all asking for `CurrentUser` cause one database
/// query. This is FastAPI's dependency cache, made explicit and made typed.
///
/// Dependencies compose: a `resolve` body calls `ctx.depends::<Other>()`, and
/// the cache makes the composition free.
///
/// Implemented by `#[derive(moso::Dependency)]` for the two common shapes, and
/// by hand for anything else. `Depends<T>` in a handler signature is what makes
/// the framework call [`resolve`](Dependency::resolve); the result is memoised
/// in the [`RequestCtx`] for the rest of the request.
///
/// ```
/// use moso::prelude::*;
/// # /// A user record.
/// # #[derive(Clone, Debug)] pub struct User { pub is_admin: bool }
/// /// Who the request acts as.
/// #[derive(Clone, Debug)]
/// pub struct CurrentUser(pub User);
///
/// impl Dependency for CurrentUser {
///     async fn resolve(_ctx: &RequestCtx) -> Result<Self> {
///         Ok(CurrentUser(User { is_admin: true }))
///     }
/// }
///
/// /// A `CurrentUser` already proved to be an administrator.
/// #[derive(Clone, Debug)]
/// pub struct AdminUser(pub User);
///
/// impl Dependency for AdminUser {
///     const PROVIDER_REQ: &'static [moso::ProviderReq] =
///         <CurrentUser as Dependency>::PROVIDER_REQ;
///
///     fn describe(op: &mut OperationBuilder) {
///         <CurrentUser as Dependency>::describe(op);
///         op.response(403, ResponseSpec::problem("Admin required"));
///     }
///
///     async fn resolve(ctx: &RequestCtx) -> Result<Self> {
///         let CurrentUser(user) = ctx.depends::<CurrentUser>().await?;
///         if !user.is_admin {
///             return Err(Error::forbidden("admin required"));
///         }
///         Ok(AdminUser(user))
///     }
/// }
/// # fn main() {}
/// ```
///
/// Two extractors and a guard all asking for `CurrentUser` cost one resolve.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a request dependency",
    label = "not a dependency",
    note = "add `#[derive(moso::Dependency)]` to `{Self}`, or implement `Dependency` by hand",
    note = "a dependency is resolved once per request and cached; for an application-lifetime \
            value such as a database pool use `Inject<{Self}>` and register it with \
            `App::provide`",
    note = "help: write `#[derive(moso::Dependency, Clone)]` above `{Self}`"
)]
pub trait Dependency: Clone + Send + Sync + 'static {
    /// Providers this dependency — and everything it resolves — needs.
    ///
    /// `#[derive(Dependency)]` computes this from the fields. A manual impl
    /// that uses `ctx.provider::<T>()` and leaves this empty loses the boot
    /// check; `moso check` warns about exactly that.
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    /// Contribute to the OpenAPI operation: the security scheme this dependency
    /// implies, and the 401/403 it can produce.
    ///
    /// Defaults to contributing nothing, which is right for a dependency that
    /// cannot fail and implies no authentication.
    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    /// Produce the value. Called at most once per request per type.
    fn resolve<'a>(ctx: &'a RequestCtx) -> impl Future<Output = Result<Self>> + Send + 'a;
}

/// Extracts a [`Dependency`], resolving it or reading it from the request cache.
///
/// ```
/// use moso::prelude::*;
/// # /// A user, as the API returns one.
/// # #[derive(Schema)] pub struct UserOut { /// Stable identifier.
/// #     pub id: u64 }
/// # /// Who the request acts as.
/// # #[derive(Clone, Debug)] pub struct CurrentUser { pub id: u64 }
/// # impl Dependency for CurrentUser {
/// #     async fn resolve(_: &RequestCtx) -> Result<Self> { Ok(CurrentUser { id: 1 }) }
/// # }
/// /// Who am I?
/// #[endpoint]
/// async fn me(Depends(user): Depends<CurrentUser>) -> Result<Json<UserOut>> {
///     Ok(Json(UserOut { id: user.id }))
/// }
/// # fn main() { assert_eq!(Router::new().get("/me", moso::ep!(me)).len(), 1); }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Depends<T: Dependency>(pub T);

impl<T: Dependency> Depends<T> {
    /// The resolved value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: Dependency> Deref for Depends<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Dependency> Extract for Depends<T> {
    const PROVIDER_REQ: &'static [ProviderReq] = <T as Dependency>::PROVIDER_REQ;

    fn describe(op: &mut OperationBuilder) {
        <T as Dependency>::describe(op);
    }

    async fn extract(_parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        Ok(Depends(ctx.depends::<T>().await?))
    }
}

/// The longest type name a diagnostic will print in full.
///
/// Past this, [`short_type_name`] drops the module path — a 300-character
/// `impl` path in an error message is noise, and the last segment is what the
/// reader is looking for.
const MAX_TYPE_NAME: usize = 80;

/// A type name short enough to read, keeping the generic arguments.
///
/// `alloc::string::String` stays as it is; a name past [`MAX_TYPE_NAME`] loses
/// everything up to the last `::` that precedes its generic arguments, so
/// `shop::search::client::SearchClient<shop::search::PgBackend>` becomes
/// `SearchClient<shop::search::PgBackend>` rather than being truncated
/// mid-token.
fn short_type_name(name: &str) -> &str {
    if name.len() <= MAX_TYPE_NAME {
        return name;
    }
    let head = name.find('<').unwrap_or(name.len());
    match name[..head].rfind("::") {
        Some(index) => &name[index + 2..],
        None => name,
    }
}

/// The runtime error a missing provider produces.
///
/// Only reachable when a hand-written `Dependency` or `Extract` impl reads a
/// provider it did not declare in `PROVIDER_REQ`, so the message says exactly
/// that rather than blaming the application author.
///
/// `Inject<T>` cannot reach it in a normally-built application: its
/// `PROVIDER_REQ` made `AppBuilder::build` refuse to produce an `App` while the
/// provider was missing. The two ways here are an undeclared
/// `PROVIDER_REQ` and `AppBuilder::build_unchecked`, and the message names
/// both, because "which of my assumptions was wrong" is the only question the
/// reader has.
pub fn missing_provider_error(type_name: &'static str) -> Error {
    let short = short_type_name(type_name);
    Error::internal_msg(format!(
        "no provider is registered for `{short}`.\n\
         This is only reachable when a hand-written `Extract` or `Dependency` impl reads a \
         provider it did not declare, or when the application was built with \
         `AppBuilder::build_unchecked`.\n\
         help: declare it, so that `App::build()` reports this at boot instead of at 3am:\n    \
         const PROVIDER_REQ: &'static [ProviderReq] = &[ProviderReq::of::<{short}>()];\n\
         help: and register the value on the builder:\n    .provide(/* a {short} */)"
    ))
}

// ---------------------------------------------------------------------------
// Test-only overrides
// ---------------------------------------------------------------------------

/// A test's replacement for one [`Dependency`]'s `resolve`.
///
/// Boxed and `Arc`-shared because the registry is type-erased and one request
/// may resolve the same dependency more than once (the result is still
/// memoised; see [`RequestCtx::depends`](crate::ctx::RequestCtx::depends)).
#[cfg(any(test, feature = "test"))]
pub type DependencyOverrideFn<D> =
    Arc<dyn Fn(RequestCtx) -> crate::BoxFuture<'static, Result<D>> + Send + Sync>;

/// The `dependency_overrides` table, modelled on FastAPI's.
///
/// ```
/// use moso::prelude::*;
/// use moso_core::di::DependencyOverrides;
/// # /// Who the request acts as.
/// # #[derive(Clone, Debug, PartialEq)] pub struct CurrentUser { pub is_admin: bool }
/// # impl Dependency for CurrentUser {
/// #     async fn resolve(_: &RequestCtx) -> Result<Self> { Ok(CurrentUser { is_admin: false }) }
/// # }
/// # /// Nobody at all.
/// # #[derive(Clone, Debug, PartialEq)] pub struct Anonymous;
/// # impl Dependency for Anonymous {
/// #     async fn resolve(_: &RequestCtx) -> Result<Self> { Ok(Anonymous) }
/// # }
/// # fn main() {
/// let mut overrides = DependencyOverrides::new();
/// // The closure takes the `RequestCtx` and returns a future, so a fixture can
/// // still read the request it is standing in for.
/// overrides.insert::<CurrentUser, _, _>(|_ctx| async { Ok(CurrentUser { is_admin: true }) });
///
/// assert!(overrides.contains::<CurrentUser>());
/// assert!(overrides.get::<CurrentUser>().is_some());
/// // Anything not overridden still resolves the real way.
/// assert!(!overrides.contains::<Anonymous>());
/// assert_eq!(overrides.len(), 1);
/// # }
/// ```
///
/// A test reaches this through `TestApp`'s `override_dependency` rather than
/// building one by hand.
///
/// # How this is wired
///
/// The table is itself a **provider**, registered under
/// `TypeId::of::<DependencyOverrides>()` like any other. `AppBuilder`'s
/// `override_dependency` therefore needs no new field: it takes the existing
/// table out of the provider map (or makes an empty one), calls
/// [`insert`](DependencyOverrides::insert), and puts it back with
/// [`ProviderMapBuilder::insert`]. [`RequestCtx::depends`] consults it before
/// calling `D::resolve`.
///
/// `override_provider` needs nothing at all here: registration is last-write-
/// wins, so re-registering the type over the real one is the whole feature.
///
/// # Why it is compiled out
///
/// The whole type sits behind `#[cfg(any(test, feature = "test"))]`, so a
/// release build contains neither the table nor the lookup that reads it — a
/// production application cannot have a dependency silently replaced, and the
/// `depends` fast path has nothing extra in it.
#[cfg(any(test, feature = "test"))]
#[derive(Default)]
pub struct DependencyOverrides {
    entries: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

#[cfg(any(test, feature = "test"))]
impl DependencyOverrides {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace `D`'s resolution with `f`. Last write wins.
    pub fn insert<D, F, Fut>(&mut self, f: F) -> &mut Self
    where
        D: Dependency,
        F: Fn(RequestCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<D>> + Send + 'static,
    {
        let erased: DependencyOverrideFn<D> = Arc::new(move |ctx| Box::pin(f(ctx)));
        self.entries.insert(TypeId::of::<D>(), Arc::new(erased));
        self
    }

    /// The override registered for `D`, if any.
    pub fn get<D: Dependency>(&self) -> Option<DependencyOverrideFn<D>> {
        let entry = self.entries.get(&TypeId::of::<D>())?;
        (**entry).downcast_ref::<DependencyOverrideFn<D>>().cloned()
    }

    /// Whether `D` is overridden.
    pub fn contains<D: Dependency>(&self) -> bool {
        self.entries.contains_key(&TypeId::of::<D>())
    }

    /// How many dependencies are overridden.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is overridden.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(any(test, feature = "test"))]
impl core::fmt::Debug for DependencyOverrides {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DependencyOverrides")
            .field("overridden", &self.entries.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_req_is_const_evaluable() {
        const REQS: &[ProviderReq] = &[ProviderReq::of::<String>()];
        assert_eq!(REQS.len(), 1);
        assert_eq!(REQS[0].id(), TypeId::of::<String>());
        assert!(!REQS[0].optional);
    }

    #[test]
    fn provider_req_names_the_full_path() {
        let req = ProviderReq::of::<std::collections::HashMap<String, u32>>();
        assert!(req.name().contains("HashMap"));
    }

    #[test]
    fn optional_requirements_are_distinguishable() {
        assert_ne!(ProviderReq::of::<u8>(), ProviderReq::optional_of::<u8>());
    }

    // ── the provider map ──────────────────────────────────────────────────

    /// A stand-in for `Db`: something with identity, so a test can prove the
    /// map handed back *the* value rather than a copy of it.
    #[derive(Debug, PartialEq, Eq)]
    struct Pool(u32);

    trait Mailer: Send + Sync {
        fn name(&self) -> &'static str;
    }

    struct SmtpMailer;
    impl Mailer for SmtpMailer {
        fn name(&self) -> &'static str {
            "smtp"
        }
    }

    struct CapturingMailer;
    impl Mailer for CapturingMailer {
        fn name(&self) -> &'static str {
            "capturing"
        }
    }

    fn map_with_a_pool() -> (Arc<ProviderMap>, Arc<Pool>) {
        let pool = Arc::new(Pool(7));
        let mut builder = ProviderMapBuilder::new();
        builder.insert_arc(Arc::clone(&pool));
        (builder.build(), pool)
    }

    #[test]
    fn a_registered_value_comes_back_by_type() {
        let mut builder = ProviderMapBuilder::new();
        builder.insert(Pool(1));
        let map = builder.build();

        assert_eq!(*map.get::<Pool>().expect("registered"), Pool(1));
        assert!(map.contains::<Pool>());
        assert_eq!(map.len(), 1);
        assert!(!map.is_empty());
    }

    #[test]
    fn an_unregistered_type_is_none_rather_than_a_panic() {
        let map = ProviderMap::new();
        assert!(map.get::<Pool>().is_none());
        assert!(!map.contains::<Pool>());
        assert!(map.is_empty());
        assert!(!map.contains_req(&ProviderReq::of::<Pool>()));
    }

    #[test]
    fn a_trait_object_is_keyed_by_the_trait_not_the_concrete_type() {
        let mut builder = ProviderMapBuilder::new();
        builder.insert_dyn::<dyn Mailer>(Arc::new(SmtpMailer));
        let map = builder.build();

        assert_eq!(map.get::<dyn Mailer>().expect("registered").name(), "smtp");
        // Registering `dyn Mailer` deliberately does NOT register `SmtpMailer`:
        // depending on the concrete type is what the trait object prevents.
        assert!(map.get::<SmtpMailer>().is_none());
    }

    #[test]
    fn a_later_registration_replaces_an_earlier_one() {
        // This is `override_provider` in full: register the real mailer, then
        // register the fake over it.
        let mut builder = ProviderMapBuilder::new();
        builder.insert_dyn::<dyn Mailer>(Arc::new(SmtpMailer));
        builder.insert_dyn::<dyn Mailer>(Arc::new(CapturingMailer));
        let map = builder.build();

        assert_eq!(
            map.get::<dyn Mailer>().expect("registered").name(),
            "capturing"
        );
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn sized_and_unsized_providers_share_one_map() {
        let mut builder = ProviderMapBuilder::new();
        builder
            .insert(Pool(3))
            .insert_dyn::<dyn Mailer>(Arc::new(SmtpMailer));
        let map = builder.build();

        assert_eq!(map.get::<Pool>().expect("pool").0, 3);
        assert_eq!(map.get::<dyn Mailer>().expect("mailer").name(), "smtp");
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn the_builder_exposes_the_map_it_has_built_so_far() {
        let mut builder = ProviderMapBuilder::new();
        assert!(builder.as_map().is_empty());
        builder.insert(Pool(1));
        assert!(builder.as_map().contains::<Pool>());
    }

    #[test]
    fn registered_names_are_sorted_for_the_boot_report() {
        let mut builder = ProviderMapBuilder::new();
        builder.insert(Pool(1)).insert(String::new()).insert(9u8);
        let map = builder.build();

        let names = map.registered_names();
        assert_eq!(names.len(), 3);
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert!(names.iter().any(|name| name.ends_with("Pool")));
    }

    // ── the shape of a lookup ─────────────────────────────────────────────

    #[test]
    fn lookup_returns_the_registered_allocation_rather_than_a_copy() {
        let (map, pool) = map_with_a_pool();

        let first = map.get::<Pool>().expect("registered");
        let second = map.get::<Pool>().expect("registered");

        // Same allocation as the one handed to the builder, and as each other:
        // the map cloned an `Arc`, it did not clone a `Pool`.
        assert!(Arc::ptr_eq(&pool, &first));
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn lookup_costs_exactly_one_refcount_increment() {
        let (map, pool) = map_with_a_pool();
        let before = Arc::strong_count(&pool);

        let handle = map.get::<Pool>().expect("registered");
        assert_eq!(Arc::strong_count(&pool), before + 1);

        drop(handle);
        assert_eq!(Arc::strong_count(&pool), before);
    }

    /// A smoke bound, not a benchmark.
    ///
    /// The rigorous claims — that a lookup copies nothing and costs exactly one
    /// refcount increment — are asserted deterministically by the two tests
    /// above. This one guards the remaining shape of the sentence, that a
    /// lookup is *bounded work*: no I/O, no lock, no linear scan over the
    /// providers. The documented cost is tens of nanoseconds; the bound is two
    /// orders of magnitude looser so that a debug build on a loaded CI box with
    /// the whole suite running in parallel still passes.
    #[test]
    fn lookup_stays_within_the_documented_order_of_magnitude() {
        const ITERATIONS: u32 = 100_000;

        let (map, _pool) = map_with_a_pool();
        // Warm the branch predictor and the cache line before timing.
        for _ in 0..1_000 {
            std::hint::black_box(map.get::<Pool>());
        }

        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(map.get::<Pool>());
        }
        let per_lookup = start.elapsed() / ITERATIONS;

        assert!(
            per_lookup < std::time::Duration::from_micros(5),
            "provider lookup took {per_lookup:?} each, which is far past a hash plus a downcast"
        );
    }

    // ── diagnostics ───────────────────────────────────────────────────────

    #[test]
    fn short_names_are_left_alone() {
        assert_eq!(short_type_name("shop::db::Db"), "shop::db::Db");
    }

    #[test]
    fn long_names_lose_the_module_path_but_keep_the_arguments() {
        let long = "shop::search::client::really::deeply::nested::SearchClient<\
                    shop::search::PgBackend>";
        assert!(long.len() > MAX_TYPE_NAME);
        assert_eq!(
            short_type_name(long),
            "SearchClient<shop::search::PgBackend>"
        );
    }

    #[test]
    fn a_long_name_without_a_path_is_returned_unchanged() {
        let long = "A".repeat(MAX_TYPE_NAME + 1);
        assert_eq!(short_type_name(&long), long);
    }

    // ── test-only overrides ───────────────────────────────────────────────

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct CurrentUser(&'static str);

    impl Dependency for CurrentUser {
        async fn resolve(_ctx: &RequestCtx) -> Result<Self> {
            Ok(CurrentUser("from the database"))
        }
    }

    #[derive(Clone)]
    struct Tenant;

    impl Dependency for Tenant {
        async fn resolve(_ctx: &RequestCtx) -> Result<Self> {
            Ok(Tenant)
        }
    }

    #[test]
    fn an_override_is_stored_and_read_back_by_dependency_type() {
        let mut overrides = DependencyOverrides::new();
        assert!(overrides.is_empty());

        overrides.insert::<CurrentUser, _, _>(|_| async { Ok(CurrentUser("fixture")) });

        assert_eq!(overrides.len(), 1);
        assert!(overrides.contains::<CurrentUser>());
        assert!(overrides.get::<CurrentUser>().is_some());
        // A different dependency is untouched.
        assert!(!overrides.contains::<Tenant>());
        assert!(overrides.get::<Tenant>().is_none());
    }

    #[test]
    fn overrides_travel_through_the_provider_map() {
        // This is exactly how `AppBuilder::override_dependency` wires it, and
        // how `RequestCtx::depends` finds it again.
        let mut overrides = DependencyOverrides::new();
        overrides.insert::<CurrentUser, _, _>(|_| async { Ok(CurrentUser("fixture")) });

        let mut builder = ProviderMapBuilder::new();
        builder.insert(overrides);
        let map = builder.build();

        let table = map.get::<DependencyOverrides>().expect("registered");
        assert!(table.get::<CurrentUser>().is_some());
    }
}
