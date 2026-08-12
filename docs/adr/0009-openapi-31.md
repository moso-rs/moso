# ADR-0009 — Target OpenAPI 3.1, not 3.0

Status: Accepted
Date: 2026-07-29

## Context

OpenAPI 3.0 uses a JSON-Schema-like dialect that is not actually JSON Schema. 3.1 aligns with JSON
Schema 2020-12. Since `#[derive(Schema)]` naturally produces JSON Schema, targeting 3.0 means a
lossy translation on every emit.

Concretely, 3.0 lacks: `$defs`, `const`, proper `null` handling (it has a bespoke `nullable`),
`exclusiveMinimum`/`exclusiveMaximum` as numbers, `unevaluatedProperties`, and full `$ref` sibling
support. Tooling support for 3.1 has matured.

## Decision

Emit **OpenAPI 3.1.1** with JSON Schema 2020-12. Validate output against the official meta-schema
in CI.

Provide `moso openapi export --version 3.0` as a **documented, lossy** downgrade for teams whose
tooling has not caught up, emitting warnings for each construct that cannot be represented faithfully.

## Alternatives considered

- **3.0 as the default.** Broadest tooling compatibility today, but it means our schema generator
  produces a degraded dialect for everyone, permanently, to serve a shrinking minority.
- **Emit both by default.** Doubles the drift surface and the tests.

## Consequences

- Users on old generators may need the downgrade flag. We test our output against
  `openapi-generator` and `orval` in CI so we know when this bites.
- Our generated TypeScript client can use `discriminator` properly, producing real discriminated
  unions rather than `any`.
- Nullable handling is correct rather than approximated, which matters for `Option<T>` fields —
  by far the most common shape in Rust models.

## Reversal criteria

- If a majority of surveyed users report their tooling cannot consume 3.1 a year after launch,
  reconsider the default (not the capability).
