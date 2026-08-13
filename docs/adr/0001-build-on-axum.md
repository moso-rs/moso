# ADR-0001 - Build on Axum/Tower/Tokio rather than a new HTTP core

Status: Accepted
Date: 2026-07-29

## Context

Moso needs an HTTP core: routing, connection handling, HTTP/1 and HTTP/2, middleware. The options
are to write one, or to build on an existing one.

The substrate question in the Rust ecosystem is settled. Axum has roughly a 5:1 all-time download
lead over actix-web, is the default community recommendation, is maintained by the Tokio team, and
carries the `tower`/`tower-http` middleware ecosystem with it. `async-std`'s discontinuation
removed the last competing runtime story, and with it Tide's foundation.

## Decision

Build on **Axum + Tower + Hyper + Tokio**. Axum's `Router`, `axum::serve`, `IntoResponse`, WebSocket
upgrade, and the `tower-http` ecosystem are used directly. Moso is a layer above, not a replacement.

Every Moso abstraction exposes the layer beneath: `Router::into_axum()`, `Router::mount_axum()`,
`App::into_service()`, `Opaque<T>` for foreign extractors.

## Alternatives considered

- **Write a new core on hyper.** Full control over ergonomics and diagnostics. Rejected: months of
  work on a solved problem, no middleware ecosystem, and it strands users who want `tower-http`.
- **Build on actix-web.** Smaller ecosystem, actor-model conceptual overhang, and a reputational
  scar from the 2020 maintainer crisis that our target audience remembers.
- **Runtime-agnostic core.** Doubles the surface for approximately zero users. See ADR-0011.

## Consequences

- We inherit Axum's release cadence and its breaking changes. The 0.6→0.7 forced hyper-1.0 upgrade
  and the 0.8 path-syntax change are live memories for our users; we mitigate by re-exporting Axum
  under `moso::deps` and treating its major version as part of Moso's semver contract
  (`05-delivery/53`).
- We inherit its performance characteristics, which are excellent.
- We must maintain interop shims in both directions, and test them.
- We cannot fix Axum's ergonomics by changing Axum; we fix them by owning the traits users touch
  (ADR-0002).

## Reversal criteria

- Axum's maintenance stalls for two consecutive quarters with unaddressed security issues.
- A breaking change lands that we cannot shim without leaking into our public API.
- Our interop layer's cost (measured overhead or maintenance burden) exceeds the cost of a bespoke
  core. This is not plausible today and we would need real numbers.
