# 62 — Macro Reference & Expansions

> **Rule from `00-foundations/01`: no magic that cannot be printed.** Every macro's expansion is
> documented here and verifiable with `cargo expand`. If an implementation diverges from this
> document, one of the two is wrong and the discrepancy must be resolved in the same PR.
>
> **This file has been reconciled against the shipped macros.** Sections for macros that belong to
> crates outside this build are collected at the end and marked ⛔.

## Shared conventions

- Generated code refers only to `::moso::__private::*`, a `#[doc(hidden)]` re-export module in the
  facade. This lets internal crates be refactored without changing macro output, and it is why
  `moso-macros` depends on no runtime Moso crate.
- Generated identifiers are prefixed `__moso_` and marked `#[doc(hidden)]`
  `#[allow(non_camel_case_types, non_snake_case, unreachable_pub, dead_code)]`. Generated items also
  carry a real doc comment, so an application with `#![deny(missing_docs)]` does not fail on macro
  output it never wrote.
- Attribute parsing uses `darling` plus hand-written `syn` meta parsers; an unknown key is a compile
  error with a Levenshtein suggestion.
- Every macro emits at most **one** error for a given user mistake, and emits a well-typed
  placeholder so downstream code does not produce a cascade.
- `#[cfg(..)]` attributes on the annotated item are copied onto everything generated from it.
  `cfg_attr` is deliberately not copied — it can expand to any attribute at all.
- Expansion size budgets are enforced by `xtask expand-size`, which expands `examples/crud` and
  attributes every generated line to the macro that produced it. **All three budgets are currently
  exceeded** — measured 2026-07-30 on rustc 1.97.1:

| Macro | Budget | Measured (`examples/crud`) |
| --- | --- | --- |
| `#[endpoint]` | ≤ 60 lines | **152–179 lines per endpoint** (7 endpoints, 1153 lines) |
| `#[derive(Schema)]` | ≤ 25 lines/field, ≤ 300 lines per type | **33.8–44.0 lines/field**; the largest type is 135 lines, inside the 300-line cap |
| `#[derive(Config)]` | ≤ 20 lines/field | **21.6–61.0 lines/field** |

  The largest single contributor to `#[endpoint]` is the `__moso_op_*` companion type from
  ADR-0013, which did not exist when the 60-line budget was written: it carries the operation's
  full OpenAPI description as `const` data, and that data is the thing the budget is measuring.
  Either the budget is wrong for the design that was chosen, or the description belongs behind a
  function call instead of an inline `const`. Resolving that is WP-25 work and needs a decision,
  not a smaller number quietly written into this table.

---

## `#[endpoint]`

### Input
```rust
/// Create a user.
///
/// Sends a welcome email asynchronously.
#[endpoint]
async fn create(
    Inject(db): Inject<Db>,
    Depends(actor): Depends<CurrentUser>,
    Json(body): Json<CreateUser>,
) -> Result<Created<UserOut>> { /* body */ }
```

### Expansion

The `async fn` is emitted **unchanged**; the metadata goes on a companion unit struct beside it,
because Rust cannot attach an associated type to a `fn` item. See
[ADR-0013](../adr/0013-handler-registration.md).

```rust
async fn create(
    Inject(db): Inject<Db>,
    Depends(actor): Depends<CurrentUser>,
    Json(body): Json<CreateUser>,
) -> Result<Created<UserOut>> { /* body — unchanged */ }

/// The [`Endpoint`] generated for `create` by `#[endpoint]`.
#[doc(hidden)]
#[allow(non_camel_case_types, non_snake_case, unreachable_pub, dead_code)]
#[derive(Clone, Copy, Default)]
pub struct __moso_op_create;

impl ::moso::__private::Endpoint for __moso_op_create {
    const NAME: &'static str = "create";

    fn spec(__moso_b: &mut ::moso::__private::OperationBuilder) {
        __moso_b.summary("Create a user.");
        __moso_b.description("Sends a welcome email asynchronously.");
        __moso_b.operation_id(match ::core::module_path!().rsplit_once("::") {
            ::core::option::Option::Some((_, __moso_module)) =>
                ::std::format!("{}_{}", __moso_module, "create"),
            ::core::option::Option::None => ::std::string::String::from("create"),
        });
        __moso_b.source(::core::file!(), ::core::line!());
        <Inject<Db> as ::moso::__private::Extract>::describe(__moso_b);
        <Depends<CurrentUser> as ::moso::__private::Extract>::describe(__moso_b);
        <Json<CreateUser> as ::moso::__private::ExtractBody>::describe(__moso_b);
        <Result<Created<UserOut>> as ::moso::__private::Describe>::describe(__moso_b);
    }

    fn required_providers() -> &'static [::moso::__private::ProviderReq] {
        ::moso::__private::concat_reqs!(
            <Inject<Db> as ::moso::__private::Extract>::PROVIDER_REQ,
            <Depends<CurrentUser> as ::moso::__private::Extract>::PROVIDER_REQ,
            <Json<CreateUser> as ::moso::__private::ExtractBody>::PROVIDER_REQ,
        )
    }
}

// The extraction glue: one concrete async block, no generics, so it compiles once.
impl ::moso::__private::HandlerFn for __moso_op_create {
    #[allow(unused_mut, unused_variables, deprecated)]
    fn invoke(
        __moso_req: ::moso::__private::Request,
        __moso_ctx: ::moso::__private::RequestCtx,
    ) -> ::moso::__private::BoxFuture<'static, ::moso::__private::Response> {
        ::std::boxed::Box::pin(async move {
            let (mut __moso_parts, __moso_body) = __moso_req.into_parts();
            let __moso_a0 = match <Inject<Db> as ::moso::__private::Extract>::extract(
                &mut __moso_parts, &__moso_ctx).await
            {
                ::core::result::Result::Ok(v) => v,
                ::core::result::Result::Err(e) =>
                    return ::moso::__private::IntoResponse::into_response(e),
            };
            let __moso_a1 = /* … Depends<CurrentUser>, same shape … */;
            let __moso_a2 = match <Json<CreateUser> as ::moso::__private::ExtractBody>::extract_body(
                ::moso::__private::Request::from_parts(__moso_parts, __moso_body), &__moso_ctx).await
            {
                ::core::result::Result::Ok(v) => v,
                ::core::result::Result::Err(e) =>
                    return ::moso::__private::IntoResponse::into_response(e),
            };
            ::moso::__private::IntoResponse::into_response(
                create(__moso_a0, __moso_a1, __moso_a2).await)
        })
    }
}

