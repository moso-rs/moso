# ADR-0016 - Battery-mounted routes and the documentation / boot-check boundary

Status: Accepted
Date: 2026-08-12
Deciders: Alessandro Zucchiatti

## Context

`moso-auth` ships a mountable set of authentication routes - thirty-four of them with every flag on
(`03-batteries/30-auth.md`) - reached as `moso::auth::routes()…build()` and handed to `.mount()`.
They are real handlers with real behaviour, tested route by route, and their request and response
DTOs are real `Schema` types with hand-written impls that validate.

Two things about them are *worse* than an application's own `#[endpoint]` handlers, and both trace to
one fact: **`moso-auth` sits below the `moso` facade.** `#[endpoint]`, `routes!` and `ep!` all expand
to `::moso::__private::…` (the macro-output rule in `AGENTS.md`), and the facade is above this crate,
so none of the three is reachable from inside the battery. The mounted handlers are therefore plain
`async fn`s registered through `Router::post`/`get`/`delete`.

- **The document under-describes them (item 10.1).** A handler registered without `#[endpoint]`
  carries `moso_core::UndocumentedEndpoint`, whose `spec` stamps the operation `x-moso-undocumented`
  and contributes no request or response schema. So the bodies these routes speak are absent from the
  OpenAPI document even though the `Schema` types exist. What *is* documented is written by hand and
  true of every route in its group - the `auth` tag, the 429 on throttled routes, the 503 an
  unreachable store produces, the 401 on authenticated ones - because inventing per-route metadata to
  fill the gap would break the framework's own rule that a document must never carry
  plausible-looking metadata it cannot derive from the handler.

- **The boot check cannot see their dependencies (item 10.2).** `App::build()` walks every route's
  `Endpoint::required_providers()` and reports a forgotten `.provide(..)` as a boot error rather than
  a first-request 500 (`di.rs`, `validate_providers` in `app.rs`). `UndocumentedEndpoint::required_providers()`
  returns `&[]`. The mounted handlers each take `Inject<AuthState>`, but that requirement never
  reaches the route table, so a composition root that mounts the routes and forgets
  `.provide(AuthState…)` boots clean. `Inject::extract` then runs `ctx.provider::<AuthState>()?` on
  the first request, which returns `missing_provider_error` - a **500 naming `AuthState`**, not a
  panic and not a silent wrong answer, but a runtime failure where the framework's promise is a boot
  failure. The invariant "`Inject<T>` is infallible at the use site because boot proved the provider
  exists" holds for `#[endpoint]` routes; it is exactly this class of route where it does not.

`30-auth.md` flags 10.1/10.2 as deserving an ADR and names the three ways out: a
`moso-macros`-independent `#[endpoint]`, moving `routes` above the facade, or accepting the gap. This
record decides.

A third, smaller limitation rides along and is recorded here rather than in its own ADR because it is
the same decision seen from a different angle (item 10.3): **the mounted set is fixed to
`DefaultUser`.** `AuthRoutes::build` has no type parameter, so its handlers are one concrete
instantiation, and the account store is taken as `Arc<dyn AccountStore<User = DefaultUser>>`.
`Accounts<S>` holds an `Arc<S>` and `S` carries the implicit `Sized` bound, so
`Accounts<dyn AccountStore<..>>` cannot be named; the crate bridges the gap with a private
`ErasedAccountStore` newtype for `DefaultUser` only.

## Decision

**The mounted routes are honestly `x-moso-undocumented`, keep an empty `required_providers()`, and
are the *prototyping* tier. The documented, boot-checked tier is the copy-out: `moso new --auth`
copies the handlers into the application, where they become ordinary `#[endpoint]` handlers reached
through the facade.**

1. **Accept the documentation gap for the mounted tier.** The bodies stay `x-moso-undocumented`. No
   metadata is synthesised to close it. The hand-written group-level facts (tag, 429, 503, 401) are
   the honest floor and stay.

2. **Do not make `required_providers()` non-empty for the mounted set - it is not the cheap fix it
   looks like.** Two mechanisms could carry `AuthState` into the boot check, and both are refactors
   the size of an RFC, not a local correctness patch:
   - A `Router::requires::<T>()` builder that appends a `ProviderReq` to a route after registration.
     `RouteEntry::providers` is `&'static [ProviderReq]`, populated only from
     `Endpoint::required_providers()`; appending means changing that public field of a **floor crate**
     (`moso-core`) to an owned or `Cow` form and threading it through registration, `describe`, and
     `validate_providers`. That is a public-API change to `moso-core`, which is RFC-shaped in its own
     right.
   - A hand-written `Endpoint` + `HandlerFn` per mounted handler whose `required_providers()` names
     `AuthState`. That is re-implementing what `#[endpoint]` generates, by hand, for thirty-four
     routes - the "`moso-macros`-independent `#[endpoint]`" alternative, at full cost.

   Forcing either through to close a gap that the copy-out tier closes for free is the more dangerous
   change. The interim failure is a **first-request 500 that names `AuthState`**, which is loud,
   greppable, and already tells the operator the fix.

