# ADR-0013 — Handler registration: a companion type, `routes!` and `ep!`

Status: Accepted
Date: 2026-07-29
Deciders: core team
Supersedes nothing. Amends the registration story sketched in `01-http/11-routing.md` and
`06-reference/62-macro-reference.md`.

## Context

ADR-0002 decided that Moso owns `Handler`, and that `Handler<M>` carries an associated
`type Endpoint: Endpoint` — the compile-time description an operation contributes to the OpenAPI
document and to the boot-time provider check. `01-http/11-routing.md` then wrote:

> Method shorthands (`Router::get(path, handler)`) are the ergonomic default and are **all most
> users ever need**: the handler's `#[endpoint]` metadata is picked up through the `Handler` trait's
> associated `Endpoint` type.

While building WP-04 this turned out to be **not implementable in Rust**, and the discovery is
load-bearing enough to deserve its own record.

`#[endpoint]` is an attribute macro over an `async fn`. Its output must let

```rust
Router::new().get("/users", list)
```

recover the metadata `#[endpoint]` computed for `list`. That requires attaching an associated type
to `list`. But `list` is a **`fn` item**, and:

1. A `fn` item's type is anonymous and unnameable. There is no path a user or a macro can write for
   it, so `impl Handler<M> for typeof(list)` cannot be spelled.
2. `Handler<M>` is therefore only reachable for a `fn` item through a **blanket impl** over function
   pointers/`Fn` traits — `impl<F, Fut, T1, …> Handler<(PartsOnly, T1, …)> for F where F: Fn(T1, …)
   -> Fut`. A blanket impl is written once, for *all* functions, so its `type Endpoint` is one fixed
   type. It cannot vary per function, which is exactly what carrying per-handler metadata means.
3. Rust has no `#[attribute]`-visible way to add an inherent associated item to an existing `fn`
   item, and no specialisation that would let one blanket impl be refined per function.

So the metadata has to live on a **type**, and the macro has to create that type. The remaining
question was which shape that takes at the call site.

## Decision

`#[endpoint]` **leaves the `async fn` exactly as written** and additionally emits a companion unit
struct beside it:

```rust
#[doc(hidden)]
#[allow(non_camel_case_types, non_snake_case, unreachable_pub, dead_code)]
#[derive(Clone, Copy, Default)]
pub struct __moso_op_create;

impl ::moso::__private::Endpoint  for __moso_op_create { /* summary, params, responses, providers */ }
impl ::moso::__private::HandlerFn for __moso_op_create { /* one concrete extraction glue fn */ }
```

Two macros hide that name, and both resolve it through the same function
(`moso_macros::routes::op_ident`), so they can never disagree:

- **`moso::routes! { GET "/users" => list, … }`** is the primary registration API. It rewrites the
  **last segment only** of each handler path — `list` → `__moso_op_list`, `users::list` →
  `users::__moso_op_list` — and expands to the equivalent builder chain:

  ```rust
  ::moso::__private::Router::new()
      .endpoint::<__moso_op_list>(
          ::moso::__private::HttpMethod::Get,
          ::moso::__private::route_path!("/users"),
      )
  ```

- **`moso::ep!(list)`** is a one-token proc macro expanding to `__moso_op_list`. Because the
  companion type is a unit struct, its path is also an *expression*, so the builder chain works
  unchanged: `Router::new().get("/users", ep!(list))`.

`Router::endpoint::<E: Endpoint + HandlerFn + Default>(method, path)` is the explicit generic form
both macros lower to.

Three `Handler` families exist, with no overlap:

| Written | `M` | `Handler::Endpoint` | Documents itself |
| --- | --- | --- | --- |
| `routes! { GET "/u" => list }` | `EndpointMarker` | `__moso_op_list` | fully |
| `Router::get("/u", ep!(list))` | `EndpointMarker` | `__moso_op_list` | fully |
| `Router::get("/u", list)`, plain `async fn` | `(PartsOnly, T1..Tn)` / `(WithBody, …)` | `UndocumentedEndpoint` | not at all |

The third family is kept deliberately: a handler without `#[endpoint]` still compiles and still
serves. What it loses is the OpenAPI operation and the boot-time provider check, and it says so —
`UndocumentedEndpoint::spec` writes `x-moso-undocumented: true` into the operation, which
`moso routes` renders as `<undocumented>`.

## Alternatives considered

### A. The unit struct *replaces* the function

`#[endpoint] async fn create(..)` expands to `pub struct create;` plus an `impl` holding the body,
so `Router::get("/users", create)` names the struct directly and no second macro is needed.

Rejected. It costs more than it saves:

- **The function stops being callable.** `create(a, b, c).await` from a test, from another handler,
  or from a `#[job]` no longer compiles. Handlers are ordinary functions in every framework users
  are coming from, and making them not-functions is a large, invisible behaviour change.