// Diagnostics — the whole reason this is not opt-in.
#[allow(dead_code, non_snake_case)]
const _: () = {
    fn __moso_assert_extract<T: ::moso::__private::Extract>() {}
    fn __moso_assert_body<T: ::moso::__private::ExtractBody>() {}
    fn __moso_assert_describe<T: ::moso::__private::Describe>() {}
    fn __moso_assert_response<
        T: ::moso::__private::IntoResponse + ::moso::__private::Describe,
    >() {}
    fn __moso_check() {
        __moso_assert_extract::<Inject<Db>>();
        __moso_assert_extract::<Depends<CurrentUser>>();
        __moso_assert_body::<Json<CreateUser>>();
        __moso_assert_response::<Result<Created<UserOut>>>();
    }
};
```

Differences from the sketch this file used to carry, all deliberate:

- **The `operationId` is computed at runtime from `module_path!()`**, not baked in as a literal.
  A proc macro cannot see the module it is expanded in, and hard-coding `"users_create"` would be
  wrong for a handler that gets moved.
- **Bindings are `__moso_a0…`, the builder is `__moso_b`, the request is `__moso_req`.** The
  `__moso_` prefix is uniform, so no generated name can shadow a user's.
- **The response assertion requires `IntoResponse + Describe`**, not `IntoResponse` alone: a handler
  return type that cannot describe itself would silently produce an operation with no responses.
- **`Send` is not separately asserted.** `HandlerFn::invoke` returns a `BoxFuture<'static, …>`
  whose `Send` bound is in the return type, so a non-`Send` future fails there with a span in the
  generated block — one assertion fewer for the same diagnostic.
- **A placeholder companion type is emitted on failure**, so a `routes!` table naming the handler
  yields no second error.

### Compile-time checks performed by the macro itself
| Check | Error |
| --- | --- |
| body extractor not last | "request body extractor must be the last parameter" (`01-http/11`) |
| more than one body extractor | "only one body extractor is allowed per handler" |
| > 16 parameters | "handlers support at most 16 parameters; group them into a `Depends` struct" |
| not `async` | "handlers must be `async fn`" |
| `self` parameter | "handlers must be free functions, not methods" |
| generic parameters or a `where` clause | "handlers may not be generic; use a concrete type or a trait object" |
| `impl Trait` in argument position | rejected, with the concrete-type fix |
| unknown attribute arg | listed with a Levenshtein suggestion |

Which parameter consumes the body is decided by a **name heuristic** over the outermost path segment
of the parameter's type. The heuristic decides which *trait is named*; the trait bound is what
enforces the rule. A misclassification therefore fails with `ExtractBody is implemented but Extract
is not` (or the converse), whose `on_unimplemented` note says exactly that — never trait-resolution
vomit.

### Attribute arguments
`operation_id`, `tag`, `hidden`, `deprecated`, `response(status, description)`,
`example(request = …, response = …)`, `errors = Type`. All optional; the common case is bare.

`response(..)` is emitted *before* the describers, so an explicit attribute wins under the builder's
first-writer-wins merge rule. `example(..)` is applied *after*, over whatever media types the
extractors produced, and never overwrites an example a type supplied for itself.

---

## `routes!`

### Input
```rust
moso::routes! {
    GET    "/users"      => list,
    POST   "/users"      => create,
    GET    "/users/{id}" => users::show,
}
```

### Expansion
```rust
::moso::__private::Router::new()
    .endpoint::<__moso_op_list>(
        ::moso::__private::HttpMethod::Get,
        ::moso::__private::route_path!("/users"),
    )
    .endpoint::<__moso_op_create>(
        ::moso::__private::HttpMethod::Post,
        ::moso::__private::route_path!("/users"),
    )
    .endpoint::<users::__moso_op_show>(
        ::moso::__private::HttpMethod::Get,
        ::moso::__private::route_path!("/users/{id}"),
    )
```

It **is** the builder chain, written as a table — which is how acceptance criterion 5 of
`01-http/11` ("`routes!` and the builder chain produce byte-identical OpenAPI documents") is
satisfied structurally rather than by testing.

Notes:

- Only the **last segment** of a handler path is rewritten: `users::show` →
  `users::__moso_op_show`, `::blog::routes::list` → `::blog::routes::__moso_op_list`.
- `route_path!` wraps the literal so `:id`, `*rest` and an unbalanced brace are rejected at the
  span the user wrote, rather than at boot. `Router::endpoint` re-checks at registration.
- `ANY "/webhook" => receive` expands to seven registrations, in `HttpMethod::ALL` order, so the
  route table and the document come out deterministically.
- A malformed table expands to `{ compile_error!(…) ::moso::__private::Router::new() }`, so a
  trailing `.tag("users")` still type-checks.

---

## `ep!`

### Input / expansion
```rust
moso::ep!(list)         //  →  __moso_op_list
moso::ep!(users::list)  //  →  users::__moso_op_list
```

One token in, one path out. The companion type is a unit struct, so its path is an expression:
`Router::new().get("/users", ep!(list))` reaches the same `Handler<EndpointMarker>` impl `routes!`
uses.

`ep!(GET "/healthz" => healthz)` — the predictable mistake — is detected before parsing and answered
with `help: write Router::new().get("/healthz", ep!(healthz))`.

---

## `#[derive(Schema)]`

### Input
```rust
#[derive(Schema)]
pub struct CreateUser {
    /// Public handle.
    #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
    pub username: String,
    pub email: Email,
    #[schema(range = 13..=130)]
    pub age: Option<u8>,
}
```

