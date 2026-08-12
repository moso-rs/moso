# Moso — Design Documentation

> **Moso** (`moso`) is a batteries-included, model-driven web framework for Rust.
> It targets the developer experience of FastAPI and the completeness of Django/Rails,
> on top of the Tokio / Tower / Hyper / Axum substrate.

These documents are the **normative specification** for building Moso. They are written to be
executed by a team of engineers — human or agent — working in parallel. Every document states
what to build, why that choice was made, what the public API looks like, and what "done" means.

---

## Reading order

If you read nothing else, read these five, in order:

1. [`00-foundations/00-vision.md`](00-foundations/00-vision.md) — what Moso is and who it is for
2. [`00-foundations/02-architecture.md`](00-foundations/02-architecture.md) — the layer cake, and the three loops we optimise
3. [`06-reference/63-implementation-status.md`](06-reference/63-implementation-status.md) — **what is actually built today**
4. [`06-reference/60-example-app.md`](06-reference/60-example-app.md) — the whole framework, end to end, as working code
5. [`05-delivery/51-work-packages.md`](05-delivery/51-work-packages.md) — the build order and who does what

> **Read #3 before planning anything.** These documents specify a complete framework; the current
> build is its HTTP half. The data layer and every battery — the ORM, migrations, KV, auth, authz,
> jobs, mail, storage, admin — are **design intent, not code**. The status document says exactly
> which is which, per document and per work package.

---

## Index

### 00 — Foundations
| Doc | Contents |
| --- | --- |
| [00-vision.md](00-foundations/00-vision.md) | Thesis, target user, competitive position, what "better than FastAPI" means concretely |
| [01-goals.md](00-foundations/01-goals.md) | Goals, non-goals, explicit anti-goals, success metrics |
| [02-architecture.md](00-foundations/02-architecture.md) | Layer diagram, substrate choice, the Axum relationship, data flow of a request |
| [03-crate-layout.md](00-foundations/03-crate-layout.md) | Every crate, its dependencies, its public surface, feature flags |
| [04-project-structure.md](00-foundations/04-project-structure.md) | The generated app layout, conventions, when to split into a workspace |
| [05-glossary.md](00-foundations/05-glossary.md) | Terms used precisely throughout these docs |

### 01 — HTTP layer
| Doc | Contents |
| --- | --- |
| [10-app-lifecycle.md](01-http/10-app-lifecycle.md) | `App`, boot sequence, graceful shutdown, startup validation |
| [11-routing.md](01-http/11-routing.md) | `Router`, `#[endpoint]`, `routes!`, path syntax, nesting, versioning |
| [12-extractors-responses.md](01-http/12-extractors-responses.md) | `Extract` / `IntoResponse` traits, the built-in set, self-describing extractors |
| [13-schema-validation.md](01-http/13-schema-validation.md) | `#[derive(Schema)]`, one attribute → validation + OpenAPI, parse-don't-validate types |
| [14-openapi.md](01-http/14-openapi.md) | Zero-annotation OpenAPI 3.1, doc UIs, client generation, drift tests |
| [15-dependency-injection.md](01-http/15-dependency-injection.md) | `Inject`, `Depends`, request-scoped caching, startup-time graph validation |
| [16-errors.md](01-http/16-errors.md) | `moso::Error`, RFC 9457 problem details, field-pathed 422s, error taxonomy |
| [17-middleware.md](01-http/17-middleware.md) | Tower integration, the default stack, ordering rules, per-route layers |
| [18-configuration.md](01-http/18-configuration.md) | `#[derive(Config)]`, layered sources, secrets, environment profiles |

### 02 — Data layer
| Doc | Contents |
| --- | --- |
| [20-orm-overview.md](02-data/20-orm-overview.md) | Why a new ORM, the non-negotiables, the sealed-facade risk strategy |
| [21-entities-queries.md](02-data/21-entities-queries.md) | `#[derive(Entity)]`, typed columns, the shape-stable query builder, projections |
| [22-relations.md](02-data/22-relations.md) | Relations, N+1-safe eager loading, `Related<T>`, nested preloads |
| [23-migrations.md](02-data/23-migrations.md) | Schema snapshot diffing, generated + reviewable migrations, destructive-change policy |
| [24-transactions-pooling.md](02-data/24-transactions-pooling.md) | Transactions, retries, pooling, read replicas, multi-tenancy |
| [25-kv-cache.md](02-data/25-kv-cache.md) | `KvStore` trait, typed namespaces, backends, `#[cached]` |