3. **The copy-out tier is the real answer, and it closes *both* holes structurally.** A handler copied
   into an application is written with `#[endpoint]` and reached through the facade, so its request
   and response types are documented and `Inject<AuthState>`'s `PROVIDER_REQ` flows into
   `required_providers()` - a forgotten provider becomes a boot error there with no extra machinery.
   The mounted tier exists for a one-line working login in a prototype and as the reference the copied
   code is generated from, so the two cannot drift.

4. **The `DefaultUser` fix is the copy-out, not a generic mounted set (item 10.3, recorded as a known
   limitation).** An application with its own `User` copies the handlers and names its own type in
   them. Making the mounted set generic would need `AuthRoutes::build<U>` and an `Accounts` that can
   hold an unsized store, which trades the tier split for a pile of type parameters on the very code
   that is meant to be read and copied. It is not built and is not owed as a separate line beyond the
   copy-out tier itself.

## Alternatives considered

- **Move `routes` above the facade** so `#[endpoint]` is reachable. Rejected: it inverts the layering
  (`moso-auth` is a battery *below* the facade; dependencies point downward only, and
  `xtask check-deps` enforces it), and it would put concrete route handlers in the facade crate. The
  gap is a symptom of correct layering, not of a misplaced crate.

- **Build a `moso-macros`-independent `#[endpoint]`** usable from inside a battery. This is the
  general fix and would let any battery mount fully-documented, boot-checked routes. Rejected *for
  now* because it duplicates the macro's expansion as hand-maintained code, and the copy-out tier
  removes the need: a copied handler already gets the macro. It is the natural thing to revisit if a
  second battery ever needs to mount documented routes - see reversal criteria.

- **Synthesise the schemas / hardcode a non-empty `required_providers()`** for the mounted routes.
  Rejected: the first half violates "never synthesise plausible-looking metadata"; the second still
  needs the `moso-core` plumbing of decision point 2 and buys only the mounted tier, which is being
  retired by the copy-out anyway.

- **Make the mounted set generic over `User`** to close 10.3 directly. Rejected as above: type
  parameters on the reference implementation, to serve a tier whose long-run answer is to be copied.

## Consequences

- A prototype built on `moso::auth::routes()` has an OpenAPI document that names the auth *operations*
  and their statuses but not their bodies; a client generator sees `unknown` request/response shapes
  for them. This is acceptable for the prototyping tier and disappears the moment the handlers are
  copied.
- Forgetting `.provide(AuthState…)` is a first-request 500, not a boot error, for the mounted tier
  only. The message names `AuthState`; the copied tier gets the boot error.
- `30-auth.md`'s "what is still owed" item 1 ("OpenAPI bodies for the mounted routes, **or an ADR
  accepting the gap**") is satisfied by this record. Item 2 (`moso new --auth`) is now load-bearing:
  it is the tier that closes the gap, not merely a convenience.
- The `DefaultUser` fixing of the mounted set is a documented limitation, not a bug: an application
  with a custom `User` uses the copy-out.
- No `moso-core` public surface changes and no per-route provider machinery is added, so the floor
  crate stays as small as it is.

## Reversal criteria

- **`moso new --auth` ships and the mounted tier is retired.** The documentation and boot-check gaps
  leave with it; this ADR then describes history, and its record of *why* the mounted tier was only a
  prototype survives as the reason the copy-out is the product.
- **A second battery needs to mount fully-documented, boot-checked routes.** At that point the
  `moso-macros`-independent operation-builder pays for itself across more than one caller; build it,
  adopt it in the mounted auth routes, and both gaps close for the mounted tier too. This ADR is
  superseded by the one that records that decision.
- **The first-request 500 proves to be a real production footgun** - measured by support incidents,
  not by aesthetics. Then promote the `Router::requires::<T>()` API of decision point 2 to an RFC (it
  is a `moso-core` public-API change and must be), give the mounted set a non-empty
  `required_providers()`, and turn the 500 into a boot error without waiting for the copy-out.