- **`cargo expand` stops matching what was written.** Rule 1 of `00-foundations/01` is "no magic
  that cannot be printed"; a macro that deletes the item it was attached to prints badly.
- **Every error inside the body moves.** Spans inside a re-emitted `impl` block are worse than spans
  in an untouched `fn`, and the body is where most compile errors actually are.
- **Name collisions become likely.** A `struct create` in scope collides with anything else called
  `create`, and the collision message names a type the user never wrote.
- It also breaks `#[deprecated]`, `#[cfg]`, `#[instrument]` and every other attribute users stack on
  handlers, because they would now apply to a struct.

### B. `routes!` only — no `ep!`, no method shorthands for documented handlers

Make the table the *only* way to register a documented endpoint, and let the builder chain accept
plain functions (undocumented) alone.

Rejected. The table is genuinely better at ten routes and genuinely worse at one; a health check or
a single webhook receiver should not require a table. More importantly, the builder chain is the
shape every Axum user already knows, and closing it off for the documented case would mean the
familiar spelling silently produced the *worse* result — an undocumented route — with nothing at the
call site to signal it. `ep!` is three extra tokens and keeps both spellings first-class and
equivalent.

### C. `ep!` only — no `routes!`

Rejected for the mirror-image reason. `moso generate resource` regenerates a route table
deterministically, a table diffs cleanly in review, and acceptance criterion 5 of
`01-http/11-routing.md` — "`routes!` and the builder chain produce byte-identical OpenAPI documents"
— is trivially satisfied when one *is* the other.

### D. A link-time registry (`inventory` / `ctor`)

`#[endpoint]` registers the operation into a global collected at link time; the router looks it up by
name. Rejected by ADR-0004 on independent grounds (breaks in static libs, wasm and tests;
non-deterministic ordering; invisible to `cargo expand`). Nothing here changes that.

### E. Put the metadata in a `const` beside the function and look it up by name

`#[endpoint]` emits `const __MOSO_SPEC_create: OperationSpec = …` and `routes!` names it. This is
what was chosen, minus the trait — and losing the trait loses the thing that makes it work:
`Router::endpoint::<E>` needs `E::spec`, `E::required_providers` **and** `E::invoke` together, and a
trait is how three associated items travel as one. It would also have forced `OperationSpec` to be
`const`-constructible, which it is not (it holds `String`s and an `IndexMap`).

## Consequences

- **Two spellings, one meaning.** `routes!` and `ep!` reach the same `Handler<EndpointMarker>` impl
  and produce the same document. This is asserted by a unit test
  (`routes::tests::ep_and_routes_agree_on_the_name`) rather than by convention.
- **A generated name leaks into diagnostics.** A user who mistypes a handler name in a table sees
  `cannot find type __moso_op_lst in this scope`. Mitigated by `routes!` spanning the rewritten
  identifier at the *user's* token, so the underline is on `lst`, and by `#[endpoint]` emitting a
  well-typed placeholder companion type even when the handler itself failed to expand — so one
  mistake yields one error.
- **`#[endpoint]` must emit `#[derive(Clone, Copy, Default)]`.** `Handler: Clone` and
  `Router::endpoint::<E>` additionally requires `E: Default`. This is now part of the macro
  contract, not an implementation detail.
- **`ep!` must reject a whole route.** `ep!(GET "/healthz" => healthz)` is the predictable mistake,
  so it is detected and answered with `help: write Router::new().get("/healthz", ep!(healthz))`
  rather than "expected a handler name".
- **Registering a plain `async fn` is legal and lossy.** Documented in `UndocumentedEndpoint`, in
  `01-http/11-routing.md`, and surfaced by `moso routes`. The alternative — refusing to compile —
  would make the first five minutes with Moso worse for no correctness gain.
- **`Router::get(path, handler)` is no longer described as "all most users ever need"**; the docs
  now lead with `routes!`. `01-http/11-routing.md` and `06-reference/62-macro-reference.md` were
  corrected in the same change that introduced this record.

## Reversal criteria

- If Rust gains a way to attach an associated item to a `fn` item — inherent associated types on
  function items, or an attribute macro that can extend an existing item's impl surface — the
  companion type becomes unnecessary and `Router::get("/users", list)` could carry full metadata.
  Revisit then; `routes!` and `ep!` would remain as sugar and the migration would be additive.
- If specialisation lands in a form that lets a blanket `Handler` impl be refined per function type,
  the same applies.
- If user testing shows the `__moso_op_*` name leaking into more than a small minority of first-week
  compile errors, reconsider alternative A despite its costs — but measure first, because A's costs
  are certain and this one's is not.