### Expansion (abridged)
```rust
// 1. serde — generated by delegating to a shadow type, so every `#[serde(..)]`
//    attribute the user wrote keeps working, and `#[schema(trim)]` can normalise
//    on the way in.
const _: () = { /* the serde shadow, the __MosoNormalise trait, the Deserialize impl */ };
impl ::serde::Serialize for CreateUser { /* … */ }

// 2. runtime validation
#[automatically_derived]
#[allow(unused_variables, unused_mut, unused_qualifications, clippy::all, clippy::pedantic)]
impl ::moso::__private::Validate for CreateUser {
    fn validate(&self, __ctx: &mut ::moso::__private::ValidationCtx)
        -> ::core::result::Result<(), ::moso::__private::ValidationErrors>
    {
        let mut __errors = __ctx.errors();          // inherits the context's error cap
        {
            let __value = &self.username;
            ::moso::__private::check_len_str(
                ::core::convert::AsRef::<str>::as_ref(__value),
                ::core::option::Option::Some(3u64), ::core::option::Option::Some(32u64),
                "/username", &mut __errors);
            static __MOSO_RE: ::std::sync::OnceLock<::moso::__private::regex::Regex>
                = ::std::sync::OnceLock::new();
            ::moso::__private::check_pattern(
                ::core::convert::AsRef::<str>::as_ref(__value),
                __MOSO_RE.get_or_init(|| /* compiled once */),
                "^[a-z0-9_]+$", "/username", &mut __errors);
        }
        // email — the constrained type validated on construction; nothing to do here
        if let ::core::option::Option::Some(__value) = &self.age {
            ::moso::__private::check_range_i64(
                *__value as i64, ::core::option::Option::Some(13), ::core::option::Option::Some(130),
                ::moso::__private::Bounds::INCLUSIVE, "/age", &mut __errors);
        }
        __errors.into_result()
    }
}

// 3. the document
impl ::moso::__private::Schema for CreateUser {
    fn schema_name() -> ::std::borrow::Cow<'static, str> {
        ::std::borrow::Cow::Borrowed("CreateUser")
    }
    const HAS_CONSTRAINTS: bool = true;
    fn json_schema(__generator: &mut ::moso::__private::SchemaGenerator)
        -> ::moso::__private::SchemaNode
    {
        let mut __object = __generator.object(<Self as ::moso::__private::Schema>::schema_name());
        {
            let mut __field = __generator.subschema_for::<String>();
            __field.min_length = ::core::option::Option::Some(3u64);
            __field.max_length = ::core::option::Option::Some(32u64);
            __field.pattern = ::core::option::Option::Some(/* "^[a-z0-9_]+$" */);
            __field.description = ::core::option::Option::Some(
                ::std::borrow::Cow::Borrowed("Public handle."));
            __object = __object.property("username", __field, true);
        }
        { let mut __field = __generator.subschema_for::<Email>();
          __object = __object.property("email", __field, true); }
        { let mut __field = __generator.subschema_for::<Option<u8>>();
          __field.minimum = /* 13 */; __field.maximum = /* 130 */;
          __object = __object.property("age", __field, false); }
        let mut __node = __object.build();
        __node
    }
}

// 4. so a handler can `-> Result<CreateUser>` — see D9 below
impl ::moso::__private::IntoResponse for CreateUser {
    fn into_response(self) -> ::moso::__private::Response {
        ::moso::__private::json_response(::moso::__private::http::StatusCode::OK, &self)
    }
}
impl ::moso::__private::Describe for CreateUser {
    fn describe(__operation: &mut ::moso::__private::OperationBuilder) {
        ::moso::__private::describe_json::<Self>(__operation, 200u16);
    }
}

// 5. Debug, redacting every `#[schema(secret)]` field
impl ::core::fmt::Debug for CreateUser { /* … */ }
```

**Key property:** `check_len_str(…, Some(3), Some(32), …)` and `min_length = Some(3); max_length =
Some(32)` are generated from the *same* parsed attribute. They cannot disagree.

Corrections to the previous sketch:

- The check is **`check_len_str`** (there is also `check_len_seq` for collections), not `check_len`;
  it takes `Option<u64>` bounds so an open range is expressible.
- `check_range_i64` takes `Option` bounds **and a `Bounds`** carrying exclusivity, because
  `#[schema(range = 0.0..1.0)]` and `#[schema(positive)]` are exclusive on one side.
- Field nodes are built by **mutating `SchemaNode` fields** after `subschema_for::<T>()`, not by a
  builder chain. That is what makes a constraint composable with whatever the field's *type* already
  said about itself — `#[schema(len = 1..=10)]` on a `Vec<Slug>` narrows the array without
  discarding `Slug`'s pattern.
- There is no `.description(None)`: a `None` there cannot infer its type parameter. The builders
  expose `description_opt(Option<Cow<'static, str>>)` for the cases that need it.
- **Item 4 is not optional.** `impl<T: Schema> IntoResponse for T` is an orphan-rule violation, so
  "returning a bare `T: Schema` is fine" is only true because the derive emits `IntoResponse` and
  `Describe` per type (decision **D9**). The derive suppresses them when `#[derive(Responder)]` is
  also present or the container carries `#[schema(no_response)]`, since both derives emitting them
  would be a coherence error.

`check_*` helpers take `&mut ValidationErrors` (a concrete type), so validation bodies do not
monomorphise per call site — see rule A4 in `04-devex/42`.

### Attribute vocabulary — what the macro actually accepts

