# ADR-0004 — No link-time registration (`inventory` / `ctor`)

Status: Accepted
Date: 2026-07-29

## Context

Frameworks in other languages discover routes, jobs, and models by scanning modules at import time.
Rust's nearest equivalent is link-time collection via `inventory` or `ctor`: a macro registers an
item into a linker section, and the framework enumerates it at startup. It is tempting because it
would let `#[endpoint]` register a route with no explicit `.mount()` call.

## Decision

**No link-time registries anywhere in Moso.** Routes, jobs, permissions, and admin models are
registered explicitly in ordinary Rust code.

## Alternatives considered

- **`inventory`-based auto-registration.** Rejected for five reasons:
  1. It does not work reliably when the crate is a static library, in some wasm targets, or when
     the linker garbage-collects sections — failures are silent and platform-specific.
  2. Registration order is not deterministic, so behaviour can differ between builds.
  3. It makes the route table invisible: you cannot see what your application serves by reading it,
     and `cargo expand` does not show you either.
  4. It breaks test isolation: two `App`s in one process would share a global registry.
  5. It conflicts with our "no magic that cannot be printed" rule.
- **A build-script that scans source files.** Fragile, slow, and wrong in the presence of `cfg`.

## Consequences

- Users write `.mount(routes::router())` and `.register::<SendWelcomeEmail>()` explicitly. The
  composition root in `lib.rs` is ~20 lines and shows the whole application.
- We gain: deterministic behaviour, testability (two apps in one process), a printable route table,
  and boot-time validation that a job enqueued somewhere is actually registered.
- The cost is a small amount of typing, which the generators write for you anyway.

## Reversal criteria

None foreseen. If explicit registration proves a genuine adoption barrier, the correct fix is better
generators, not link-time magic.
