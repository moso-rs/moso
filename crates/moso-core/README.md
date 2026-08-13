# moso-core

**The runtime: `App`, `Router`, extraction, responses, errors, dependency
injection and configuration.**

You almost certainly want [`moso`](../moso) instead - it re-exports everything
here under stable paths and adds the macros. Depend on `moso-core` directly only
when writing a Moso battery that must not pull in the facade.

## What is in it

| Module | Contents |
| --- | --- |
| `app` | `App`, `AppBuilder`, `AppState`, `Resolver`, `Lifespan` - the composition root and boot-time validation |
| `router` | `Router`, `RouteEntry`, `MethodRouter`, guards, static files |
| `handler` | `Endpoint`, `HandlerFn`, `Handler<M>` - how a plain `async fn` becomes a route |
| `extract` | `Json`, `Form`, `Path`, `Query`, `Headers`, `Cookies`, `Bytes`, `Text`, `BodyStream`, Axum interop |
| `response` | `Created`, `NoContent`, `Page`, `Redirect`, `Sse`, `File`, `Cached`, `Either`, `Raw` |
| `di` | `Inject<T>`, `Depends<T>`, `ProviderMap`, `ProviderReq` |
| `ctx` | `RequestCtx`, the per-request dependency cache, `Limits` |
| `error` | one concrete `Error`, RFC 9457 problem rendering, the boot report |
| `config` | `Config`, layered sources, `SecretString`, `Profile` |
| `middleware` | the named `MiddlewareStack`, `Slot`, `Next`, `Guard` |
| `health`, `shutdown`, `task` | readiness probes, graceful drain, the blocking pool |

## Three design decisions worth knowing

**`Router` is not generic over state.** There is no `Router<S>` and no
`FromRef`. Everything a handler needs comes from the provider map through
`Inject<T>` or from the request through `Depends<T>`. This removes the largest
family of Axum trait errors and the largest monomorphisation cost.

**Validation happens inside extraction.** `Json<T>` reads the body under a hard
byte cap, deserialises with `serde_path_to_error` (a `400` naming the exact JSON
Pointer), then validates (a `422`, one entry per failed field). There is no way
to obtain a `T` from a request that skipped either step.

**Missing providers are a boot error, not a 500.** `App::build()` walks every
route's `required_providers()` and reports every problem at once, so
`Inject<Db>` is infallible at the use site.

## Features

| Feature | Default | Effect |
| --- | --- | --- |
| `openapi` | yes | mounts `/docs` and `/openapi.json`; the document is generated either way |
| `tracing` | yes | installs `TraceLayer` and request-id spans in the default stack |
| `compression` | no | response compression |
| `cors` | no | the CORS layer |
| `multipart` | no | `Multipart` bodies |
| `ws` | no | WebSocket upgrades |

`moso-openapi` is an **unconditional** dependency so that trait signatures are
never feature-dependent.

## Licence

MIT - see the root [`LICENSE`](../../LICENSE).