| Position | Keys |
| --- | --- |
| container | `rename`, `rename_all`, `deny_unknown`, `from`, `check`, `title`, `description`, `tag`, `content`, `untagged`, `deprecated`, `example`, `no_serde`, `no_response` |
| field | `len`, `pattern`, `format`, `trim`, `lowercase`, `uppercase`, `non_empty`, `contains`, `starts_with`, `ends_with`, `range`, `multiple_of`, `positive`, `non_negative`, `unique`, `each`, `nested`, `default`, `rename`, `skip`, `read_only`, `write_only`, `secret`, `deprecated`, `example`, `flatten`, `title`, `description`, `enum_values`, `delimiter`, `flatten_bracket` |
| `each(..)` | `len`, `pattern`, `format`, `non_empty`, `contains`, `starts_with`, `ends_with`, `range`, `multiple_of`, `positive`, `non_negative`, `nested`, `enum_values` |
| variant | `rename`, `skip`, `title`, `description`, `deprecated` |
| `#[constrained(..)]` | `inner`, `name`, `len`, `pattern`, `format`, `trim`, `lowercase`, `uppercase`, `non_empty`, `contains`, `starts_with`, `ends_with`, `range`, `multiple_of`, `positive`, `non_negative`, `check`, `title`, `description`, `secret` |

`#[schema(pattern = "…")]` is compiled by the macro at expansion time, so an invalid regex is a
compile error rather than a panic on the first request.

---

## `#[derive(Constrained)]`

```rust
// input
#[derive(Constrained)]
#[constrained(inner = String, pattern = r"^ORD-\d{8}$", format = "order-number")]
pub struct OrderNumber(String);

// expansion (abridged)
impl OrderNumber {
    pub fn new(value: String) -> Result<Self, ConstraintError> { /* the checks, then Ok(Self(v)) */ }
    pub fn into_inner(self) -> String { self.0 }
}
impl TryFrom<String> for OrderNumber { /* … */ }
impl Validate for OrderNumber { /* nothing — construction already validated */ }
impl Schema for OrderNumber {
    fn schema_name() -> Cow<'static, str> { Cow::Borrowed("OrderNumber") }
    fn json_schema(g: &mut SchemaGenerator) -> SchemaNode { /* string + pattern + format */ }
    fn schema_ref() -> SchemaRef { ::moso::__private::inline_schema_ref::<Self>() }
    const HAS_CONSTRAINTS: bool = true;
}
```

The **parse-don't-validate** shape: a constructed `OrderNumber` is valid, so `Validate` has nothing
to do and a `#[derive(Schema)]` struct containing one emits no runtime check for that field.

---

## `#[derive(Responder)]`

```rust
// input
#[derive(Schema, Responder)]
#[responder(status = 201, header(location = "self.url"))]
struct UserCreated {
    #[serde(skip)] url: String,
    id: Uuid,
    email: Email,
}

// expansion (abridged)
impl ::moso::__private::IntoResponse for UserCreated {
    fn into_response(self) -> ::moso::__private::Response {
        let mut __response = ::moso::__private::json_response(StatusCode::CREATED, &self);
        ::moso::__private::set_header(&mut __response, http::header::LOCATION, &self.url);
        __response
    }
}
impl ::moso::__private::Describe for UserCreated {
    fn describe(__operation: &mut ::moso::__private::OperationBuilder) {
        ::moso::__private::describe_json::<Self>(__operation, 201u16);
        /* plus one `header(..)` per declared header */
    }
}
```

Deriving `Responder` suppresses the `IntoResponse`/`Describe` that `#[derive(Schema)]` would
otherwise emit.

---

## `#[derive(Dependency)]`

```rust
// input — the "wrap and check" shape
#[derive(Dependency, Clone)]
#[depends(from = CurrentUser, check = "is_admin", error = "admin required")]
pub struct AdminUser(pub User);

// expansion (abridged)
impl ::moso::__private::Dependency for AdminUser {
    const PROVIDER_REQ: &'static [ProviderReq] =
        <CurrentUser as ::moso::__private::Dependency>::PROVIDER_REQ;
    fn describe(__operation: &mut OperationBuilder) {
        <CurrentUser as Dependency>::describe(__operation);
        __operation.response(403, ResponseSpec::problem("admin required"));
    }
    async fn resolve(__ctx: &RequestCtx) -> Result<Self> {
        let __from = __ctx.depends::<CurrentUser>().await?;      // memoised per request
        if !__from.0.is_admin() { return Err(Error::forbidden("admin required")); }
        Ok(AdminUser(__from.0))
    }
}
```

`PROVIDER_REQ` is computed from the fields, which is what keeps the boot check honest for composed
dependencies. A hand-written impl that resolves with `ctx.provider::<T>()` and leaves `PROVIDER_REQ`
empty loses the boot guarantee — the runtime error says so by name.

---

## `#[derive(Config)]`

```rust
// input
#[derive(Config)]
pub struct AppConfig {
    #[config(default = "0.0.0.0:3000")]
    pub bind: SocketAddr,
    #[config(secret)]
    pub database_url: SecretString,
    #[config(nested)]
    pub mail: MailConfig,
}

// expansion (abridged)
#[automatically_derived]
impl ::moso::__private::Config for AppConfig {
    fn descriptor() -> &'static ConfigDescriptor {
        static __MOSO_DESCRIPTOR: ::std::sync::OnceLock<ConfigDescriptor> =
            ::std::sync::OnceLock::new();
        __MOSO_DESCRIPTOR.get_or_init(|| ConfigDescriptor {
            type_name: "AppConfig",
            // Leaked once per process: a nested section's descriptor comes from a
            // function call, so the field table cannot be a `const`, and
            // `&'static [FieldDescriptor]` has to come from somewhere.
            fields: Box::leak(Vec::into_boxed_slice(vec![ /* one per field */ ])),
        })
    }

    fn load_nested(
        __loader: &ConfigLoader,
        __prefix: &ConfigKey,
        __errors: &mut BootErrors,
    ) -> Option<Self> {
        let __bind = __loader.field::<SocketAddr>(__prefix, &SPEC_BIND, __errors);
        let __database_url =
            __loader.field::<SecretString>(__prefix, &SPEC_DATABASE_URL, __errors);
        let __mail = __loader.section::<MailConfig>(__prefix, "mail", __errors);
        Some(AppConfig { bind: __bind?, database_url: __database_url?, mail: __mail? })
    }
}
```

`ConfigLoader` offers three readers the derive lowers to: `field::<T>` (required),
`optional_field::<T>` (where absence is a value, so the outer `Option` is success and the inner one
is presence) and `section::<C>` (a nested `Config`). Each records a `BootError` and returns `None`
rather than short-circuiting.

The **shape of the trait changed** from the original design (decision **D10**). It was
`fn load_from(sources: &[Box<dyn ConfigSource>]) -> Result<Self>`; that signature short-circuits on
the first bad field (so a user fixes one problem per run) and has no key prefix, so it cannot express
`#[config(nested)]`. `load_nested` accumulates every problem into `BootErrors` and returns `None`;
`load_from(&ConfigLoader)` and `load()` are defaulted conveniences on top of it.

