---
title: Introduction
description: What Moso is, the single idea it is built on, what ships in the box, who it is for, and how to read the rest of these documents.
order: 1
status: shipped
---

Moso is a batteries-included web framework for Rust. It sits on the settled substrate (Tokio, Hyper,
Tower, Axum) and supplies the layer above it: typed configuration, dependency injection validated at
boot, an ORM, migrations, authentication, authorization, background jobs, a cache, mail, file
storage, a test harness and a CLI.

It exists because of one specific frustration. In a typical Rust API you describe the same payload
three times: once for `serde`, once for whatever validation crate you picked, and once again in an
OpenAPI annotation. Those three descriptions drift. Moso makes the type the only description.

## The one idea

A type definition drives parsing, validation, serialisation and documentation. There is no second
place to keep in sync, because there is no second place.

```rust title="src/routes.rs"
use moso::prelude::*;

/// What `POST /greetings` accepts.
#[derive(Schema, Debug)]
pub struct NewGreeting {
    /// Who to greet.
    #[schema(len = 1..=64)]
    pub name: String,
}

/// What the greeting endpoints return.
#[derive(Schema, Debug)]
pub struct Greeting {
    /// The rendered message.
    pub message: String,
}

/// Greet someone by name.
#[endpoint]
async fn greet(Json(body): Json<NewGreeting>) -> Result<Created<Greeting>> {
    Ok(Created::at("/greetings", Greeting {
        message: format!("hello, {}", body.name),
    }))
}
```

That single attribute, `#[schema(len = 1..=64)]`, produces two artefacts from one parse: the runtime
check, and the `minLength`/`maxLength` in the published schema. They cannot disagree. Add `trim` and
a third thing moves, the normalisation that runs before validation, plus a sentence in the schema
description explaining it.

The handler body never validates anything, because a `NewGreeting` that failed validation cannot
exist. Send a bad body and the request is rejected before `greet` is called:

```json
{
  "type": "https://moso.rs/errors/validation",
  "title": "Validation Failed",
  "status": 422,
  "detail": "1 field failed validation",
  "instance": "/greetings",
  "errors": [
    {
      "pointer": "/name",
      "code": "len",
      "message": "must be between 1 and 64 characters",
      "params": { "max": 64, "min": 1 }
    }
  ],
  "request_id": "01KYZA8A5J2QJCE017DVV0MH55"
}
```

RFC 9457 `application/problem+json`, with an RFC 6901 JSON Pointer at the offending field. A client
can highlight the right form input without parsing prose. A body that is not valid JSON, or that is
missing a required member, is a `400` instead: that is a serialiser bug rather than a data bug, and
the distinction is worth keeping.

Meanwhile `#[endpoint]` derived the OpenAPI operation from the same signature: the summary from the
doc comment, the request schema from `Json<NewGreeting>`, the `201` and its `Location` header from
`Created<Greeting>`. `/openapi.json` and `/docs` are served without a line of configuration.

Read [schemas](../guides/schemas.md) for the full attribute vocabulary.

## The second idea

Anything that can fail should fail at boot with a sentence, not at 3am inside a request.

`App::build()` walks the assembled application and proves it: every `Inject<T>` any handler or any
middleware asks for has a provider, every path template is well formed, no two routes collide, no
two operations share an id, no route is shadowed by a framework path. It reports every problem at
once, grouped by provider rather than by route, with the source location of each handler that is
affected and a mechanical fix.

```text
error: application failed to build (1 problem)

  x missing provider: `shop::store::Store`
      required by  GET /status                      src/routes/health.rs:36
                   GET /api/v1/posts                src/routes/posts.rs:57
                   POST /api/v1/posts               src/routes/posts.rs:87
      fix          register it on the `App` builder, usually in src/lib.rs
                   let value: Store = /* construct it */;
                   App::new(config).provide(value)
```

That is a class of production incident moved to the second before the listener binds. See
[dependency injection](../guides/dependency-injection.md) and
[health and shutdown](../guides/health-and-shutdown.md).

## What ships in the box

