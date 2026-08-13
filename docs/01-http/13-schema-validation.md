# 13 - `#[derive(Schema)]`: One Model, Three Jobs

> **Status: implemented.** Two signature corrections are marked inline below; everything else in
> this document describes what was built.

> This is the headline feature. If Moso does nothing else well, it must do this better than
> anything else in Rust.

## The problem

Today, a Rust type that is a validated, documented API model needs:

```rust
#[derive(Serialize, Deserialize, ToSchema, Validate)]   // three ecosystems
pub struct CreateUser {
    #[validate(length(min = 3, max = 32))]              // runtime rule
    #[schema(min_length = 3, max_length = 32)]          // doc rule - must match by hand
    pub username: String,
    #[validate(email)]
    #[schema(format = "email")]
    pub email: String,
}
```

The constraint is written twice, in two vocabularies, with no compiler check that they agree. They
drift. The docs lie. Moso's answer: **one attribute vocabulary, one derive, and the OpenAPI
constraint is generated from the validation rule** - they cannot disagree because there is only one.

## The target

```rust
// example
#[derive(Schema)]
pub struct CreateUser {
    /// Public handle. Lowercase letters, digits and underscores.
    #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
    pub username: String,

    pub email: Email,                       // constrained type: format + validation come with it

    #[schema(secret, len = 12..)]
    pub password: Password,                 // redacted in Debug/logs, never serialised

    #[schema(range = 13..=130)]
    pub age: Option<u8>,

    #[schema(default = "Locale::EN")]
    pub locale: Locale,                     // enum → OpenAPI enum

    #[schema(len = ..=10, each(len = 1..=24))]
    pub tags: Vec<String>,

    #[schema(nested)]                       // validate the inner struct too
    pub address: Address,
}
```

Produces, from one derive: `Serialize`, `Deserialize`, `Validate`, `Schema` (which is where the JSON
Schema lives - there is no separate `JsonSchema` trait), `IntoResponse` + `Describe` so the type can
be returned from a handler (decision D9 - a blanket `impl<T: Schema> IntoResponse for T` would
violate the orphan rule), and a `Debug` impl that redacts `#[schema(secret)]` fields.

**The JSON Schema model lives in `moso-schema`**, not in `moso-openapi` (decision D2):
`moso_schema::json_schema::{SchemaNode, SchemaGenerator, SchemaRef, ObjectBuilder, StringBuilder,
NumberBuilder, ArrayBuilder}`. `moso-openapi` depends on this crate and embeds what it produces.

## The traits

```rust
// spec - moso-schema/src/lib.rs

pub trait Schema: Serialize + DeserializeOwned + Validate + Send + Sync + 'static {
    /// Stable name used for `$defs` and generated client type names.
    fn schema_name() -> Cow<'static, str>;
    /// Emit into the generator, returning either an inline schema or a `$ref`.
    /// NOTE: the parameter is `generator`. `gen` is a RESERVED KEYWORD in edition 2024.
    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode;
    /// Cheap reference for use in operation descriptions.
    fn schema_ref() -> SchemaRef { SchemaRef::inline_or_named(Self::schema_name()) }
    /// True if any field carries a constraint ⇒ a 422 is possible.
    const HAS_CONSTRAINTS: bool = false;
}

pub trait Validate {
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors>;
}

pub struct ValidationErrors(SmallVec<[FieldError; 4]>);

pub struct FieldError {
    /// RFC 6901 JSON Pointer: "/address/postcode", "/tags/2"
    pub pointer: String,
    /// Machine-readable, stable, documented: "required", "len", "range", "pattern",
    /// "format", "type", "enum", "unique", "custom:<name>"
    pub code: Cow<'static, str>,
    /// Human message, localisable.
    pub message: Cow<'static, str>,
    /// Constraint parameters, for clients that render their own messages.
    pub params: BTreeMap<&'static str, serde_json::Value>,
}
```

## The `#[schema(...)]` attribute vocabulary

One vocabulary; each entry generates **both** the runtime check and the JSON Schema keyword.

### String
| Attribute | Runtime | JSON Schema |
| --- | --- | --- |
| `len = 3..=32` | char-count check (not bytes) | `minLength`/`maxLength` |
| `pattern = "..."` | compiled once, `OnceLock<Regex>` | `pattern` |
| `format = "uuid"` | delegated to the format registry | `format` |
| `trim` | trims on deserialise | - (documented in description) |
| `lowercase` / `uppercase` | normalises on deserialise | - |
| `non_empty` | `len = 1..` | `minLength: 1` |
| `contains = "x"` / `starts_with` | substring check | `pattern` (escaped) |

### Numeric
| Attribute | Runtime | JSON Schema |
| --- | --- | --- |
| `range = 1..=100` | bounds check | `minimum`/`maximum` |
| `range = 0.0..1.0` | exclusive upper | `exclusiveMaximum` |
| `multiple_of = 5` | modulo check | `multipleOf` |
| `positive` / `non_negative` | | `exclusiveMinimum: 0` / `minimum: 0` |