Note the `?` placement: every field is read **before** any of them is unwrapped, which is what makes
one run report every bad field rather than the first.

---

## `#[derive(Error)]`

```rust
// input
#[derive(Debug, moso::Error)]
pub enum ShopError {
    #[error(status = 409, type = "https://shop.example/errors/out-of-stock")]
    #[error(detail = "Only {available} left in stock")]
    OutOfStock { available: u32 },
    #[error(status = 500)]
    Payment(#[from] PaymentError),
}

// expansion (abridged)
impl ::core::fmt::Display for ShopError { /* the `detail` templates */ }
impl ::core::error::Error for ShopError { /* source() from #[source]/#[from] */ }
impl ::core::convert::From<PaymentError> for ShopError { /* from the #[from] field */ }

impl ::core::convert::From<ShopError> for ::moso::Error {
    fn from(__value: ShopError) -> Self {
        match __value {
            ShopError::OutOfStock { available } => ::moso::Error::new(ErrorKind::Conflict)
                .with_type("https://shop.example/errors/out-of-stock")
                .with_detail(::std::format!("Only {available} left in stock")),
            ShopError::Payment(__source) => ::moso::Error::internal(__source),
        }
    }
}

impl ::moso::__private::Describe for ShopError {
    fn describe(__operation: &mut OperationBuilder) {
        __operation.response(409, ResponseSpec::problem(/* … */));
        __operation.response(500, ResponseSpec::problem(/* … */));
    }
}
```

The `Describe` impl is what `#[endpoint(errors = ShopError)]` calls, and it is why an error taxonomy
appears in the OpenAPI document without being written twice. Variants sharing a status share one
documented response.

---

## `#[middleware]`

```rust
// input
#[moso::middleware]
async fn tenant(mut req: Request, next: Next) -> Result<Response> { /* body */ }

// expansion (outline, not a program)
async fn tenant(mut req: Request, next: Next) -> Result<Response> { /* unchanged */ }

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TenantLayer;

impl TenantLayer {
    pub const NAME: &'static str = "tenant";
    pub const PROVIDER_REQ: &'static [ProviderReq] = concat_reqs!();
    pub const fn new() -> Self { TenantLayer }
    pub const fn required_providers() -> &'static [ProviderReq] { Self::PROVIDER_REQ }
}

impl<S> ::tower::Layer<S> for TenantLayer {
    type Service = TenantService<S>;
    fn layer(&self, inner: S) -> TenantService<S> { TenantService { inner } }
}

pub(crate) struct TenantService<S> { inner: S }
impl<S: Clone> Clone for TenantService<S> { /* … */ }
impl<S> ::tower::Service<Request> for TenantService<S> where /* the Route bounds */ {
    fn call(&mut self, req: Request) -> Self::Future {
        let inner = /* the polled-ready instance */;
        Box::pin(async move {
            let next = Next::new(inner);
            Ok(match tenant(req, next).await {
                Ok(response) => response,
                Err(error)   => error.into_response(),
            })
        })
    }
}
```

- The layer's visibility follows the function's, and its name is the function's in `PascalCase` plus
  `Layer` / `Service`.
- `TenantLayer::NAME` is what `moso middleware` prints, and `PROVIDER_REQ` is what makes leading
  `Inject<T>` parameters participate in boot validation.
- **Parameters before `req` are extracted first**, so `Inject<Db>` works. `Depends<T>` is a compile
  error: middleware runs before routing, so request-scoped dependencies do not exist yet.
- The `Service` impl is generic over `S`, but every Moso registration point erases the inner service
  to `moso::Route` first, so exactly one instantiation is compiled however many routes it is applied
  to.

---

## Debugging macro output

```
cargo expand --package blog routes::posts        # see the expansion
```

`moso check --expand` was specified to annotate the expansion with which attribute produced which
line. **`moso check` is not implemented in this build**; `cargo expand` is the whole story today.

---

## ORM macros

`moso-orm-macros` provides `#[derive(Entity)]`, `#[derive(Projection)]`, `#[derive(Embedded)]`,
`#[derive(DbEnum)]`, `#[derive(Factory)]`, `#[migration]` and `sql!`. Every one resolves against
`::moso::__private::*` and nothing else (decision D6), so the expansions below are abridged only in
the `…` marked places.

<details>
<summary><code>#[derive(Entity)]</code></summary>