| Crate | What it gives you | State |
| --- | --- | --- |
| `moso` | the facade, the prelude, the macros. The only crate most applications name | shipped |
| `moso-core` | `App`, `Router`, extraction, responses, errors, DI, configuration, middleware | shipped |
| `moso-schema` | `Schema`, `Validate`, JSON Schema 2020-12, constrained types | shipped |
| `moso-openapi` | the OpenAPI 3.1 document model and the embedded docs UI | shipped |
| `moso-macros` | `#[endpoint]`, `routes!`, `ep!`, `#[middleware]` and the derives | shipped |
| `moso-sql` | the sealed SQL construction facade | built, docs unverified |
| `moso-orm` | entities, a shape-stable query builder, relations without N+1 | built, docs unverified |
| `moso-migrate` | migration generation, planning, the ledger and the runner | built, docs unverified |
| `moso-kv` | typed namespaces, caching, locks, rate limiting | built, docs unverified |
| `moso-auth` | sessions, passwords, JWT, OAuth, passkeys, API keys, MFA | built, docs unverified |
| `moso-authz` | permissions, roles, policies, query scoping, explain traces | built, docs unverified |
| `moso-jobs` | `#[job]`, transactional enqueue, retries, a dead letter queue, cron | built, docs unverified |
| `moso-mail` | a `Mailer`, templates, previews, suppression, provider webhooks | built, docs unverified |
| `moso-storage` | object storage over local, memory, S3, GCS and Azure | built, docs unverified |
| `moso-test` | `TestApp`, `TestClient`, JSON diffing, contract assertions | shipped |
| `moso-cli` | the `moso` binary: nine command groups | shipped |

"Built, docs unverified" is a real distinction and not a hedge. Those crates are workspace members,
they compile, their test suites pass, and their Postgres and Redis backends were exercised against
real servers. What has not happened is a line-by-line reconciliation between each design document
and the code written from it, so a specific API in those areas may differ from what the design says.

The CLI covers `new`, `dev`, `generate`, `db`, `routes`, `openapi`, `config`, `doctor` and `self`.
Unimplemented subcommands do not appear in the command tree at all rather than printing "coming
soon", so `moso --help` is an accurate list of what exists.

## Where Moso sits

Moso is to Axum what FastAPI is to Starlette. Axum is the engine and it stays visible: you can mount
an `axum::Router` with `mount_axum`, use any `tower-http` layer, and take the composed service back
out with `App::into_service()`. Every escape hatch is a documented, tested API rather than a
workaround.

What Moso adds is the layer Axum deliberately does not have: a composition root that validates
itself, a document derived from your signatures, an error model, and the batteries.

## Who this is for

You ship product APIs. You have built a web service before and you know Rust well enough to read a
trait bound, but your patience for a sixty-line trait error is zero. You are tired of assembling
eight crates and a build script before the first endpoint, and you want the OpenAPI document to be
correct because it was derived rather than because someone remembered to update it.

If you came from FastAPI, Rails or Django and your complaint about Rust is "there is no consensus
data layer and the errors are unreadable" rather than "Rust is hard", this is aimed at you.

## Who this is not for

If you are writing a 2M requests per second edge proxy, use Axum or Hyper directly. Moso will not
out-benchmark a hand-written Axum service and does not claim to. The claim is that it lands within
noise of a competent hand-rolled equivalent while removing most of the code you would have written.

If your service is one endpoint and a health check, the framework is more machinery than the problem
needs. Axum alone is fine.

If you need a stable public API today, wait. See the next section.

## How mature this is

Be pessimistic when planning around this. Concretely, as of the last reconciliation:

- **Nothing is published.** The workspace is at an unreleased `0.1.0` with no release tags, so
  `cargo add moso` does not get you this code. You install from a checkout, which
  [Installation](./installation.md) walks through in about two minutes.
- **There is no semver promise yet.** Names will move.
- **The HTTP half is solid.** Routing, extraction, responses, schemas, validation, errors, DI,
  configuration, middleware, OpenAPI, health and shutdown are implemented, tested, and their design
  documents have been checked against the code.
- **The batteries are real but unreconciled.** See the table above.
- **Two workspace quality gates fail.** A third-party dependency count over budget (155 against 90),
  and macro expansion sizes over budget. Neither was fixed by lowering the number.
- **The CLI is complete as a command tree, and narrower than its design document in places.** All
  eighteen commands exist, including `moso check`, `moso run`, `moso test`, `moso build`,
  `moso client`, `moso middleware` and all eight `moso db` subcommands, so a compiler message that
  tells you to run `moso check` now points at something real. What is thinner than the sketch:
  `moso dev` does not replay requests queued across a restart, `moso test` manages no database,
  `moso deploy` is `checklist` alone, and `moso self update` reports a version rather than replacing
  the binary. `moso worker`, `moso task` and `moso db prune-test` are absent by decision rather than
  by omission. Each is explained in the CLI reference.
- **There is no way to install the CLI but `cargo install moso-cli`.** No prebuilt binaries, no
  Homebrew, no release pipeline.

## How to read the rest

- [Installation](./installation.md) and [Quick start](./quick-start.md) get a validated, documented
  endpoint answering on your machine. Start there. Then [Project layout](./project-layout.md)
  explains the shape of what you generated.
- [Guides](../guides/index.md) are the how-to reference, one per feature, each covering the advanced
  paths and the failure modes rather than only the happy path. This is the largest section and the
  one you will come back to.
