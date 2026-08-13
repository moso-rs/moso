# ADR-0006 - Assemble the OpenAPI document at boot, not at build

Status: Accepted
Date: 2026-07-29

## Context

The OpenAPI document could be produced at compile time (a build script or a codegen step), or
assembled at application startup from compile-time-constructed pieces.

## Decision

**Content is compile-time; assembly is boot-time.** `#[endpoint]` produces an `OperationSpec` at
compile time from the handler's types and doc comments. `App::build()` walks the composed router and
merges those specs into a `Document`, deduplicating schemas into `$defs`. The result is serialised
once and served from a cached byte slice with an ETag.

## Alternatives considered

- **Build-script generation.** Cannot see the router composition, which is ordinary Rust code
  (`nest`, `merge`, conditional mounting). It would require a second, parallel description of the
  routing, which is the drift problem we exist to eliminate.
- **A separate `moso openapi` binary that links the app.** Works, but it makes the document a build
  artefact rather than something you can inspect from a running server, and it does not remove the
  need for boot-time assembly when serving `/docs`.

## Consequences

- A one-time boot cost: budget < 15 ms for 200 endpoints, measured in `examples/bench`.
- The document reflects *exactly* what the running process serves, including conditionally mounted
  routes. That is a correctness advantage over any build-time approach.
- Determinism is required so that the committed `openapi.json` diffs cleanly: `IndexMap` everywhere,
  stable schema ordering, stable `$defs` naming.
- Drift detection is a CI check (`moso openapi check`) comparing the committed file against the
  assembled one.

## Reversal criteria

- Boot cost exceeds 50 ms for a realistic application and users complain about cold-start latency
  (relevant for serverless deployment). The mitigation would be to cache the serialised document as
  a build artefact and validate it at boot rather than rebuild it.
