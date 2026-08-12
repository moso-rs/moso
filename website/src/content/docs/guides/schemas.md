---
title: Schemas
description: Derive serde, runtime validation and a JSON Schema document from one set of attributes on one struct.
order: 5
status: shipped
---

A Moso model is one struct with one derive. `#[derive(Schema)]` reads a single `#[schema(...)]`
attribute vocabulary and emits both halves of every constraint: the runtime check that rejects a bad
request, and the JSON Schema keyword that documents it. They are generated from the same parsed
attribute, so they cannot drift apart.

This page covers the derive end to end: what it generates, every attribute key, and the parts that
behave differently from what you would guess (nested validation is opt-in, `secret` is refused on a
`String`, a struct with a secret field must not also derive `Debug`). For the constrained types and
the shape of a validation failure on the wire, see [validation](./validation.md).

## The smallest model

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
    /// Optional age, in years.
    #[schema(range = 13..=130)]
    pub age: Option<u8>,
}

let user: CreateUser = serde_json::from_str(
    r#"{"username":"ada","email":"ada@example.com","age":36}"#,
).unwrap();
let ctx = &mut moso::schema::ValidationCtx::new();
assert!(moso::schema::Validate::validate(&user, ctx).is_ok());
```

`Schema` and `Email` come from the prelude. `Validate` and `ValidationCtx`
do not; reach them at `moso::schema::*`.

You rarely call `validate` yourself. `Json<CreateUser>` in a handler signature runs it during
extraction, so a handler cannot be entered with an invalid value. See
[extractors](./extractors.md).

## What the derive generates

| Item | Notes |
| --- | --- |
| `Serialize` and `Deserialize` | Delegated to serde's own derive on a `#[serde(remote = "…")]` shadow type inside an anonymous `const`, so every `#[serde(...)]` attribute you write by hand keeps working. |
| `Validate` | The runtime half of every constraint. All failures collected, each with a JSON Pointer. |
| `Schema` | `schema_name()`, `json_schema()` and the `HAS_CONSTRAINTS` constant. |
| `Debug` | Only when at least one field is `#[schema(secret)]`. Secret fields print `[redacted]`. |
| `IntoResponse` and `Describe` | So a handler can return the type directly and the OpenAPI document knows its response shape. |
| `From<Other>` | One impl per `#[schema(from = Other)]`, matched by field name. |

`schema_name()` is the key the type occupies in `components/schemas`, and it is part of your public
API: changing it breaks generated clients. `HAS_CONSTRAINTS` is computed at compile time and drives
whether a `422` is documented for operations taking the type as a body, so a constraint-free DTO does
not advertise a response the server can never send.

> [!NOTE]
> A field can make `HAS_CONSTRAINTS` true without carrying an attribute. `email: Email` has no
> `#[schema(...)]` above it, but the *type* can reject a value, so the struct is constrained.

## Container attributes

Written on the struct or enum itself, as `#[schema(...)]`.

| Key | Form | Effect |
| --- | --- | --- |
| `rename` | `= "Name"` | Sets the serde container rename and the `components/schemas` key. |
| `rename_all` | `= "camelCase"` | One of `lowercase`, `UPPERCASE`, `PascalCase`, `camelCase`, `snake_case`, `SCREAMING_SNAKE_CASE`, `kebab-case`, `SCREAMING-KEBAB-CASE`. An unknown convention is a compile error with a suggestion. |
| `deny_unknown` | flag | Emits `#[serde(deny_unknown_fields)]` and `additionalProperties: false`. Forces `HAS_CONSTRAINTS`. Cannot coexist with a `flatten`ed field. |
| `from` | `= Other` | Generates `impl From<Other> for Self` by field-name matching. Repeatable. Structs only. |
| `check` | `= my_fn` | Runs `fn(&Self, &mut ValidationCtx) -> Result<(), ValidationErrors>` after the field checks, merging its errors. Repeatable. Forces `HAS_CONSTRAINTS`. |
| `title` | `= "..."` | Sets `title`. |
| `description` | `= "..."` | Overrides the doc comment. |
| `tag` | `= "kind"` | Internally tagged enum. |
| `content` | `= "data"` | With `tag`, adjacently tagged. Without `tag`, a compile error. |
| `untagged` | flag | Untagged enum. With `tag`, a compile error. |
| `deprecated` | flag or `= "note"` | Sets `deprecated: true`; the note is appended to the description. |
| `example` | `= expr` | Serialised with `serde_json::to_value` and pushed onto `examples`. Repeatable. |
| `no_serde` | flag | Suppresses the generated `Serialize` and `Deserialize` so you can write them yourself. |
| `no_response` | flag | Suppresses the generated `IntoResponse` and `Describe`. Implied by a sibling `#[responder(...)]`. |

