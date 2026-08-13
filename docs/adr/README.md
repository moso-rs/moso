# Architecture Decision Records

Short, dated records of decisions that are expensive to reverse. Each states the context, the
decision, the alternatives considered, the consequences, and - critically - the **reversal
criteria**: what evidence would make us change our mind.

A decision without written reversal criteria tends to become dogma. These exist so that changing
course later is a normal engineering act rather than an admission of failure.

## Format

```markdown
# ADR-NNNN - Title
Status: Proposed | Accepted | Superseded by ADR-XXXX | Rejected
Date: YYYY-MM-DD
Deciders: names

## Context
## Decision
## Alternatives considered
## Consequences
## Reversal criteria
```

## Index

| # | Title | Status | Reversal risk |
| --- | --- | --- | --- |
| [0001](0001-build-on-axum.md) | Build on Axum/Tower/Tokio rather than a new HTTP core | Accepted | Low |
| [0002](0002-own-the-handler-traits.md) | Own the handler and extractor traits | Accepted | Medium |
| [0003](0003-runtime-di-with-boot-validation.md) | Runtime DI with boot-time validation, not compile-time codegen | Accepted | Low - door left open |
| [0004](0004-no-link-time-registries.md) | No `inventory`/`ctor` link-time registration | Accepted | Low |
| [0005](0005-sealed-sql-facade.md) | Sealed query-construction facade over `sea-query` | Accepted | **Designed to be reversible** |
| [0006](0006-openapi-assembly-at-boot.md) | Assemble the OpenAPI document at boot, not at build | Accepted | Low |
| [0007](0007-shape-stable-query-builder.md) | Shape-stable query builder; no type-level query encoding | Accepted | **High cost to reverse** |
| [0008](0008-entities-are-not-schemas.md) | Entities do not implement `Schema` | Accepted | Low |
| [0009](0009-openapi-31.md) | Target OpenAPI 3.1, not 3.0 | Accepted | Low |
| [0010](0010-postgres-first.md) | Postgres first-class, SQLite full, MySQL best-effort | Accepted | Medium |
| [0011](0011-tokio-only.md) | Tokio only; no runtime abstraction | Accepted | Low |
| [0012](0012-licence-and-commercial-model.md) | Apache-2.0 OR MIT, permissive forever, first-party commercial products | Superseded by 0014 | - |
| [0013](0013-handler-registration.md) | Handler registration via a companion type, `routes!` and `ep!` | Accepted | Low - blocked by the language |
| [0014](0014-agpl-relicence.md) | `AGPL-3.0-only`, superseding the permissive-forever commitment | Superseded by 0018 | - |
| [0015](0015-webauthn-openssl-exception.md) | A scoped OpenSSL + MPL exception for the WebAuthn attestation path | Accepted | Low - off-by-default feature contains it; deleted if `webauthn-rs` drops OpenSSL |
| [0016](0016-battery-routes-documentation-and-boot-check-boundary.md) | Battery-mounted routes stay `x-moso-undocumented` with an empty boot check; the copy-out tier is the documented answer | Accepted | Low - closed by `moso new --auth` or a macro-free `#[endpoint]` |
| [0017](0017-moso-auth-seams-to-the-application.md) | Four seams where `moso-auth` hands a concern to the application (throttle, session cookie, i18n, mail) | Accepted | Low per seam; each moves only under its own RFC |
| [0018](0018-mit-relicence.md) | `MIT`, superseding the AGPL relicence - adoption over source protection | Accepted | One-way: tightening back to copyleft needs every contributor's assent once published |

## Pending decisions

Recorded here so they are not forgotten. Each names who decides and on what evidence.

| Topic | Decide by | Evidence needed |
| --- | --- | --- |
| Joined-set type parameter on `Select<E, J>` vs. a runtime check | end of WP-13 | measured ergonomics with 5 external developers; the worst error message each produces |
| Wrap `apalis` vs. build the job backend | end of WP-19 | Postgres backend at 1000 jobs/s; whether `apalis` supports lease/heartbeat/transactional enqueue semantics |
| Whether `#[endpoint]` assertion codegen stays unconditional in release builds | WP-25 | compile-time cost measured against the 6% budget |
| Whether to ship a `moso build` ahead-of-time DI analyser | post-1.0 | user demand; whether boot validation proved insufficient |
| Admin templating engine: `maud` vs `minijinja` for the built-ins | start of WP-22 | compile-time cost, override ergonomics |