### Collections
| Attribute | Runtime | JSON Schema |
| --- | --- | --- |
| `len = 1..=10` | length | `minItems`/`maxItems` |
| `unique` | duplicate detection | `uniqueItems: true` |
| `each(...)` | applies the inner rules to every element, pointer `/tags/2` | `items: {...}` |

### Structural
| Attribute | Effect |
| --- | --- |
| `nested` | recurse into the field's `Validate`; pointers compose |
| `default = expr` | serde default **and** OpenAPI `default` **and** it is documented as optional |
| `rename = "x"` / `rename_all = "camelCase"` | serde rename mirrored into the schema |
| `skip` | absent from both serde and schema |
| `read_only` / `write_only` | OpenAPI `readOnly`/`writeOnly`; enforced on deserialise |
| `secret` | redacted `Debug`, `write_only`, never logged, zeroised on drop where possible |
| `deprecated = "use `x` instead"` | OpenAPI `deprecated` + description note |
| `example = expr` | OpenAPI `examples` |
| `flatten` | serde flatten + `allOf` composition |
| `deny_unknown` (container) | unknown fields → 422 instead of ignored |
| `from = OtherType` (container) | generate `From<OtherType>` by field-name matching |
| `title` / `description` | override; doc comments are the default source |

### Cross-field and custom
```rust
// example
#[derive(Schema)]
#[schema(check = "passwords_match")]
pub struct SignUp {
    pub password: Password,
    pub password_confirm: Password,
}

fn passwords_match(v: &SignUp, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
    if v.password != v.password_confirm {
        return Err(ValidationErrors::one("/password_confirm", "custom:match",
            "passwords do not match"));
    }
    Ok(())
}
```

Async / IO-backed validation (e.g. "email is unique") is **deliberately not supported here**. It
belongs in the service layer, inside the transaction that will enforce it, because a check-then-act
validation is a race condition. The docs say this explicitly and show the correct pattern
(unique constraint → `Error::Conflict` → 409 with a field pointer).

## Constrained types - parse, don't validate

Attributes are convenient; types are correct. Moso ships newtypes whose invariant is guaranteed by
construction, so an invalid value cannot exist anywhere in the program - the property Pydantic gets
from validate-on-construct.

```rust
// spec - moso-schema/src/types.rs
pub struct Email(String);          // RFC 5322-lite + domain sanity; format: "email"
pub struct Url(url::Url);          // format: "uri"
pub struct Slug(String);           // ^[a-z0-9]+(-[a-z0-9]+)*$
pub struct Password(SecretString); // secret, min length policy, zeroised
pub struct PhoneE164(String);
pub struct Hostname(String);
pub struct IpCidr(ipnet::IpNet);
pub struct NonEmpty<T>(T);                        // String, Vec<_>, …
pub struct Bounded<T, const MIN: i64, const MAX: i64>(T);
pub struct Length<T, const MIN: usize, const MAX: usize>(T);
pub struct Trimmed(String);
pub struct Sanitised<P: SanitisePolicy>(String);  // HTML sanitisation for user content
pub struct Cursor(/* opaque */);
pub struct Id<E: Entity>(Uuid);                   // typed ids: Id<User> ≠ Id<Post>
```

Each: `Deserialize` enforces the invariant and yields a `FieldError` with the right `code`;
`Schema` emits the matching keyword; `Display`/`AsRef<str>`/`Deref` make them pleasant;
`TryFrom<String>` and `FromStr` exist for construction in code.

`Id<E>` deserves emphasis: it makes `fn get(id: Id<User>)` un-callable with a `Id<Post>`, which
eliminates an entire class of production bug and costs nothing at runtime.

**Defining your own** is a documented, one-derive path:

```rust
// example
#[derive(Constrained)]
#[constrained(inner = String, pattern = r"^ORD-\d{8}$", format = "order-number")]
pub struct OrderNumber(String);
```

## Enums

```rust
// example
#[derive(Schema)]
#[schema(rename_all = "snake_case")]
pub enum Status { Draft, Published, Archived }        // → OpenAPI enum of strings

#[derive(Schema)]
#[schema(tag = "kind", rename_all = "snake_case")]     // internally tagged
pub enum Event {
    Created { id: Uuid },
    Deleted { id: Uuid, reason: String },
}                                                      // → oneOf + discriminator
```

Moso supports serde's four enum representations and maps each to the correct OpenAPI 3.1
construction, including `discriminator` for internally-tagged enums (which is what makes generated
TypeScript clients produce a real union type rather than `any`).

## Generic and reference-heavy models

- `Schema` is implemented for `Option<T>`, `Vec<T>`, `HashMap<String, T>`, `BTreeMap`, `HashSet`,
  arrays, tuples up to 12, `Box<T>`, `Arc<T>`, `Cow<'static, T>`.