```rust
#[derive(Entity, Debug, Clone)]
#[entity(table = "users", timestamps, soft_delete = "deleted_at",
         index(columns("email"), unique, where = "deleted_at is null"),
         check(name = "users_email_shape", expr = "email like '%@%'"),
         new_derives(Debug, Default))]
pub struct User {
    #[entity(pk, default = "uuid_generate_v7()")] pub id: Id<User>,
    #[entity(unique, index)]                      pub email: Email,
    #[entity(json)]                               pub preferences: Preferences,
    #[entity(embedded)]                           pub address: Address,
    pub deleted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[entity(has_many = Post, fk = "author_id")]  pub posts: Related<Vec<Post>>,
}

// →
impl ::moso::__private::Entity for User {
    type Pk = Id<User>;
    const TABLE: TableRef = TableRef::from_static("users");
    const COLUMNS: &'static [ColumnDef] = {
        // Literal array when nothing is embedded. With `#[entity(embedded)]`
        // the value object's own `MOSO_COLUMNS` are spliced in at compile time.
        const __MOSO_OFFSET_0: usize = 0usize;
        const __MOSO_OFFSET_1: usize = 0usize + 3usize + <Address>::MOSO_COLUMNS.len();
        const __MOSO_PARTS: &[&[ColumnDef]] = &[
            &[ColumnDef::new("id", <Id<User> as SqlType>::KIND).primary_key().with_default(),
              ColumnDef::new("email", <Email as SqlType>::KIND).unique(),
              ColumnDef::new("preferences", <Json<Preferences> as SqlType>::KIND)],
            <Address>::MOSO_COLUMNS,
            &[ /* deleted_at, created_at, updated_at */ ],
        ];
        const __MOSO_ALL: [ColumnDef; total_columns(__MOSO_PARTS)] = concat_columns(__MOSO_PARTS);
        &__MOSO_ALL
    };
    const NAME: &'static str = "User";

    fn pk(&self) -> Self::Pk { ::core::clone::Clone::clone(&self.id) }

    fn from_row(__row: &Row) -> Result<Self, DecodeError> {
        // Positional. No name is hashed and no column is looked up.
        let id = <Id<User> as SqlType>::decode(__row, __MOSO_OFFSET_0 + 0usize)
            .map_err(|e| e.in_entity("User").in_field("id"))?;
        let preferences = <Json<Preferences> as SqlType>::decode(__row, __MOSO_OFFSET_0 + 2usize)
            .map_err(|e| e.in_entity("User").in_field("preferences"))?
            .into_inner();
        let address = <Address>::moso_from_row(__row, { __MOSO_OFFSET_0 + 3usize })
            .map_err(|e| e.in_entity("User").in_field("address"))?;
        /* … */
        Ok(Self { id, email, preferences, address, deleted_at, created_at, updated_at,
                  posts: Related::NotLoaded })
    }

    fn descriptor() -> &'static EntityDescriptor {
        static __MOSO_DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
        __MOSO_DESCRIPTOR.get_or_init(|| {
            let mut __builder = EntityDescriptor::builder("User", Self::TABLE);
            __builder = __builder.column(/* the rich ColumnDescriptor */);
            for __column in <Address>::moso_descriptors() { __builder = __builder.column(__column); }
            /* indexes, checks, foreign keys, relations, enum types */
            __builder = __builder.soft_delete("deleted_at");
            __builder = __builder.timestamps("created_at", "updated_at");
            __builder.build()
        })
    }
}

impl User {
    /// Login identity.                       ← the field's doc comment
    pub const EMAIL: Column<User, Email>      = Column::new("email");
    /// Everything this user wrote.
    pub const POSTS: HasMany<User, Post>      = HasMany::new("posts", "author_id");

    /// The loaded `posts`, or the error that names how to load it.
    pub fn posts(&self) -> Result<&Vec<Post>, NotLoaded> {
        match &self.posts {
            Related::Loaded(v) => Ok(v),
            Related::NotLoaded => Err(NotLoaded::of("User", "posts", "User::POSTS")),
        }
    }

    pub fn query() -> Select<Self, ()> { Select::new() }
    pub fn find(key: <Self as Entity>::Pk) -> Select<Self, ()> { Select::find(key) }
    pub fn insert(row: NewUser) -> Insert<Self> { Insert::row(row) }
    pub fn insert_many(rows: impl IntoIterator<Item = NewUser>) -> Insert<Self> { Insert::rows(rows) }
    pub fn update(&self) -> Update<Self> { Update::by_key(Entity::pk(self)) }
    pub fn update_all() -> Update<Self> { Update::all() }
    pub fn delete(&self) -> Delete<Self> { Delete::by_key(Entity::pk(self)) }
    pub fn delete_all() -> Delete<Self> { Delete::all() }
}

/// What has to be supplied to create a `User`. …
#[derive(Debug, Default)]                     // ← only what `new_derives(..)` asked for
pub struct NewUser {
    pub email: Email,
    /// `is_admin`. `None` leaves it to the database's default.
    pub is_admin: Option<bool>,
    pub preferences: Preferences,             // ← the user type, not `Json<..>`
    pub address: Address,
}

impl NewEntity for NewUser {
    const COLUMNS: &'static [&'static str] = { /* spliced, as above */ };
    fn into_row(self) -> Vec<Expr> {
        let mut __row = Vec::with_capacity(Self::COLUMNS.len());
        __row.push(Expr::bound(<Email as SqlType>::into_value(self.email)));
        __row.push(match self.is_admin {
            Some(v) => Expr::bound(<bool as SqlType>::into_value(v)),
            None    => Expr::Default,          // ← the DB default, not a bound NULL
        });
        __row.push(Expr::bound(<Json<Preferences> as SqlType>::into_value(Json::new(self.preferences))));
        __row.extend(self.address.moso_into_values());
        __row
    }
}
```

Three details that are easy to miss:

- **A tenant-scoped entity's `query()` returns `Select<Self, NeedsTenant>`**, which has no `fetch_*`
  until `.scoped(..)` or `.across_tenants()` discharges it.
- **A `belongs_to`'s foreign key has to be a declared field.** `#[entity(belongs_to = User, fk =
  "author_id")]` requires `pub author_id: Id<User>` next to it, and refuses to expand without one.
  That is not bureaucracy: the preloader groups the parents by the foreign key it reads out of each
  row through the generated `ForeignKeyFn`, and with no field to read its fallback is the parent's
  *own* primary key — which returns the wrong rows, silently. The declared field also gives
  `Post::AUTHOR_ID: Column<Post, Id<User>>` for filtering without a join, which is what
  `22-relations.md` promised.
- **Every relation constant carries its setter.** `.linking(..)` is the `LinkFn` the preloader calls
  to put the loaded rows into the field, and it branches on `LoadedRows::is_count` so that one
  constant serves both `.with(..)` and `.with_count(..)`. The count lands in a field the entity
  declares as `#[entity(count_of = "comments")] pub comments_count: Option<i64>`, which is not a
  column; `post.comments_count()?` reads it.
