# moso-schema

**One type definition doing three jobs: a serde model, a set of runtime
constraints, and a JSON Schema 2020-12 document.**

A Rust type that is a validated, documented API model normally needs three
ecosystems and two vocabularies for the same constraint:

```text
#[derive(Serialize, Deserialize, ToSchema, Validate)]
pub struct CreateUser {
    #[validate(length(min = 3, max = 32))]     // enforced
    #[schema(min_length = 3, max_length = 32)] // documented - must match by hand
    pub username: String,
}
```

They drift, and then the documentation lies. Moso's answer is one attribute
vocabulary and one derive, with the OpenAPI constraint *generated from* the
validation rule - they cannot disagree because there is only one of them.

```rust
use moso::prelude::*;

/// A user, as the API accepts one.
#[derive(Schema)]
pub struct CreateUser {
    /// Public handle. Lowercase letters, digits and underscores.
    #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
    pub username: String,
    /// Contact address - the type carries the constraint.
    pub email: Email,
    /// Optional age, in years.
    #[schema(range = 13..=130)]
    pub age: Option<u8>,
}
```

## What is in it

| Module | Contents |
| --- | --- |
| `schema` | the `Schema` trait |
| `validate` | `Validate`, `ValidationCtx`, `ValidationErrors`, `FieldError`, the closed `codes` set |
| `checks` | the `check_*` helpers a generated `Validate` body calls |
| `json_schema` | the JSON Schema 2020-12 model, its builders and `SchemaGenerator` |
| `message` | English messages and the `MessageProvider` extension point |
| `types` | constrained types: `Email`, `Password`, `Slug`, `Id<E>`, `Cursor`, `Url`, `Sanitised<P>`, … |

## Two guarantees worth stating plainly

**Attributes and types are not alternatives.** `#[schema(len = …)]` protects one
field; a constrained type such as `Email` protects every value of that type
everywhere in the program, including ones constructed in code that never saw a
request. Prefer the type where one exists.

**Validation is synchronous and pure.** There is deliberately no
`async fn validate`. "Is this email already taken?" is not validation: a
check-then-act against a database is a race, and the correct place for that rule
is the transaction that will enforce it, surfacing as a `409 Conflict`.

## Independence

Nothing here knows about HTTP. The crate is usable on its own - for a CLI's
configuration model, or a message-queue payload - and `moso-openapi` depends on
*it*, not the other way round. It is published on crates.io: `cargo add
moso-schema`, or `moso-schema = "0.0.1"`.

The `#[derive(Schema)]` macro itself lives in
[`moso-macros`](../moso-macros) and is re-exported from
[`moso`](../moso).

## Licence

MIT - see the root [`LICENSE`](../../LICENSE).