- Generic user types are supported: `Page<T>` names itself `Page_UserOut` in `$defs`. The naming
  function is documented and stable, because generated client type names depend on it.
- Recursive types work via `$ref` cycles; the generator detects cycles and emits refs rather than
  recursing (a test with a `Category { children: Vec<Category> }` guards this).

## Error output shape (normative)

A validation failure is an RFC 9457 problem with a Moso extension member:

```json
{
  "type": "https://moso.rs/errors/validation",
  "title": "Validation failed",
  "status": 422,
  "detail": "2 fields are invalid",
  "instance": "/api/v1/users",
  "errors": [
    { "pointer": "/username", "code": "pattern",
      "message": "must match ^[a-z0-9_]+$",
      "params": { "pattern": "^[a-z0-9_]+$" } },
    { "pointer": "/tags/2", "code": "len",
      "message": "must be between 1 and 24 characters",
      "params": { "min": 1, "max": 24 } }
  ],
  "request_id": "01J8..."
}
```

Requirements:
- `pointer` is always a valid JSON Pointer into the request body (or `/query/limit`,
  `/path/id`, `/header/x-tenant` for non-body sources).
- `code` values are a closed, documented set. Adding one is a minor version change; changing one is
  a breaking change. Clients branch on `code`, never on `message`.
- All failing fields are reported, not just the first. (Configurable cap, default 50.)
- `message` is localisable via a `MessageProvider` in the app; the default is English.

## Localisation

```rust
// spec
pub trait MessageProvider: Send + Sync + 'static {
    fn message(&self, code: &str, params: &BTreeMap<&str, Value>, locale: &Locale) -> Option<String>;
}
```
Registered with `.provide_dyn::<dyn MessageProvider>(...)`. The default provider reads a bundled
Fluent file; apps can override per-code. `Accept-Language` selects the locale unless a `Depends`
sets it explicitly.

## Implementation notes for the macro author

1. **Field-attribute parsing** uses `darling`. Unknown keys are a compile error with a suggestion
   (`unknown attribute 'lenght' - did you mean 'len'?`).
2. **Ranges** are parsed from real Rust range syntax (`3..=32`, `1..`, `..=10`) so they cannot be
   malformed strings.
3. **Regexes are validated at compile time** by attempting to parse them in the proc macro; an
   invalid pattern is a compile error pointing at the literal.
4. **`HAS_CONSTRAINTS`** is a const computed by the macro; it drives whether a 422 is documented,
   so a constraint-free DTO does not pollute the OpenAPI with impossible responses.
5. **Generated code size budget:** ≤ 25 lines of expansion per field average, ≤ 300 lines for a
   20-field struct. Measured by `xtask expand-size`. Validation bodies must not monomorphise per
   call site - they take `&mut ValidationCtx` (a concrete type), not generics.
6. **No `regex` dependency unless a `pattern` is used** - the macro emits a `cfg`-gated path and
   `moso-schema`'s `regex` dep is optional but enabled by the derive's feature detection. If this
   proves impossible with feature unification, prefer `regex-lite` for the default path.

## Comparison to prior art (state honestly in the docs)

| | Pydantic | validator | garde | utoipa | **Moso Schema** |
| --- | --- | --- | --- | --- | --- |
| One declaration for validation + docs | ✅ | ❌ | ❌ | ❌ | ✅ |
| Field-pathed errors | ✅ | ⚠️ flat | ✅ | n/a | ✅ |
| Nested/collection element rules | ✅ | ⚠️ | ✅ `dive` | n/a | ✅ `nested`/`each` |
| Validate-on-construct guarantee | ✅ | ❌ | ❌ | n/a | ✅ via constrained types |
| Custom context | ✅ | ⚠️ | ✅ | n/a | ✅ `ValidationCtx` |
| Generates OpenAPI | ✅ | ❌ | ❌ | ✅ | ✅ |

We credit garde and utoipa explicitly in the docs; both solved parts of this well and we are
integrating their lessons, not dismissing them.

## Acceptance criteria (WP-05)

1. Every attribute in the vocabulary tables has: a runtime test, a JSON Schema snapshot test, and a
   documented `code`.
2. A struct with a `#[schema(len = 3..=32)]` field produces `minLength: 3, maxLength: 32` in the
   generated schema **and** rejects a 2-char value with `code: "len"`. Same test, one struct.
3. `#[derive(Schema)]` on an unsupported shape (untagged enum with ambiguous variants, unit struct
   with attributes) gives a hand-written compile error.
4. `Password` never appears in `Debug`, `Display`, `Serialize`, or a tracing field. Guarded by a
   test that formats a struct and greps for the secret.
5. Recursive and generic models produce valid, resolvable OpenAPI 3.1 (validated with a real
   JSON Schema 2020-12 validator in CI).
6. A 20-field struct's derive adds ≤ 40 ms to a clean compile on the reference machine.