- **`belongs_to_any(..)` generates a `BelongsToAny` constant, its variant table, a
  `{CONST}_KEY: PolymorphicKeyFn` reader, and the `{Entity}{Field}Ref` enum** — whose variants hold
  the *loaded entity*, so `comment.target()?` gives back a `&Post` or a `&Tag` with the compiler
  checking which. Its two columns must be declared fields too, for the same reason.

</details>

<details>
<summary><code>#[derive(Projection)]</code></summary>

```rust
#[derive(Projection)]
#[projection(entity = User, join(Post))]
pub struct UserSummary {
    pub id: Id<User>,
    #[projection(expr = "count(posts.id)")]              pub post_count: i64,
    #[projection(column = Post::CREATED_AT, agg = "max")] pub last_post_at: Option<DateTime<Utc>>,
    #[projection(skip)]                                   pub note: String,
}

// →
impl ProjectionScope<User> for UserSummary {}
impl ProjectionScope<Post> for UserSummary {}   // ← one per named entity; this is the check

impl Projection for UserSummary {
    const COLUMNS: usize = 3usize;               // ← `skip` does not count

    fn select_items() -> Vec<SelectItem> {
        vec![
            checked_column_as::<Self, _, _>(User::ID, "id"),
            raw_expr_as("count(posts.id)", "post_count"),
            checked_aggregate::<Self, _, _>(Post::CREATED_AT, AggregateFunc::Max, "last_post_at"),
        ]
    }

    fn from_row(__row: &Row) -> Result<Self, DecodeError> {
        Ok(Self {
            id: <Id<User> as SqlType>::decode(__row, 0usize)
                .map_err(|e| e.in_entity("UserSummary").in_field("id"))?,
            post_count: <i64 as SqlType>::decode(__row, 1usize)/* … */?,
            last_post_at: <Option<DateTime<Utc>> as SqlType>::decode(__row, 2usize)/* … */?,
            note: Default::default(),
        })
    }
}
```

A field with no attribute reads the entity constant of the same name — `email` becomes
`User::EMAIL`. `checked_column_as`'s `P: ProjectionScope<E>` bound is what turns a column of an
unjoined entity into an error at the field.

</details>

<details>
<summary><code>#[derive(Embedded)]</code></summary>

```rust
#[derive(Embedded, Clone, Debug)]
#[embedded(prefix = "address_")]
pub struct Address { pub line1: String, #[embedded(len = 64)] pub city: String }

// →
impl Address {
    pub const MOSO_COLUMNS: &'static [ColumnDef] = &[
        ColumnDef::new("address_line1", <String as SqlType>::KIND),
        ColumnDef::new("address_city",  <String as SqlType>::KIND),
    ];
    pub const MOSO_COLUMN_NAMES: &'static [&'static str] = &["address_line1", "address_city"];
    pub fn moso_into_values(self) -> Vec<Expr> { /* in `MOSO_COLUMNS` order */ }
    pub fn moso_from_row(__row: &Row, __offset: usize) -> Result<Self, DecodeError> {
        Ok(Self {
            line1: <String as SqlType>::decode(__row, __offset + 0usize)/* … */?,
            city:  <String as SqlType>::decode(__row, __offset + 1usize)/* … */?,
        })
    }
    pub fn moso_descriptors() -> Vec<ColumnDescriptor> { /* with VarChar(64) for `city` */ }
}
```

Inherent items rather than a trait, because the **prefix has to be baked into the names at
expansion time** — `const fn` cannot concatenate strings on stable — so the owner splices the
literals and never rewrites them.

</details>

<details>
<summary><code>#[derive(DbEnum)]</code></summary>

```rust
#[derive(DbEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[db_enum(as = "pg_enum", type_name = "order_status")]
pub enum Status { Pending, PaidInFull }

// →
impl DbEnum for Status {
    const VARIANTS: &'static [&'static str] = &["pending", "paid_in_full"];
    const STORAGE: EnumStorage = EnumStorage::PgEnum;
    const TYPE_NAME: &'static str = "order_status";
    fn as_db_str(&self) -> &'static str { /* match */ }
    fn from_db_str(v: &str) -> Option<Self> { /* match, `_ => None` */ }
    fn as_db_int(&self) -> i32 { /* the discriminants, explicit ones honoured */ }
    fn from_db_int(v: i32) -> Option<Self> { /* match */ }
}

impl SqlType for Status {
    const KIND: ValueKind = ValueKind::Text;
    const TYPE_NAME: &'static str = "Status";
    fn data_type() -> DataType { DataType::Enum(TypeRef::from_static("order_status")) }
    fn to_value(&self) -> Value { Value::text(self.as_db_str()) }
    fn decode(__row: &Row, __index: usize) -> Result<Self, DecodeError> {
        let __stored = __row.get_str(__index)?;
        Self::from_db_str(__stored).ok_or_else(|| DecodeError::malformed(
            __index, "Status",
            format!("`{__stored}` is not a variant of `Status`; the variants are \
                     `pending`, `paid_in_full`"),
        ))
    }
}
```

Reading an unknown value is a decode error that **lists the variants** — never a silent fallback.

</details>

<details>
<summary><code>#[derive(Factory)]</code></summary>

