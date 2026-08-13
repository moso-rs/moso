# moso

**The facade. This is the only Moso crate an application depends on.**

It contains no logic of its own: it re-exports the runtime crates, provides the
prelude, and owns the hidden `__private` module that macro output resolves
against.

Published on crates.io - add it with `cargo add moso`, or:

```toml
[dependencies]
moso = "0.0.1"
```

```rust
use moso::prelude::*;

/// A user, as the API accepts one.
#[derive(Schema)]
pub struct CreateUser {
    /// Public handle.
    #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
    pub username: String,
    /// Contact address.
    pub email: Email,
}

/// A user, as the API returns one.
#[derive(Schema)]
pub struct UserOut {
    /// Stable identifier.
    pub id: u64,
    /// Public handle.
    pub username: String,
}

/// Create a user.
#[endpoint]
async fn create(Json(body): Json<CreateUser>) -> Result<Created<UserOut>> {
    Ok(Created::at("/users/1", UserOut { id: 1, username: body.username }))
}

/// Everything this module serves.
pub fn router() -> Router {
    moso::routes! { POST "/users" => create }.tag("users")
}
```

The body is parsed, validated and rejected with an RFC 9457
`application/problem+json` document *before* `create` runs, and the OpenAPI
operation - path, request schema, `201` response schema, tag - is derived from
the same signature. There is no second description of this endpoint to keep in
sync.

## Where things live

| Path | Contents |
| --- | --- |
| `moso::prelude` | the ~30 names an application actually types |
| `moso::extract` | `Json`, `Path`, `Query`, `Headers`, `Inject`, `Depends`, `Cookies`, … |
| `moso::response` | `Created`, `NoContent`, `Page`, `Redirect`, `Sse`, `File`, `Either`, … |
| `moso::config` | `Config`, layered sources, `SecretString`, `Profile` |
| `moso::middleware` | `MiddlewareStack`, `Slot`, `Next`, `Guard` |
| `moso::schema` | `Schema`, `Validate`, `Email`, `Slug`, `Id`, the JSON Schema model |
| `moso::openapi` | the OpenAPI 3.1 document model and its builders |
| `moso::deps` | the third-party crates whose types appear in Moso's API |

## Feature flags

| Feature | Default | Effect |
| --- | --- | --- |
| `http` | yes | accepted and inert; `moso-core` is unconditional |
| `openapi` | yes | mounts `/docs` and `/openapi.json` |
| `tracing` | yes | installs the tracing layer in the default middleware stack |
| `compression` | no | response compression |
| `cors` | no | the CORS layer |
| `multipart` | no | multipart bodies |
| `ws` | no | WebSocket upgrades |

The document is generated whatever `openapi` says; the feature decides only
whether the routes that *serve* it are mounted, so `moso openapi export` works
in every build.

## Licence

MIT - see the root [`LICENSE`](../../LICENSE).
