# ADR-0002 — Own the handler and extractor traits

Status: Accepted
Date: 2026-07-29

## Context

Having chosen Axum as the engine (ADR-0001), the question is whether to reuse `axum::Handler`,
`FromRequestParts` and `FromRequest` directly, or to define Moso equivalents.

Three forces push toward owning them:

1. **OpenAPI.** An extractor must be able to describe its contribution to the API contract. Axum's
   traits have nowhere to put that, which is exactly why `utoipa` needs a per-handler annotation
   that drifts from the handler signature.
2. **Diagnostics.** `the trait bound ...: Handler<_, _, _> is not satisfied` is the single
   most-complained-about error in the ecosystem. We cannot own the message without owning the trait.
3. **Dependency injection.** `Inject`/`Depends` need per-request memoisation and a boot-time
   provider requirement list. `State`/`FromRef` express neither.

## Decision

Define `moso_core::{Handler, Extract, ExtractBody, Describe}` as strict supersets of Axum's traits:
the same extraction method, plus `describe(&mut OperationBuilder)` and a `PROVIDER_REQ` const.

Provide blanket impls in both directions so the ecosystems interoperate: every Moso extractor is an
Axum extractor, and `Opaque<T>` lifts any Axum extractor into a Moso handler (contributing nothing
to the documentation, which is the honest behaviour).

Re-export Axum's `IntoResponse` rather than defining a parallel response trait — there is no
OpenAPI or DI reason to own it, and sharing it means the entire ecosystem's response types work.

## Alternatives considered

- **Reuse Axum's traits, bolt OpenAPI on externally.** This is the `utoipa` model. Rejected: the
  annotation-drift problem is the specific thing we exist to solve.
- **Reuse Axum's traits, derive OpenAPI from a build-time source analysis.** Fragile, needs a
  non-standard build step, and cannot see runtime router composition.
- **Own everything including `IntoResponse`.** Rejected: no benefit, and it would fragment
  compatibility with `tower-http` and `axum-extra`.

## Consequences

- More traits to document, and a real risk of user confusion about which trait applies. Mitigated by
  `on_unimplemented` messages that name the built-in extractors and the `Opaque` escape hatch.
- The blanket impls must carry `#[diagnostic::do_not_recommend]` so they do not pollute error output.
- We control the diagnostic quality of the most common failure modes, which is the point.

## Reversal criteria

- If a future Axum version gains a description hook and a DI model that meet our needs, reconsider —
  though by then our traits would be load-bearing in user code and the migration cost would be high.

## Implementation note (2026-07-29, WP-03/WP-04)

Two claims in the *Decision* section above did not survive contact with the compiler. The decision
itself stands; these are corrections to how it is realised.

1. **There is no blanket `impl<T: Extract> ExtractBody for T`.** It conflicts under coherence with
   `impl<T: Schema> ExtractBody for Json<T>`, and it would make the `PartsOnly`/`WithBody` marker
   ambiguous for every handler whose last parameter is a parts extractor. The
   "at most one body extractor, and it must be last" rule is enforced by `#[endpoint]` instead —
   where the error message is hand-written, which is better than anything trait resolution produces.
   See `01-http/12-extractors-responses.md § The traits`.

2. **There is no blanket `impl<T: Extract> axum::extract::FromRequestParts<()> for T`.** It is an
   orphan-rule violation (E0210): `T` is an uncovered type parameter in an impl of a foreign trait.
   Moso → Axum interop is therefore by wrapper — `MosoExt<T>` for parts extractors and
   `MosoExtBody<T>` for body extractors — both of which read the `RequestCtx` the handler adapter
   places in the request extensions. Axum → Moso is `Opaque<T>` / `OpaqueBody<T>` as written above.

A third correction belongs to ADR-0013: `Handler`'s associated `Endpoint` type cannot be attached to
a plain `fn` item, so `#[endpoint]` emits a companion unit struct and `routes!`/`ep!` name it.