```rust
#[derive(Entity, Factory)]
#[factory(email = "format!(\"user{n}@example.com\")")]
pub struct User { /* … */ }

// →
pub struct UserFactory { email: Option<Email>, /* one per `New…` field */
                         count: usize,
                         sequence: Vec<Box<dyn Fn(usize, NewUser) -> NewUser + Send + Sync>> }

impl UserFactory {
    pub fn new() -> Self { /* every field `None`, `count: 1` */ }
    pub fn email(mut self, value: impl Into<Email>) -> Self { /* … */ }
    pub const fn count(mut self, rows: usize) -> Self { /* … */ }
    pub fn sequence(mut self, step: impl Fn(usize, NewUser) -> NewUser + Send + Sync + 'static) -> Self;
    pub fn build(&self) -> NewUser { self.build_at(0) }
    pub fn build_many(&self) -> Vec<NewUser>;
    pub fn build_at(&self, __index: usize) -> NewUser {
        #[allow(unused_variables)] let n: usize = __index;   // ← `n` is in scope for every default
        let mut __row = NewUser { email: match &self.email {
            Some(v) => Clone::clone(v),
            None    => format!("user{n}@example.com"),       // ← the `#[factory(..)]` expression
        } };
        for __step in &self.sequence { __row = __step(__index, __row); }
        __row
    }
    pub async fn create(&self, ex: impl Executor<'_>) -> OrmResult<User>;
    pub async fn create_many(&self, ex: impl Executor<'_>) -> OrmResult<Vec<User>>;  // one statement
}

impl User { pub fn factory() -> UserFactory { UserFactory::new() } }
```

**There is no faker.** `43-testing.md` writes the defaults as `faker::internet::Email`; the string
is an ordinary Rust expression, so that works if the application depends on such a crate, and
`format!("user{n}@example.com")` works with no dependency and is reproducible by construction. A
field with no default falls back to `Default::default()`.

**`#[factory(..)]` is a container attribute and only a container attribute.** Every other derive
here reads a field form, so `#[factory(default = "…")]` above the field is the natural thing to
write — and it is a compile error naming the container line to write instead
(`moso-ui-tests/tests/ui/orm/factory_default_on_field.rs`). It cannot be ignored: `factory` is a
declared helper attribute, so rustc strips it wherever it appears, and a field-level default would
vanish without a word while the field quietly fell back to `Default::default()`.

</details>

<details>
<summary><code>#[migration]</code> and <code>sql!</code></summary>

```rust
// migrations/20260730T090000_backfill_slugs.rs
/// Fills in the slugs the old importer left null.
#[migration]
pub struct BackfillSlugs;

// →
pub struct BackfillSlugs;
impl BackfillSlugs {
    pub const VERSION: &'static str = "20260730T090000";     // ← from the file name
    pub const NAME: &'static str = "backfill_slugs";
    pub const DESCRIPTION: &'static str = "Fills in the slugs the old importer left null.";
    pub const SOURCE: (&'static str, u32) = (file!(), line!());
}
```

It **registers nothing** — ADR-0004 rules out link-time registries, so the list of migrations is a
written-down list — and it leaves `impl Migration` to the author. A file whose name carries no
timestamp and which writes no `version = "…"` is an error, because a migration whose order is a
guess is worse than one that does not compile.

```rust
moso::sql!("select id from users where email = {email} limit {n as i64}")
// →
::moso::__private::RawQuery::new("select id from users where email = $1 limit $2")
    .bind(email)
    .bind({ let __value: i64 = n; __value })
```

**An interpolation is always a bind parameter.** There is no syntax that concatenates a runtime
string into the statement text, so `sql!` cannot produce an injection even when it is handed a
request body. `{{` and `}}` are literal braces.

</details>

---

## ⛔ Macros not in this build

These belong to crates that do not exist yet (`moso-kv`, `moso-authz`, `moso-jobs`, `moso-mail`,
`moso-test`'s attribute form). Their designed expansions are preserved here as intent. **None of
them compiles today.** The `moso-orm-macros` set — `#[derive(Entity)]`, `Projection`, `Embedded`,
`DbEnum`, `Factory`, `#[migration]` and `sql!` — moved to *ORM macros* above, because it does.

<details>
<summary><code>#[job]</code></summary>

```rust
#[job(queue = "mail", retries = 5, backoff = "exponential(30s, max = 1h)", timeout = "2m")]
pub async fn send_welcome_email(args: SendWelcome, Inject(db): Inject<Db>, ctx: JobCtx)
    -> Result<()> { /* body */ }

// →
pub struct SendWelcomeEmail;
impl ::moso::jobs::Job for SendWelcomeEmail {
    type Args = SendWelcome;
    const NAME: &'static str = "send_welcome_email";
    const QUEUE: &'static str = "mail";
    const RETRIES: u32 = 5;
    const TIMEOUT: Duration = Duration::from_secs(120);
    fn backoff(a: u32) -> Duration { ::moso::jobs::exponential(a, 30_000, 3_600_000) }
    async fn run(args: SendWelcome, ctx: JobCtx) -> Result<()> { /* … */ }
}
```
</details>

<details>
<summary><code>permissions!</code> / <code>roles!</code></summary>

```rust
moso::permissions! { posts.read = "View posts", posts.publish = "Publish posts" }

// →
#[repr(u16)] #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Perm { PostsRead = 0, PostsPublish = 1 }
impl Perm {
    pub const ALL: &'static [Perm] = &[Perm::PostsRead, Perm::PostsPublish];
    pub const fn as_str(self) -> &'static str { /* "posts.read" */ }
    pub const fn description(self) -> &'static str { /* … */ }
    pub const fn group(self) -> &'static str { /* "posts" */ }
    pub fn parse(s: &str) -> Option<Perm> { /* perfect-hash match */ }
}
```
</details>

<details>
<summary><code>namespace!</code>, <code>sql!</code>, <code>#[moso::test]</code></summary>

```rust
moso::kv::namespace! { pub Profile: Id<User> => UserProfile, ttl = 15.min(), codec = Json }

moso::sql!("select * from users where email = {email}")
// →  ::moso::db::RawQuery::new("select * from users where email = $1",
//                              ::moso::db::args![email])
// Interpolations are ALWAYS bind parameters. There is no syntax that concatenates
// a runtime string into SQL text.

#[moso::test]
async fn creates_a_post(app: TestApp) -> Result<()> { /* body */ }
```

The shipped harness offers `moso_test::test_app!(my_crate::app())` inside an ordinary
`#[tokio::test]` instead of a `#[moso::test]` attribute — the attribute's value was mostly the
per-test database lifecycle, and there is no database layer to manage.
</details>
