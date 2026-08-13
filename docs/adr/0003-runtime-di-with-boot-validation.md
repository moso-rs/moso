# ADR-0003 - Runtime DI with boot-time validation, not compile-time codegen

Status: Accepted
Date: 2026-07-29

## Context

Pavex demonstrates compile-time dependency injection in Rust: a `Blueprint` is analysed by a code
generation step that emits plain Rust with no runtime lookup and produces framework-level
diagnostics rather than raw trait-resolution errors. The diagnostics are genuinely better than
anything achievable with trait bounds alone.

The cost is a non-standard build: `pavex build` sits between the source and `cargo build`.

## Decision

Use a **runtime type-map for providers, validated exhaustively at `App::build()`**.

Each `#[endpoint]` emits the list of `ProviderReq` its parameters require. `build()` checks every
registered operation against the provider map and reports **all** missing providers at once, with
the route, the file:line, and a concrete fix.

## Alternatives considered

- **Pavex-style codegen.** Rejected for M1 on three grounds:
  1. A non-standard build step breaks `cargo build`, `cargo install`, rust-analyzer's default
     behaviour, and every CI template. That is a large tax on the first-five-minutes loop, which is
     our primary optimisation target.
  2. The runtime cost avoided is ~15 ns of hash + downcast, against a request that will spend
     milliseconds in Postgres.
  3. Boot-time validation captures the great majority of the diagnostic value at a fraction of the
     complexity: the user still learns about a missing provider before serving traffic, in prose.
- **Axum-style `State` + `FromRef`.** Rejected: produces trait-resolution errors, forces `Router<S>`
  generics (a monomorphisation cost - see `04-devex/42` rule A1), and cannot express request-scoped
  memoisation.
- **No DI at all; pass a context struct.** Honest and simple, but it makes every handler signature
  churn when a dependency is added, and it cannot express request-scoped values like `CurrentUser`.

## Consequences

- A ~15 ns per-lookup cost, benchmarked with a 200 ns total DI budget per request.
- Boot-time validation depends on `PROVIDER_REQ` being declared correctly. Derives compute it;
  hand-written `Dependency` impls must declare it, and `moso check` warns when one references
  `ctx.provider` with an empty `PROVIDER_REQ`.
- We keep the door open: `ProviderReq` is deliberately const-evaluable, so a future `moso build`
  could perform the same analysis ahead of time with no change to user-facing API.

## Reversal criteria

- Users report that boot-time errors are insufficient - i.e. real production incidents caused by a
  DI problem that a compile-time analysis would have caught.
- Measured DI overhead becomes material (> 2% of request time) in a realistic workload.
- Pavex's reception demonstrates that Rust developers accept an extra build step in exchange for
  diagnostics. This is a genuine open question and Pavex is the experiment; watch it.
