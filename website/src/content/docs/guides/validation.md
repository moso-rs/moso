---
title: Validation
description: Reject bad input at the boundary with constrained types and attributes, and return one RFC 9457 problem document naming every field that failed.
order: 6
status: shipped
---

Moso validates at the edge of your program, not inside it. An extractor deserialises the request,
runs the model's `Validate` implementation, and either hands your handler a value that has already
satisfied every rule or returns a `422` naming every field that did not. There is no `.validate()?`
line to forget, because there is no code path that produces an unvalidated `T: Schema`.

There are two ways to express a rule and you should reach for them in this order. A **constrained
type** such as `Email` or `Slug` protects every value of that type everywhere in the program,
including values built in code that never saw a request. A `#[schema(...)]` **attribute** protects
one field of one struct. Prefer the type where one exists; write the attribute when the rule really
does belong to that field.

> [!NOTE]
> Two edges worth knowing, both flagged where they bite: a truncated error set reports *that* it was
> truncated but not the dropped count, and the typed side channel on `ValidationCtx` is a reserved
> surface you populate yourself. The `MessageProvider` extension point **is** wired into the request
> path. See [custom messages](#custom-messages-and-the-message-provider).

## The failure, end to end

```rust title="src/routes/users.rs"
use moso::prelude::*;

/// Everything the sign-up endpoint accepts.
#[derive(Schema, Debug, Clone)]
pub struct SignUp {
    /// Public handle.
    #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
    pub username: String,
    /// Contact address.
    pub email: Email,
    /// Age in years.
    #[schema(range = 13..=130)]
    pub age: u8,
    /// Interests, each a non-empty tag, with no repeats.
    #[schema(unique, each(len = 1..=16))]
    pub tags: Vec<String>,
}

/// Accept a sign-up. Reached only when the body already validated.
#[endpoint]
async fn sign_up(Json(body): Json<SignUp>) -> Result<Json<String>> {
    Ok(Json(body.username))
}
```

Post a body that breaks three fields, one of them twice:

```json
{ "username": "A", "email": "ada@example.com", "age": 9, "tags": ["maths", "maths"] }
```

and you get `422 Unprocessable Entity`, `content-type: application/problem+json`, and this:

```json
{
  "type": "https://moso.rs/errors/validation",
  "title": "Validation Failed",
  "status": 422,
  "detail": "4 fields failed validation",
  "instance": "/sign-up",
  "errors": [
    { "pointer": "/username", "code": "len",
      "message": "must be between 3 and 32 characters",
      "params": { "min": 3, "max": 32 } },
    { "pointer": "/username", "code": "pattern",
      "message": "must match ^[a-z0-9_]+$",
      "params": { "pattern": "^[a-z0-9_]+$" } },
    { "pointer": "/age", "code": "range",
      "message": "must be between 13 and 130",
      "params": { "min": 13, "max": 130 } },
    { "pointer": "/tags/1", "code": "unique",
      "message": "must not contain duplicate values" }
  ],
  "request_id": "01J…"
}
```

Four things about that document are load bearing.

- **Every failing rule is reported, not the first.** A client that has to fix one field per
  round-trip shows one error per round-trip to a human.
- **`pointer` is an RFC 6901 JSON Pointer** into the request, so a form can attach the message to the
  input that produced it. `unique` points at the duplicate's index, not at the array.
- **`code` is from a closed set** and is the part of the contract clients match on. `message` is
  human-readable and may change.
- **`params` carries the constraint's numbers**, so a client that wants its own wording can render
  "between 3 and 32" without parsing English.

`params` is omitted entirely when the check has no parameters. `errors` is a Moso extension to
RFC 9457, as are `request_id` and `trace_id`.

## Error codes

| Code | Raised by |
| --- | --- |
| `required` | A missing member serde could not default (arrives as a 400, see below) |
| `type` | A value of the wrong JSON type, or a path segment that will not parse |
| `len` | `len`, `non_empty`, and the length limits of `Email`, `Slug`, `Cursor`, `Hostname`, `Password`, `Length`, `NonEmpty` |
| `range` | `range`, `positive`, `non_negative`, `Bounded`, an out-of-range CIDR prefix |
| `pattern` | `pattern`, `contains`, `starts_with`, `ends_with`, `Slug`, `PhoneE164` |
| `format` | `format`, and the format-checked constrained types |
| `enum` | `enum_values` |
| `unique` | `unique`, reported at the first duplicate's index |
| `multiple_of` | `multiple_of` |
| `custom` | A hand-written error using `codes::CUSTOM` |
| `custom:*` | Anything you invent, prefixed with `custom:` |

They live at `moso::schema::codes`, and `codes::ALL` is the closed list. Branch on the constant, not
on the string literal.

Parameter keys per code:

| Code | Keys |
| --- | --- |
| `len` | `min`, `max`, and `unit` only when it is `items`; `characters` is the default and is not sent |
| `range` | `min`, `max`, `exclusive_min`, `exclusive_max` |
| `pattern` | `pattern`, or one of `starts_with` / `ends_with` / `contains` |
| `format` | `format` |
| `enum` | `allowed` |
| `multiple_of` | `multiple_of` |
| `type` | `expected` |

## 400 versus 422

The two failures are told apart by status alone, and the difference is whether the value could be
*read*. This table is about a JSON body; the non-body sources are covered below and behave
differently.

| Situation | Status | Notes |
| --- | --- | --- |
| `{not json at all` | 400 | Nothing to point at |
| `"age": "thirty-six"` | 400 | Well-formed JSON, wrong shape. Still carries a pointer at `/age`. |
| A required member absent | 400 | Code `required` at the member's pointer. The struct could not be built at all. |
| `"age": 9` against `range = 13..=130` | 422 | Read fine, broke a rule |
| `"email": "nope"` | 422 | A constrained type rejects during deserialisation and is promoted to a 422 |
| `content-type: text/plain` | 415 | The 415 names what was sent |
| A body over `http.body_max` | 413 | The cap is enforced while reading, so an oversized payload costs the cap and not the whole upload |
| A body nesting past `http.json_depth_max` | 400 | A byte scan runs before `serde_json` does, so `[[[[…` is refused before a tree is built. The document carries `max_depth`. |

A missing required field being a **400** surprises people. It is deliberate: serde cannot construct
the struct, so nothing ran the rules. The response still names the field.

The promotion in the fifth row is worth understanding. `Email::new` fails inside `Deserialize`, and
`serde::de::Error` has no room for structured data, so the constrained type encodes its failure into
the message with the prefix `moso.constraint:`. The extractor recognises that prefix, recovers the
code and the message, and reports a 422 rather than a 400. So `{"email": "nope"}` reports `format` at
`/email`, while `{"email": 7}` reports `type` and a 400. A promoted error carries no `params`,
because the string protocol has nowhere to put them.

A body with **no** `content-type` at all is accepted: too many clients omit the header, and refusing
them buys nothing the parse does not already buy. A `+json` suffix type such as
`application/merge-patch+json` is accepted too.

## Where the pointers are rooted

`Json` and `Form` report pointers relative to the body root. `Query`, `Path` and `Headers` root their
*deserialisation* failures at `/query`, `/path` and `/header`, so an unparseable path segment is
reported under `/path` with the code `type`.

> [!WARNING]
> A derived `Validate` body writes its field pointers as literal strings, so a *constraint* failure
> on a query or path type is reported at `/limit`, not `/query/limit`. Deserialisation failures on
> the same type do get the `/query` root. If you match on exact pointers in a client, match on both
> or match on the trailing segment.

The non-body sources also differ on status. `Query`, `Path`, `Headers` and `Form` share one
deserialiser, and it reports **every** read failure as a 422: a missing query parameter is a 422 with
the code `required`, and an unparseable one is a 422 with the code `type`. Only the JSON body reader
produces the 400s in the table above.

Under `#[schema(deny_unknown)]`, an unknown query parameter is a 422 with the code
`custom:unknown_field`. The same attribute on a JSON body gives a 400 instead, because there it is
serde's `deny_unknown_fields` raising a read failure. Without `deny_unknown`, an unknown member is
ignored by both.

## Constrained types

These live in `moso::schema`. Only `Cursor`, `Email`, `Id` and `Slug` are in the prelude; the rest
need an explicit path. All of them validate in their constructor and route `Deserialize` through it,
so an invalid value never exists.

| Type | Constructors | Enforces | Failure code |
| --- | --- | --- | --- |
| `Email` | `new`, `new_unchecked`; `local_part`, `domain` | RFC-shaped address, 3 to 254 characters, local part at most 64 | `format`, `len` |
| `Password` | `new`, `with_min_length`, `from_trusted`; `expose` | 12 to 256 characters | `len` |
| `Slug` | `new`, `slugify`, `from_title`, `unique_from` | `^[a-z0-9]+(-[a-z0-9]+)*$`, 1 to 128 characters | `pattern`, `len` |
| `Url` | `parse`, `parse_with_schemes`, `parse_http` | A parseable absolute URL | `format` |
| `Cursor` | `from_bytes`, `decode`, `encode` | Canonical base64url, at most 2048 characters | `format`, `len` |
| `Id<E>` | `new`, `new_v7`, `nil`, `parse`, `from_uuid`, `cast` | A UUID, typed by its marker | `format` |
| `Hostname` | `new`, `new_unchecked`; `labels` | RFC 1123, at most 253 characters, no trailing dot | `format`, `len` |
| `IpCidr` | `new`, `parse`; `contains`, `network`, `prefix_len` | An address and a legal prefix length | `format`, `range` |
| `PhoneE164` | `new`, `new_unchecked`; `digits` | `^\+[1-9]\d{1,14}$` | `format` |
| `Trimmed` | `new_trimmed`, `new` | Nothing; trims on construction | never fails |
| `Sanitised<P>` | `new` | Nothing; applies `P` on construction | never fails |
| `NonEmpty<T>` | `new`, `get`, `into_inner` | At least one character or element | `len` |
| `Bounded<T, MIN, MAX>` | `new`, `clamped`, `get` | An integer in `MIN..=MAX` | `range` |
| `Length<T, MIN, MAX>` | `new`, `get`, `into_inner` | A length in `MIN..=MAX` | `len` |

Using one of these puts the rule in the type, so no attribute is needed and the schema still carries
the keyword:

```rust
use moso::prelude::*;
use moso::schema::{Bounded, Length, NonEmpty, Sanitised, StripTags, Url};

/// A typed wrapper set.
#[derive(Schema, Debug)]
pub struct Typed {
    /// A username with the bound in its type.
    pub username: Length<String, 3, 32>,
    /// A page size with the bound in its type.
    pub page_size: Bounded<u16, 1, 100>,
    /// At least one tag.
    pub tags: NonEmpty<Vec<String>>,
    /// User-supplied comment, cleaned on receipt.
    pub comment: Sanitised<StripTags>,
    /// Somewhere to go.
    pub link: Url,
}
```

That emits `minLength: 3, maxLength: 32`, `minimum: 1, maximum: 100`, `minItems: 1` and
`format: "uri"` with no `#[schema(...)]` anywhere. `Sanitised<P>` cleans the value on construction;
`StripTags` and `EscapeHtml` ship, and you can write your own by implementing `SanitisePolicy`.

`Id<E>` is worth calling out. It carries a zero-sized marker, so `Id<User>` will not coerce to
`Id<Post>` and crossing the line needs the explicit, greppable `Id::cast`. New identifiers are
UUIDv7.

### Things these types deliberately do not do

- **`Email` does no DNS lookup.** Deserialisation must not do network I/O. It also lowercases the
  domain and leaves the local part alone, so `Ada@example.com` and `ada@example.com` are different
  values.
- **`Slug::slugify` does not transliterate.** `"Über"` becomes `"ber"`. A half-complete table that
  works for German and mangles Greek is worse than an obviously lossy rule.
- **`Slug::unique_from` is a suggestion, not a reservation.** It takes a synchronous predicate, gives
  up after 10,000 attempts and returns the last candidate. The unique index is what enforces
  uniqueness, and a `409` is the right answer when it fires.
- **`Hostname` rejects Unicode.** Punycode it first; the error message says so.
- **`IpCidr::contains` is family-strict.** An IPv4-mapped IPv6 address is not in an IPv4 network,
  because pretending otherwise is how allow-lists get bypassed.
- **`EscapeHtml` is not an HTML sanitiser.** It escapes five characters for element content and
  quoted attribute values. Moso ships no allow-list HTML policy, because a correct one is large and
  adversarial and shipping a half-built one under a reassuring name would be worse than shipping
  none.
- **Every one of them has an unchecked escape hatch** (`new_unchecked`, `from_trusted`, `Id::cast`).
  They are named to be conspicuous in review, and they are real holes in the invariant.
- **Constrained types fail during deserialisation, not during `validate`.** Their `Validate` impl is
  a no-op, so a value built with `new_unchecked` is never caught later.

## Your own constrained type

```rust
use moso::prelude::*;

/// An order number, which cannot exist in an invalid state.
#[derive(Constrained, Debug)]
#[constrained(inner = String, pattern = r"^ORD-\d{8}$", format = "order-number")]
pub struct OrderNumber(String);

assert!(OrderNumber::new("ORD-00000042".to_owned()).is_ok());
// `Deserialize` routes through the constructor, so a bad value never exists.
assert!(serde_json::from_str::<OrderNumber>(r#""nope""#).is_err());
```

One derive gives you `new` (which checks), `new_unchecked`, `into_inner`, `FromStr`, `TryFrom`,
`Serialize`, a `Deserialize` routed through `new`, a trivial `Validate` and a `Schema` carrying the
same constraint. A string newtype also gets `as_str`, `into_string`, `Display`, `AsRef<str>`,
`Deref<Target = str>` and `Borrow<str>`.

`#[constrained(...)]` accepts `inner`, `name`, `len`, `pattern`, `format`, `trim`, `lowercase`,
`uppercase`, `non_empty`, `contains`, `starts_with`, `ends_with`, `range`, `multiple_of`, `positive`,
`non_negative`, `check`, `title`, `description` and `secret`. `unique` and `enum_values` are rejected
with an explanation, because they describe collections rather than values.

`#[constrained(secret)]` suppresses `Display`, `AsRef` and `Deref`, adds a redacting `Debug` and sets
`writeOnly` in the schema.

`check` here is **not** the container `check` from `#[derive(Schema)]`. It is
`fn(&Inner) -> Result<(), ConstraintError>`, it runs inside `new` after the built-in checks, and it
builds its failure with `ConstraintError::new(ErrorCode::Custom("custom:reserved"), "…")`.

## Cross-field checks

A rule that spans two fields cannot be an attribute on either of them. Put it on the container.

```rust
use moso::prelude::*;
use moso::schema::{Password, ValidationCtx, ValidationErrors};

/// A sign-up request.
#[derive(Schema)]
#[schema(rename_all = "camelCase", check = passwords_match)]
pub struct SignUp {
    /// The chosen password.
    #[schema(secret, len = 12..)]
    pub password: Password,
    /// Repeat of the chosen password.
    #[schema(secret, len = 12..)]
    pub password_confirm: Password,
}

/// The cross-field rule: both passwords must agree.
fn passwords_match(value: &SignUp, _ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
    if value.password != value.password_confirm {
        return Err(ValidationErrors::one(
            "/passwordConfirm",
            "custom:match",
            "passwords do not match",
        ));
    }
    Ok(())
}
```

The signature is `fn(&Self, &mut ValidationCtx) -> Result<(), ValidationErrors>`. `check` is
repeatable, and every function's errors are merged into the same set, so a container check does not
suppress the field errors or the other checks.

Note that you write the pointer yourself, and you write it in the **renamed** namespace:
`/passwordConfirm`, not `/password_confirm`. Field checks run in declaration order and container
checks run after all of them, which is the order the `errors` array comes out in.

`ValidationCtx` carries a typed side channel (`insert` and `get`, keyed by `TypeId`) for handing
request-scoped data to a check function. It works, but nothing in the request path populates it
today, so a `check` function sees an empty side channel. The context a check function receives *does*
carry the request's message provider and locale (see
[custom messages](#custom-messages-and-the-message-provider)), so `ctx.message(code, params)` inside
a hand-written check renders in the client's language.

### Rules that are not validation

"Is this email already taken?" is not a validation rule. A check-then-act against a database is a
race, and the correct place for the rule is the transaction that will enforce it. There is
deliberately no `async fn validate`, because it would look correct and be wrong under concurrency.

When a domain failure genuinely should look like a validation failure to the client, build the same
shape by hand:

```rust
fn decode_cursor(cursor: &Cursor) -> Result<PostKey> {
    PostKey::from_cursor(cursor).ok_or_else(|| {
        Error::validation(ValidationErrors::one(
            "/cursor",
            codes::FORMAT,
            "this is not a cursor this API issued; start from the first page",
        ))
    })
}
```

`Error::validation` produces the same `422` with the same `type` URI, so the client cannot tell the
difference and does not have to. See [the error model](./errors.md).

## Building errors by hand

`ValidationErrors` is the `Err` half of every `Validate` return. The pieces you need:

```rust
use moso::schema::{FieldError, ValidationErrors, codes};

let mut errors = ValidationErrors::new();
errors.push(
    FieldError::new("/seats", codes::MULTIPLE_OF, "must be a whole dozen")
        .with_param("multiple_of", 12),
);
errors.merge_prefixed("/order", other_errors);
errors.into_result()?;
```

`ValidationErrors::one` is the shorthand for a single error. `merge` combines two sets;
`merge_prefixed` lifts an inner set's pointers under a prefix, which is what nested validation does.
`moso::schema::field_error` builds a `FieldError` whose message is rendered by the same code the
built-in checks use, so a hand-written error reads like a generated one.

**There is a cap.** A set collects at most `DEFAULT_MAX_ERRORS` (50) entries per walk, and counts
what it dropped rather than forgetting it: `errors.dropped()` and `errors.truncated()`. A malicious
client sending a 10,000-element array should not get a 10,000-entry response body. Raise or lower it
with `ValidationCtx::with_max_errors` or `ValidationErrors::with_max_errors`. The cap is enforced on
`ValidationErrors` as well as on `ValidationCtx`, so a hand-written `Validate` that never consults
the context still cannot produce an unbounded body.

> [!WARNING]
> The dropped count is available in Rust but is not a member of the problem document today. A client
> that receives exactly 50 entries cannot tell whether there were more, and `detail` counts only what
> survived. Do not build an "and N more" affordance on the response alone.

Build a context with `ValidationCtx::new()`. `Default` is written out by hand to be exactly that,
because a derived one would set `max_errors` to zero and silently discard every failure.

## Custom messages and the message provider

Messages are rendered by `moso::schema::DefaultMessages`, a hard-coded English renderer that is
grammatical about quantities: `min == max` gives "must be exactly 8 characters", a lone `min = 1`
gives "must not be empty", and a singular quantity drops the plural.

Replace or translate every message by implementing `MessageProvider` and registering **one**
instance at boot:

```rust title="src/lib.rs"
use moso::prelude::*;
use moso::schema::{Locale, MessageProvider, codes};
use std::collections::BTreeMap;
use std::sync::Arc;
use serde_json::Value;

/// French wording for the codes this application cares about.
pub struct French;

impl MessageProvider for French {
    fn message(
        &self,
        code: &str,
        params: &BTreeMap<&'static str, Value>,
        locale: &Locale,
    ) -> Option<String> {
        if locale.language() != "fr" || code != codes::LEN {
            return None;   // fall through to the bundled English
        }
        let min = params.get("min")?;
        Some(format!("doit contenir au moins {min} caractères"))
    }
}

/// The composition root.
pub fn app() -> Result<AppBuilder> {
    Ok(App::new(AppConfig::default())
        .provide_dyn::<dyn MessageProvider>(Arc::new(French))
        .mount(routes()))
}
```

That is the whole wiring. From then on every validating extractor (`Json`, `Form`, `Query`,
`Headers` and `Path`) builds its `ValidationCtx` through `RequestCtx::validation`, which attaches
the registered provider and the request's locale. A `422` for a request sending
`Accept-Language: fr` carries the French wording; one sending nothing carries the English. No model
and no handler changes.

Returning `None` falls through, and `ChainedMessages` composes several providers with
`DefaultMessages` as the terminal one. `ValidationErrors::localise` rewrites an already-finished set,
for the rare case where the messages were produced somewhere the context could not reach.

### Where the locale comes from

`Accept-Language`, parsed by `Locale::from_accept_language` and read once per validating extractor
via `RequestCtx::locale()`. Quality values are honoured and the highest wins; ties keep the client's
own ordering; `q=0` means "not acceptable" and is skipped; `*` is skipped, because a wildcard is the
same answer as no preference. Anything unusable (a malformed weight, `en_US`, a header that is not
even UTF-8) is **dropped, not rejected**: a bad `Accept-Language` degrades to the default locale and
never fails the request.

There is no registered list of supported locales, so "best match" means "the highest-weighted tag
the client sent". Your provider decides what it can render and returns `None` for the rest, which is
the one place that knowledge actually lives.

> [!NOTE]
> **The cost when you register nothing is one hash lookup.** With no provider registered the
> extractor stops there and does not even parse `Accept-Language`, because with nothing to consult it
> the locale cannot change a single message.

> [!WARNING]
> **There is still no bundled Fluent file, and no `.ftl` loader.** `DefaultMessages` is a hard-coded
> English `match` and English is the only language Moso ships. Translating means writing a
> `MessageProvider`; nothing reads a message catalogue off disk, and no i18n crate is a dependency of
> this workspace.

## Testing validation

`moso-test` asserts against the parsed problem document, so a test says what a client would see:

```rust
#[tokio::test]
async fn an_invalid_body_is_a_422_with_a_field_error() {
    let app = spawn().await;
    app.client()
        .post("/users")
        .json(&json!({ "username": "A", "email": "ada@example.com" }))
        .send()
        .await
        .assert_status(422)
        .assert_problem("validation")
        .assert_field_error("/username", "len");
}
```

For a model with no HTTP around it, call `validate` directly:

```rust
let errors = body
    .validate(&mut ValidationCtx::new())
    .expect_err("the second tag is not lower-case");
let pointers: Vec<String> = errors.iter().map(|e| e.pointer.to_string()).collect();
assert_eq!(pointers, ["/tags/1"]);
```

More in [testing](./testing.md).

## Failure modes worth knowing

- **Nested structs are not validated unless you ask.** `#[schema(nested)]` on the field,
  `#[schema(each(nested))]` on a collection. Without it the inner rules are documented but never run.
  See [schemas](./schemas.md#nesting-is-not-implicit).
- **A unit-only enum has no constraints**, so no `422` is documented for it and an unrecognised
  variant is a 400 from deserialisation.
- **Length is counted in Unicode characters**, matching JSON Schema's `minLength`, so `"café"` is
  four characters and five bytes and passes `len = ..=4`.
- **An unknown `format` never fails.** JSON Schema says an unknown format is an annotation, so Moso
  emits it and moves on. The corollary is that Moso refuses to document a format it cannot enforce,
  which is why `SocketAddr` emits a plain string with no `format` keyword.
- **Known formats are checked strictly.** The list is `email`, `uri`, `uuid`, `hostname`, `ipv4`,
  `ipv6`, `ip-cidr`, `date-time`, `date`, `time`, `duration`, `slug`, `phone-e164`, `json-pointer`,
  `regex`, `password`, `byte`, `binary` and `cursor`, of which `password` and `binary` are pure
  annotations that always pass. `uuid` is the hyphenated 36-character form only; `time` requires an
  offset, so `09:00:00Z` passes and `09:00:00` does not; `date` is `2024-01-05` and never `2024-1-5`;
  `duration` rejects fractional components such as `PT1.5H`. The keyword ends up in your OpenAPI
  document and a generated client will hold you to it.
- **`multiple_of` on floats uses a relative epsilon.** `0.3` is a multiple of `0.1`; `0.35` is not.
- **A NaN fails every range check**, whatever the bounds.
- **128-bit integers carry no schema bounds**, because a JSON number cannot represent them
  losslessly. `u128` gets `minimum: 0` only.

## See also

- [Schemas](./schemas.md) for the derive and the full attribute vocabulary.
- [Extractors](./extractors.md) for which extractors validate and what they cost.
- [Errors](./errors.md) for the rest of the problem document and the other error kinds.
- [Security](./security.md) for why the boundary is where it is.
