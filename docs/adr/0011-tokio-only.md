# ADR-0011 - Tokio only; no runtime abstraction

Status: Accepted
Date: 2026-07-29

## Context

Some Rust libraries abstract over the async runtime so they work with Tokio, `smol`, or others. This
costs a trait layer, feature-flag combinatorics, and a permanently doubled test matrix.

`async-std` was officially discontinued in February 2025 in favour of `smol`, which removed the last
mainstream alternative and took Tide's foundation with it. The ecosystem - hyper, sqlx, tower,
reqwest, and Axum itself - is Tokio-centric in practice.

## Decision

**Tokio only.** No runtime abstraction layer, no feature flags for alternative runtimes.
`tokio::spawn`, `tokio::time`, `tokio::net`, and `tokio::task::spawn_blocking` are used directly.

`moso::task::blocking()` wraps `spawn_blocking` with a bounded pool and a tracing span, and is
public - because the most common async footgun for FastAPI refugees is calling blocking code in an
async function.

## Alternatives considered

- **Runtime-agnostic via a trait.** Doubles the surface, complicates every timeout and every
  spawn, and serves approximately zero of our target users.
- **Feature-flagged runtime selection.** All the cost of abstraction plus a combinatorial test
  matrix.

## Consequences

- Moso cannot be used in a `smol`-based application without a compatibility shim, which we do not
  provide. Documented up front.
- We use Tokio's full capability set without hedging: `JoinSet`, cancellation tokens, `tokio-console`
  integration, and the multi-threaded scheduler's work-stealing behaviour in our benchmarks.
- One fewer axis in the test matrix, which is a real ongoing saving.

## Reversal criteria

- A genuine shift in the ecosystem's runtime consensus. There is no current sign of one, and Tokio's
  position strengthened when `async-std` was discontinued.