The doc comment on the item becomes the schema `description`, and the doc comment on each field
becomes that property's description. Write them.

## Field attributes

| Key | Form | Runtime effect | Document effect |
| --- | --- | --- | --- |
| `len` | `= 3..=32`, `= 1..`, `= ..=10`, `= 1..10`, `= 4` | Length in Unicode characters for strings, elements for sequences, properties for maps | `minLength`/`maxLength`, `minItems`/`maxItems` or `minProperties`/`maxProperties` |
| `non_empty` | flag | Rejects an empty string or collection | minimum of 1 |
| `pattern` | `= r"..."` | Regex match, compiled once into a `OnceLock` | `pattern` |
| `format` | `= "email"` | Checked against the known format list | `format` |
| `contains` | `= "x"` | Substring check | escaped `pattern` |
| `starts_with` | `= "x"` | Prefix check | `pattern` anchored with `^` |
| `ends_with` | `= "x"` | Suffix check | `pattern` anchored with `$` |
| `trim` | flag | Rewrites the value during `Deserialize` | a sentence appended to the description |
| `lowercase` | flag | Rewrites during `Deserialize` | a sentence appended to the description |
| `uppercase` | flag | Rewrites during `Deserialize` | a sentence appended to the description |
| `range` | `= 1..=100`, `= 0.0..1.0`, `= 13..` | Numeric bound | `minimum`/`maximum` or `exclusiveMinimum`/`exclusiveMaximum` |
| `positive` | flag | Exclusive lower bound of 0 | `exclusiveMinimum: 0` |
| `non_negative` | flag | Inclusive lower bound of 0 | `minimum: 0` |
| `multiple_of` | `= 5` | Divisibility | `multipleOf` |
| `unique` | flag | Rejects duplicate elements, pointing at the duplicate's index | `uniqueItems: true` |
| `each(...)` | nested list | Applies a constraint set to every element, with `/tags/2`-style pointers | mutates `items` |
| `nested` | flag | Runs the field type's own `Validate`, lifting its pointers | nothing; the `$ref` already says it |
| `enum_values` | `= ["draft", "published"]` | Closed value set | `enum` |
| `default` | flag or `= expr` | `#[serde(default)]` or a generated thunk | `default`, and the field leaves `required` |
| `rename` | `= "x"` | Changes the property name and the JSON Pointer | property name |
| `skip` | flag | Absent from serde | absent from the schema |
| `read_only` | flag | `#[serde(skip_deserializing)]`; the field leaves `required` | `readOnly: true` |
| `write_only` | flag | `#[serde(skip_serializing)]` | `writeOnly: true` |
| `secret` | flag | Implies `write_only`; redacts this struct's generated `Debug` | `writeOnly: true` |
| `flatten` | flag | `#[serde(flatten)]`; the inner fields move to the root | `allOf` composition |
| `delimiter` | `= ","`, `"\|"` or `" "` | Accepts one delimited string as well as a list | nothing |
| `deprecated` | flag or `= "note"` | none | `deprecated: true` plus a description note |
| `example` | `= expr` | none | pushed onto `examples`. Repeatable. |
| `title` | `= "..."` | none | `title` |
| `description` | `= "..."` | none | overrides the doc comment |
| `flatten_bracket` | flag | none, see [edges to know](#edges-to-know) | reserved |

Ranges are real Rust range syntax parsed from the token stream, so a bound cannot be a malformed
string, and a non-literal bound is rejected with "a bound must be a literal number". A half-open
`len = 1..10` is lowered to an inclusive maximum of 9; a half-open `range = 0.0..1.0` becomes an
`exclusiveMaximum`, because a float has no predecessor to lower to.

Inside `each(...)` you may use `len`, `pattern`, `format`, `non_empty`, `contains`, `starts_with`,
`ends_with`, `range`, `multiple_of`, `positive`, `non_negative`, `nested` and `enum_values`. Anything
else is a compile error that tells you where the key belongs. `each(...)` cannot address a map's
entries; use a constrained newtype for the value type instead.

On an enum variant you may use `rename`, `skip`, `title`, `description` and `deprecated`.

## Nesting is not implicit

This is the one that catches everyone. A field whose type is another `Schema` struct is **not**
validated unless you say so.

```rust
/// A postal address.
#[derive(Schema, Debug)]
pub struct Address {
    /// Two-letter country code.
    #[schema(len = 2..=2, pattern = r"^[A-Z]{2}$")]
    pub country: String,
}

#[derive(Schema, Debug)]
pub struct SignUp {
    /// Where the account holder lives.
    #[schema(nested)]
    pub address: Address,
    /// Previous addresses.
    #[schema(each(nested))]
    pub history: Vec<Address>,
}
```

Without `#[schema(nested)]`, `SignUp::validate` returns `Ok(())` for an address whose `country` is
lower case. The inner constraints still reach the OpenAPI document, because that comes from the
`$ref`, so the symptom is a server that accepts what its own documentation forbids.

> [!WARNING]
> `nested` on a `Vec<Inner>` does nothing useful. Use `each(nested)`. The same applies to any
> collection: the attribute has to name the element, not the field.

When `nested` is on, the inner pointers are lifted into the outer namespace, so a bad country is
reported at `/address/country` and the second bad history entry at `/history/1/country`.

## Optional fields and defaults

Three different things make a property optional, and they are not interchangeable.

- `Option<T>` produces `"type": ["integer", "null"]` and drops the field from `required`. The client
  may send `null` or omit the member. Constraints still apply to the inner value when one is present.
- `#[schema(default)]` or `#[schema(default = expr)]` drops the field from `required` and writes the
  value into the schema's `default` keyword. The field type stays non-nullable, so the client may
  omit the member but may not send `null`.
- `#[schema(read_only)]` drops the field from `required` because the client is never expected to
  send it.

A missing member with none of these is a **400**, not a 422: serde cannot build the struct at all,
so it is a read failure rather than a rule failure. It still carries a pointer and the code
`required`. That split is explained in [validation](./validation.md#400-versus-422).

## Flattening

`#[schema(flatten)]` moves the inner struct's fields to the parent's JSON object and composes the
schemas with `allOf`.

```rust
/// Common paging fields.
#[derive(Schema, Debug)]
pub struct Paging {
    /// How many rows.
    #[schema(range = 1..=100, default = 20)]
    pub limit: u32,
}

/// A listing request.
#[derive(Schema, Debug)]
pub struct Listing {
    /// Paging.
    #[schema(flatten)]
    pub paging: Paging,
    /// Repeatable, or one comma-separated value.
    #[schema(delimiter = ",")]
    pub tags: Vec<String>,
}
```

`delimiter` accepts `","`, `"|"` or `" "`. It is what makes `?tags=a,b` work as well as
`?tags=a&tags=b` in a [`Query<T>`](./extractors.md) type.

`Listing` has `properties: {tags}`, `required: ["tags"]` and
`allOf: [{"$ref": "#/components/schemas/Paging"}]`. Both `{"limit":5,"tags":"a,b"}` and
`{"limit":5,"tags":["a","b"]}` deserialise to the same value, because the delimited helpers accept a
sequence as well as a string.

Two constraints on flattening. `deny_unknown` and `flatten` on the same type are a compile error,
because serde cannot enforce both. And a flattened field's pointer becomes the root, so an error
inside `Paging` is reported at `/limit`, not at `/paging/limit`.

## Read only, write only, and secret

`read_only` is for server-assigned values: an id, a `created_at`. The field is serialised but never
deserialised, so a client that sends it is ignored rather than rejected.

`write_only` is for values a client sends but must never receive back. `secret` implies `write_only`
and adds one more thing: this struct's generated `Debug` prints `[redacted]` for the field, so it
cannot reach a log line through the struct.

```rust
use moso::prelude::*;
use moso::schema::Password;

/// A sign-up request.
#[derive(Schema)]                 // no `Debug` here: the derive writes one
#[schema(rename_all = "camelCase")]
pub struct SignUp {
    /// Public handle.
    #[schema(len = 3..=32, lowercase)]
    pub user_name: String,
    /// The chosen password.
    #[schema(secret, len = 12..)]
    pub password: Password,
}
```

Two rules the compiler enforces here, and both are worth knowing before you meet them.

**`secret` is refused on a leaky type.** `#[schema(secret)]` redacts *this* struct's `Debug` and
nothing else, so a `String` behind it still has `Display`, still has `AsRef<str>`, and still formats
itself into the first `tracing::info!` that touches it. The macro rejects `secret` on `String`,
`str`, `Cow`, `Box`, `Vec`, `PathBuf` and `Path`:

```text
error: `#[schema(secret)]` needs a secret type, and `String` is not one

       note: `secret` redacts this struct's `Debug`; the `String` itself still prints everywhere else
       note: `Password` is the inbound shape; `SecretString` is the one to hold and compare
```

**A struct with a secret field cannot also derive `Debug`.** The Schema derive emits its own
redacting `Debug`, so adding the standard derive is a conflicting-implementation error (E0119). The
compiler will not tell you why; remove `Debug` from the derive list.

`Password` earns its place: it has no `Display`, no `AsRef<str>` and no `Deref<Target = str>`, its
`Serialize` always fails, its `Debug` prints `Password(***)`, and its buffer is zeroed on drop as
best effort without `unsafe`. Read the plaintext with `expose()` at the one call site that hashes it.

## Enums

All four serde representations work, and each produces the schema a client generator expects.

```rust
use moso::prelude::*;
use uuid::Uuid;

/// Externally tagged, the serde default.
#[derive(Schema, Debug)]
#[schema(rename_all = "snake_case")]
pub enum Status { Draft, Published }

/// Internally tagged.
#[derive(Schema, Debug)]
#[schema(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Created { id: Uuid },
    Deleted { id: Uuid, #[schema(len = 1..=200)] reason: String },
}

/// Adjacently tagged, and untagged.
#[derive(Schema, Debug)]
#[schema(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Adjacent { First(u32), Second(String) }

#[derive(Schema, Debug)]
#[schema(untagged)]
pub enum Either { Number(u32), Text(String) }
```

| Representation | Emitted schema |
| --- | --- |
| Unit-only enum | `{"type": "string", "enum": [...]}`, and `HAS_CONSTRAINTS` is **false** |
| External (default) | `oneOf` of single-property objects with `additionalProperties: false`; a unit variant becomes a bare `const` |
| Internal (`tag`) | `oneOf` of `$ref`s to registered components (`Event_Created`, `Event_Deleted`) plus a `discriminator` with an explicit mapping. Each arm carries the tag as a required `const` property. |
| Adjacent (`tag` + `content`) | `oneOf` of inline objects with the tag and content properties, both required |
| Untagged | `oneOf` of the raw payload schemas, with no wrapper |

Registering each arm of an internally tagged enum as its own component is what makes a generated
TypeScript client produce a real discriminated union rather than a bag of optional fields.

Two limits. An internally tagged enum with a multi-field tuple variant is a compile error naming the
variant, because serde cannot represent it. And for an untagged enum the macro only rejects variants
with identical structural signatures: two struct variants where one's fields are a subset of the
other's compile fine and are ambiguous at runtime, exactly as they would be with plain serde.

## Generics and recursion

A generic model gets a `T: Schema` bound on every generated impl. That one bound covers all four
traits, because `Schema` is a subtrait of `Serialize + DeserializeOwned + Validate + Send + Sync +
'static`.

```rust
#[derive(Schema)]
pub struct Page<T> {
    pub items: Vec<T>,
}
```

The component name is mangled by a public, documented function: `Page<UserOut>` becomes
`Page_UserOut`, `Either<A, B>` becomes `Either_A_B`, and an anonymous argument becomes `Anonymous`.
It is public precisely because generated client type names depend on it and must be predictable. You
can call it directly as `moso::schema::generic_schema_name`.

Recursive types terminate. The generator reserves a name before generating a body, so a type that
refers to itself sees its own reservation and emits a `$ref`: a
`struct Category { name: String, children: Vec<Category> }` produces
`children.items` as `{"$ref": "#/components/schemas/Category"}`. Mutual recursion works the same way.

> [!IMPORTANT]
> Two distinct Rust types that claim the same `schema_name` are a **boot error**, not a silent
> overwrite. The generator collects collisions and the OpenAPI builder reports both offenders by
> name when `App::build()` runs. Use `#[schema(rename = "...")]` to disambiguate.

## Converting from a domain type

Entities deliberately do not implement `Schema`, so your database columns cannot become your API
contract by accident. The bridge is `#[schema(from = ...)]`, which generates the `From` impl by
field-name matching and fails to compile when a field is missing or mistyped.

```rust title="src/models/post.rs"
use chrono::{DateTime, Utc};
use moso::prelude::*;

/// A post, as the API accepts one.
#[derive(Schema, Debug, Clone, PartialEq, Eq)]
pub struct CreatePost {
    /// Headline, shown in listings.
    #[schema(len = 3..=200, trim)]
    pub title: String,

    /// Free-form tags, at most five, each a short lower-case word.
    #[schema(default, len = ..=5, each(len = 2..=20, pattern = r"^[a-z0-9-]+$"))]
    pub tags: Vec<String>,

    /// Publish immediately instead of saving a draft.
    #[schema(default = false)]
    pub publish: bool,
}

/// A post, as the API returns one.
#[derive(Schema, Debug, Clone, PartialEq, Eq)]
#[schema(from = Post)]
pub struct PostOut {
    /// Primary key.
    pub id: Id<Post>,
    /// The URL-safe name.
    pub slug: Slug,
    /// Headline.
    pub title: String,
    /// When it went public, or `null` while it is a draft.
    pub published_at: Option<DateTime<Utc>>,
}
```

Renaming a field on `Post` then breaks the build rather than the API. The `#[entity(expose)]` escape
hatch is how you opt a field back in when you mean to.

## Opting out and hand-written impls

`#[schema(no_serde)]` suppresses the generated serde impls; `#[schema(no_response)]` suppresses
`IntoResponse` and `Describe`. Deriving `Responder` implies the latter.

For a foreign type, or for a shape the derive cannot express, implement `Schema` by hand with the
builders. This is how `Page<T>` itself is written, abridged to one property (the real impl also
describes `next_cursor`, `prev_cursor` and `total`):

```rust
impl<T: Schema> Schema for Page<T> {
    fn schema_name() -> Cow<'static, str> {
        generic_schema_name("Page", &[T::schema_name()])
    }

    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
        let items = generator.subschema_for::<T>();

        ObjectBuilder::named(Self::schema_name())
            .description("One page of results, in Moso's standard pagination envelope.")
            .property("items", ArrayBuilder::new().items(items).build(), true)
            .build()
    }

    const HAS_CONSTRAINTS: bool = T::HAS_CONSTRAINTS;
}
```

Always reach sub-schemas through `generator.subschema_for::<T>()` rather than calling
`T::json_schema` directly: that is what registers named components and what makes the recursion guard
work. `ObjectBuilder`, `StringBuilder`, `NumberBuilder` and `ArrayBuilder` all live in
`moso::schema`.

## Compile errors you will meet

The derive never aborts on the first mistake. It records a diagnostic, swallows the offending value
and keeps parsing, so you see every error in one build.

| What you wrote | What you get |
| --- | --- |
| `#[schema(lenght = 3..=32)]` | ``unknown `schema` attribute `lenght` `` with ``help: did you mean `len`?`` |
| `#[schema(pattern = r"^[a-z0-9_+$")]` | `this regular expression does not compile: unclosed character class`, underlined on the literal |
| `#[schema(pattern = ...)]` on an integer | ``a text constraint needs a string; `page` is an integer`` |
| `#[schema(range = ...)]` on a string | ``a numeric constraint needs a number; `query` is a string`` |
| `#[schema(len = 32..=3)]` | ``this length range is empty: 32 is greater than 3`` with ``help: did you mean `len = 3..=32`?`` |
| `#[schema(positive, non_negative)]` | ``` `positive` and `non_negative` say different things ```, and `non_empty` beside a `len` with a minimum is rejected the same way |
| `deny_unknown` with a `flatten`ed field | ``` `deny_unknown` and `flatten` cannot both be used ``` |
| A lifetime parameter | `a schema type cannot borrow`, because `Schema` requires `'static` |
| `#[derive(Schema)]` on a union | ``` `Schema` cannot be derived for a union ```, with an enum shown as the fix |

An unknown `format` name is not an error: JSON Schema says an unknown format is an annotation, so
Moso emits it and does not enforce it. A *near miss* of a known format is an error with a suggestion,
because that is almost always a typo.

## Edges to know

- `#[schema(flatten_bracket)]` is reserved for forward compatibility: the key is accepted and stored,
  but no emitter acts on it yet. You do not need it either, because bracketed query parameters already
  work through the query map's generic bracket handling rather than through this attribute.
- `deny_unknown` on a JSON body produces a **400**, not a 422. It expands to serde's
  `deny_unknown_fields`, and serde's rejection is classified as a read failure. Only the query
  deserialiser maps an unknown field to a 422, with the code `custom:unknown_field`.
- The derive misses its own generated-code size budget: roughly 34 to 44 lines per field against a
  25-line target. It costs compile time on very wide structs.

## See also

- [Validation](./validation.md) for constrained types, cross-field checks and the wire shape of a failure.
- [Extractors](./extractors.md) for where `validate` is actually called.
- [OpenAPI](./openapi.md) for how the generated schemas reach the document.