### 03 — Batteries
| Doc | Contents |
| --- | --- |
| [30-auth.md](03-batteries/30-auth.md) | Sessions, passwords, JWT, OAuth2, passkeys, API keys, `CurrentUser` |
| [31-authorization.md](03-batteries/31-authorization.md) | The typed permission registry, RBAC + resource policies, `Authorized<A, R>` |
| [32-jobs.md](03-batteries/32-jobs.md) | `#[job]`, queues, retries, cron, transactional enqueue, dashboard |
| [33-admin.md](03-batteries/33-admin.md) | Auto-generated CRUD admin, HTMX, filters/search/pagination from day one |
| [34-mail-storage-realtime.md](03-batteries/34-mail-storage-realtime.md) | Mailer, object storage, WebSockets/SSE, rate limiting |

### 04 — Developer experience
| Doc | Contents |
| --- | --- |
| [40-cli.md](04-devex/40-cli.md) | `moso new/dev/generate/db/routes/check/doctor/openapi` |
| [41-diagnostics.md](04-devex/41-diagnostics.md) | Killing trait-bound vomit: `on_unimplemented`, assertion codegen, `moso check` |
| [42-compile-times.md](04-devex/42-compile-times.md) | Budgets, architecture rules, toolchain config, measurement harness |
| [43-testing.md](04-devex/43-testing.md) | `TestApp`, typed test client, DB templating, factories, snapshot tests |
| [44-observability.md](04-devex/44-observability.md) | Tracing, OTel, metrics, health checks, structured logging |
| [45-security.md](04-devex/45-security.md) | Secure defaults, CSRF, headers, secrets handling, threat model |
| [46-docs-strategy.md](04-devex/46-docs-strategy.md) | Tutorial-grade docs as a shipped artifact, doc tests, LLM-facing docs |

### 05 — Delivery
| Doc | Contents |
| --- | --- |
| [50-roadmap.md](05-delivery/50-roadmap.md) | Milestones M0–M5, scope gates, kill criteria |
| [51-work-packages.md](05-delivery/51-work-packages.md) | Parallelisable work packages with acceptance criteria and file paths |
| [52-governance.md](05-delivery/52-governance.md) | Core team, RFC process, funding model, single-maintainer risk |
| [53-quality-gates.md](05-delivery/53-quality-gates.md) | Benchmarks, CI gates, stability policy, semver discipline |

### 06 — Reference
| Doc | Contents |
| --- | --- |
| [60-example-app.md](06-reference/60-example-app.md) | A complete application, every file, no elisions |
| [61-api-reference.md](06-reference/61-api-reference.md) | Every public trait and type signature, in one place |
| [62-macro-reference.md](06-reference/62-macro-reference.md) | Exact expansion of every macro Moso ships |
| [63-implementation-status.md](06-reference/63-implementation-status.md) | **What is actually built**, document by document, and what is deferred |

### ADRs
Short, dated decision records with alternatives and reversal criteria.
See [`adr/README.md`](adr/README.md).

---

## Conventions used in these documents

- **MUST / SHOULD / MAY** are used in the RFC 2119 sense. `MUST` items are acceptance criteria.
- Code blocks marked `// spec` are normative API signatures. Code blocks marked `// example`
  illustrate usage and may elide details.
- `TODO(agent)` marks a deliberate gap where the implementing engineer must make a judgement call;
  each one names the criteria for that call.
- Version numbers of third-party crates are given as major-minor floors, e.g. `sqlx 0.8+`.
  Pin exact versions in `Cargo.toml` at implementation time and record them in `adr/`.

## Status

All documents are **Draft — normative for M0/M1**. Anything past M2 is directional and expected
to be revised once M1 ships and real users report back.

**M1 has now been built.** The documents in `00-foundations/`, `01-http/`, `06-reference/` and
`adr/` have been reconciled against the shipped code, and each one carries a status line saying
whether it describes reality. `02-data/` and `03-batteries/` have not been implemented at all and
carry a banner saying so. The per-document ledger is
[`06-reference/63-implementation-status.md`](06-reference/63-implementation-status.md).

Where a document and the code disagreed, the resolution rule from
[`05-delivery/51-work-packages.md`](05-delivery/51-work-packages.md) applied: the document is
updated in the same change, and if the divergence was a *decision* rather than an oversight, an ADR
records it. That is where [ADR-0013](adr/0013-handler-registration.md) came from.
