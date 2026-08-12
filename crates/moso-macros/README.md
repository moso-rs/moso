# moso-macros

**Every procedural macro Moso ships.**

Nothing here is imported directly. Every macro is re-exported from
[`moso`](../moso), and every macro *expands* to paths under
`::moso::__private::*` — never to `::moso_core::…`, never to
`::moso_schema::…`. That indirection is what lets the runtime crates be split,
renamed or refactored without changing a byte of generated code, and it is why
this crate depends on `syn`, `quote`, `proc-macro2`, `darling` and `heck` and on
no Moso crate at all.

## The macros

| Macro | What it does |
| --- | --- |
| `#[endpoint]` | leaves the `async fn` alone and emits `__moso_op_<name>`: an `Endpoint` (the OpenAPI description) and a `HandlerFn` (one concrete, non-generic extraction future) |
| `routes!` | a table of `METHOD "/path" => handler`, expanded into the builder chain |
| `ep!` | one token: `handler` → `__moso_op_handler`, so the builder chain works too |
| `#[middleware]` | one `async fn` → a named, `Clone` `NameLayer` / `NameService<S>` pair |
| `#[derive(Schema)]` | serde + `Validate` + `Schema` + `Describe` + `IntoResponse`, from one attribute vocabulary |
| `#[derive(Constrained)]` | a newtype that cannot hold an invalid value; `Deserialize` routes through the constructor |
| `#[derive(Responder)]` | `IntoResponse` + `Describe` with a status and headers you choose |
| `#[derive(Dependency)]` | the "compose" and "wrap and check" shapes, including transitive `PROVIDER_REQ` |
| `#[derive(Config)]` | layered typed configuration with a boot-time report |
| `#[derive(Error)]` | `Display`, `Error`, `From`, and the status / `type` URI mapping onto RFC 9457 |

## No magic that cannot be printed

Every expansion is written out in `docs/06-reference/62-macro-reference.md` and
is reproducible with `cargo expand`. Three rules hold across all of them:

1. Generated identifiers are prefixed `__moso_` and carry `#[doc(hidden)]`,
   except for the types a user is expected to name (`TenantLayer`, …).
2. An unknown attribute key is a compile error with a "did you mean"
   suggestion, never a silent no-op.
3. One user mistake produces exactly **one** error, plus a well-typed
   placeholder so the rest of the module still resolves.

Every error's span points at the **user's** token, never at generated code, and
carries a `help:` line that is code they can paste. The regression suite for
that promise is [`moso-ui-tests`](../moso-ui-tests).

## Licence

MIT — see the root [`LICENSE`](../../LICENSE).
