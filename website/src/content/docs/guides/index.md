---
title: Guides
description: Task-oriented pages for every part of Moso, from routing and extractors to jobs, storage and security.
order: 1
---

Each guide covers one subsystem completely: the smallest thing that works, the options, the
advanced paths, and the ways it fails. They assume you know Rust and have built a web service
before. If you have not run Moso yet, start with the [quick start](../start/quick-start.md), then
come back here.

Guides are reference-shaped explanations of a feature. Each one covers a single subsystem and the
code you would actually write against it.

## HTTP

- [Routing](./routing.md): the `routes!` table, `#[endpoint]`, path syntax, nesting, per route
  layers, and the checks that run at compile time and at boot.
- [Extractors](./extractors.md): how a handler parameter reads the request, and how each one
  describes itself in the API document.
- [Responses](./responses.md): what a handler can return, and how status, headers and body are
  chosen.
- [Schemas](./schemas.md): `#[derive(Schema)]`, the one type that drives the body, the document and
  the validation.
- [Validation](./validation.md): constraints on a schema, when they run, and what a failure looks
  like on the wire.
- [Errors](./errors.md): the `Error` type, RFC 9457 problem documents, and mapping an application
  taxonomy onto statuses.
- [Dependency injection](./dependency-injection.md): `Inject`, `Depends`, providers, and the boot
  time check that proves every dependency exists.
- [Middleware](./middleware.md): the ordered stack, writing your own layer, and where per route
  layers sit inside it.
- [Configuration](./configuration.md): `#[derive(Config)]`, profiles, environment overrides and
  secrets.
- [OpenAPI](./openapi.md): the document assembled at boot, how to shape it, and how to export it in
  CI.
- [Health and shutdown](./health-and-shutdown.md): the liveness and readiness endpoints, health
  checks, and draining in flight requests.

## Data

- [Relations](./relations.md): the entity model, the query builder, and loading relations without
  an N+1.
- [Transactions and pooling](./transactions.md): transaction scope, isolation, and how the pool is
  configured.
- [Multi-tenancy](./multi-tenancy.md): scoping data to a tenant and keeping the scope from leaking.
- [Migrations](./migrations.md): writing, applying and reverting schema changes.
- [Raw SQL](./raw-sql.md): the escape hatch, what it costs, and what stays checked.
- [Cache and key value store](./cache.md): the KV facade, expiry, and the backends behind it.
- [Rate limiting and locks](./rate-limiting.md): counters, limits and distributed locks built on the
  same store.

## Batteries

- [Authentication](./authentication.md): the identity model and how a request becomes a subject.
- [Passwords and sessions](./passwords-and-sessions.md): hashing, sign in, session cookies and
  rotation.
- [JWT and API keys](./jwt-and-api-keys.md): stateless tokens, key issuance and revocation.
- [OAuth and passkeys](./oauth-and-passkeys.md): third party sign in and WebAuthn credentials.
- [Permissions and roles](./permissions.md): `#[requires]`, roles, and the 403 that reaches the
  document.
- [Policies and query scoping](./policies.md): per record decisions and pushing them into the query.
- [Background jobs](./jobs.md): enqueuing work, workers, retries and failure handling.
- [Scheduled jobs](./scheduled-jobs.md): cron style schedules and running them exactly once across
  instances.
- [Sending mail](./mail.md): templates, transports and the local preview.
- [File storage](./file-storage.md): uploads, backends and signed URLs.
- [Server sent events and realtime](./realtime.md): streaming responses and pushing to connected
  clients.

## Operating an application

- [Testing](./testing.md): driving the application as a service, overriding providers, and testing
  against a real database.
- [Observability](./observability.md): tracing, metrics, structured logs and request correlation.
- [Security](./security.md): the defaults you get, the headers, and what you still have to decide.
