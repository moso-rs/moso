# 43 - Testing

> **Status: implemented, at reduced scope.** The harness, the client, the assertions, the log
> capture, the clock and the OpenAPI contract assertions are all built. Everything that asserts
> against a battery - `db()`, `kv()`, `mail()`, `jobs()`, `storage()`, factories,
> `assert_queries!`, the whole database-isolation section - is **absent, not stubbed**, because
> there is no database, job queue or mailer in this build. Those sections are marked ⛔ in place.

## Principle

Tests exercise the **real application**: the real DI graph, the real middleware stack, the real
database, the real OpenAPI document. A test harness that constructs a parallel, simplified app
tests the harness. This is why `lib.rs::app()` is the composition root
(`00-foundations/04`).

## `TestApp`

```rust
// as built - moso-test
pub struct TestApp { /* opaque */ }

impl TestApp {
    /// Boot from an `AppBuilder` - the application's own `app()` composition root.
    pub async fn spawn(app: AppBuilder) -> Result<Self>;
    pub fn builder() -> TestAppBuilder;

    pub fn client(&self) -> &TestClient;
    pub fn logs(&self) -> &LogAssertions;
    pub fn clock(&self) -> &TestClock;
    pub fn base_url(&self) -> &Url;
    pub fn local_addr(&self) -> Option<SocketAddr>;    // Some only with `.bind()`
    pub fn openapi(&self) -> &Document;
    pub fn state(&self) -> &Arc<AppState>;
    pub fn resolver(&self) -> &Resolver;
    pub fn service(&self) -> Option<axum::Router<()>>;

    /// Move the app's clock forward: affects `TestClock`-driven time.
    pub async fn advance_time(&self, by: Duration);
    pub fn as_anonymous(&self) -> TestClient;
    pub fn as_bearer(&self, token: &str) -> TestClient;
    pub async fn shutdown(self);
}

impl TestAppBuilder {
    pub fn app(self, app: AppBuilder) -> Self;
    pub fn mount(self, router: Router) -> Self;
    pub fn mount_at(self, prefix: &'static str, router: Router) -> Self;
    pub fn override_provider<T: Send + Sync + 'static>(self, value: T) -> Self;
    pub fn override_provider_dyn<T: ?Sized + Send + Sync + 'static>(self, value: Arc<T>) -> Self;
    pub fn customise(self, f: impl FnOnce(AppBuilder) -> AppBuilder) -> Self;
    pub fn http_config(self, config: HttpConfig) -> Self;
    pub fn server_config(self, config: ServerConfig) -> Self;
    pub fn expose_internal_errors(self) -> Self;
    pub fn profile(self, profile: Profile) -> Self;
    pub fn inherit_profile(self) -> Self;
    pub fn bind(self) -> Self;                  // a real socket; needs the `server` feature
    pub fn paused_time(self) -> Self;
    pub fn clock_at(self, at: SystemTime) -> Self;
    pub fn assert_openapi(self, options: ContractOptions) -> Self;   // every response, every test
    pub fn default_header(self, name: &str, value: &str) -> Self;
    pub fn log_limit(self, limit: usize) -> Self;
    pub async fn spawn(self) -> Result<TestApp>;
}
```

Three differences from the sketch, all forced by what exists:

- **`spawn` takes the `AppBuilder`.** `TestApp::spawn()` with no argument would have to *find* the
  application's `app()`, which needs either a convention Moso cannot enforce or a link-time
  registry ADR-0004 forbids. Passing the builder is one token and keeps the composition root real.
  `moso_test::test_app!(my_crate::app())` is the shorthand.
- **`as_user(&User)` is `as_bearer(&str)` and `default_header`.** There is no `User`, no session and
  no auth backend to authenticate against.
- **`db()`, `kv()`, `mail()`, `jobs()`, `storage()`, `seed()`, `real_kv()`, `config()` are absent.**
  `config()` in particular has no meaning when `App::new(cfg)` takes the config by value and the
  test supplies the builder.

## ⛔ Database isolation - not implemented

There is no database layer, so none of the three strategies below exists, and neither does
`moso test`, `--keep-db` or `moso db prune-test`. The section is retained as intent.

Three strategies, configured in `moso.toml` (`database.test_strategy`):

| Strategy | How | Speed | When |
| --- | --- | --- | --- |
| **`template`** (default, Postgres) | migrate once into `app_test_template`, then `CREATE DATABASE x TEMPLATE …` per test | ~50 ms/test | the default; full isolation, real DDL, parallel-safe |
| `transaction` | one connection per test, everything in a transaction rolled back at the end | ~5 ms/test | fastest, but cannot test code that commits or uses multiple connections |
| `migrate` | fresh database, run all migrations | ~2 s/test | correctness checks of the migration chain |

The template strategy is the important one: it gives full isolation at a cost low enough that
nobody is tempted to share state between tests.

## The test client

```rust
// as built
let app = moso_test::test_app!(my_crate::app()).await?;

let res = app.client()
    .post("/api/v1/users")
    .json(&json!({ "email": "a@b.com", "name": "A", "password": "correct horse battery" }))
    .send().await;                          // panics with the full report; `try_send` returns a Result

res.assert_status(201)
   .assert_header_contains("location", "/api/v1/users/")
   .assert_json_path("/email", "a@b.com");

let user: UserOut = res.json();             // typed, fails the test with a clear diff on mismatch
```

`send()` **panics** with the rendered report rather than returning an error, because in a test a
transport failure is a failure, and `?` on it would put the useful output behind a `Debug` impl.
`try_send()` returns `Result<TestResponse, SendFailure>` for the tests that are *about* the
transport.

Assertions are chainable and produce **useful failure output** - a JSON diff, not
`assertion failed: left == right`:

```
── test users::create_returns_201 ──────────────────────────────
  expected status 201, got 422

  POST /api/v1/users
  request body:
    { "email": "a@b.com", "name": "A", "password": "short" }
  response body:
    { "type": ".../validation", "status": 422,
      "errors": [ { "pointer": "/password", "code": "len",
                    "message": "must be at least 12 characters" } ] }

  server logs for this request (request_id 01J8…):
    DEBUG moso::http  422 POST /api/v1/users  1.1ms
```

**As built.** `TestApp::spawn(app)` takes the builder, and `assert_json_path` compares against a
`serde_json::Value`. The full assertion set is: `assert_status`, `assert_ok`, `assert_header`,
`assert_header_present`, `assert_no_header`, `assert_header_contains`, `assert_empty_body`,
`assert_text_contains`, `assert_json_path`, `assert_no_json_path`, `assert_json_matches` (subset),
`assert_json_eq` (exact), `assert_problem`, `assert_field_error`, `assert_matches_openapi`,
`assert_matches_openapi_with`. Accessors: `status`, `headers`, `header`, `body`, `text`, `json`,
`try_json`, `json_value`, `problem`, `request`, `elapsed`, `request_id`, `logs`, `openapi`.

Attaching the server-side logs for the failing request is the difference between a five-second and
a fifteen-minute debugging session, and `TestResponse::logs()` filters the captured buffer by that
response's request id to do it.

Request helpers: `.json()`, `.form()`, `.multipart()`, `.header()`, `.bearer()`, `.cookie()`,
`.query()`, `.body()`, `.timeout()`.
⛔ `.ws("/chat")` and `.sse("/events")` are not implemented.

### Contract assertions

```rust
// example - assert the response actually matches the documented schema
res.assert_matches_openapi();
```

This validates the response body against the OpenAPI schema for that operation, catching drift
between what the docs promise and what the handler returns.

**As built**, "globally" is `TestAppBuilder::assert_openapi(ContractOptions)` rather than a
`moso.toml` key, because there is no `moso.toml`: it turns every response the harness sees into a
contract assertion. `ContractOptions` controls how strict that is (unknown properties, undocumented
statuses). `examples/crud/tests/openapi.rs` uses it.

## ⛔ Factories - not implemented

`#[derive(Factory)]` belongs to `moso-orm-macros`, and there is no ORM. Nothing in this section
exists; it is retained as intent.

```rust
// example - src/models/user.rs
#[derive(Entity, Factory)]
#[factory(
    email = "faker::internet::Email",     // deterministic under the test seed
    name  = "faker::name::Name",
    password = "PasswordHash::test()",    // fast, non-argon2 hash in tests
)]
pub struct User { … }
```

```rust
// example
let admin = User::factory().is_admin(true).create(&db).await?;
let posts = Post::factory().author(&admin).count(20)
    .sequence(|i, p| p.title(format!("Post {i}")))
    .create_many(&db).await?;
let draft = Post::factory().state(PostState::Draft).build();   // unsaved
```

- Relations are auto-created when required and not supplied.
- Faker output is seeded per test (from the test name) so failures are reproducible.
- `PasswordHash::test()` exists because argon2 in fixtures makes a test suite unusable - a real
  problem in Rails and Django suites.
- Factories are shared with `moso db seed` so fixtures are written once.

## Assertion helpers

Of the block below, **only the `logs()` and `advance_time` groups are implemented.**
`assert_queries!` needs an ORM, `jobs()` needs a queue, `mail()` needs a mailer. The shipped log
API is `assert_contains(Level, &str)`, `assert_no_errors()`, plus `records()`/`for_request(id)`.

```rust
// ⛔ queries - the N+1 guard
assert_queries!(app.db(), 2, {
    let posts = Post::query().with(Post::AUTHOR).fetch_all(app.db()).await?;
});

// ⛔ jobs
app.jobs().assert_enqueued::<SendWelcomeEmail>(1);
app.jobs().assert_enqueued_with::<SendWelcomeEmail>(|a| a.user_id == user.id);
app.jobs().drain().await?;                     // execute inline with real DI
app.jobs().assert_none_enqueued();

// ⛔ mail
app.mail().assert_sent_to("a@b.com");
app.mail().last::<WelcomeEmail>().assert_html_contains("Verify");

// ✅ logs
app.logs().assert_contains(Level::WARN, "rate limit");
app.logs().assert_no_errors();                 // a good default at the end of every test

// ✅ time
app.advance_time(Duration::from_secs(3600)).await;
```

## Testing at each level

| Level | Tool | Guidance |
| --- | --- | --- |
| Pure logic | plain `#[test]` | ✅ no framework needed |
| Query correctness | `#[moso::db_test]` | ⛔ no database layer |
| Handler behaviour | `TestApp` + client | ✅ the default; test *through* HTTP, not by calling the fn |
| Compile errors | `trybuild` | ✅ the corpus is the `moso-ui-tests` crate, not a `tests/ui/` directory |
| Migrations | `moso db test-migrations` | ⛔ |
| Contract | `assert_matches_openapi` | ✅ opt-in per response, or per app with `TestAppBuilder::assert_openapi` |
| Load | `xtask bench` | ⛔ no `xtask`, no `examples/bench` |
| Security | `moso check` + `cargo audit` + `cargo deny` | ⛔ none of the three is wired up |

**We recommend testing through HTTP, not by calling handler functions directly.** Handlers take
extractors; constructing them by hand is awkward and tests the wrong thing (it skips middleware,
validation, and serialisation - exactly where the bugs are). The docs make this argument explicitly
because the instinct to unit-test handlers is strong and unproductive.

## ⛔ Property and fuzz testing - not implemented

Neither `proptest` nor `cargo-fuzz` is in the dependency tree, and there is no nightly CI job.
Both remain worth doing; the cursor decoder and the query-string parser are the obvious first
targets.

- `proptest` strategies are generated for any `#[derive(Schema)]` type
  (`CreateUser::arbitrary()`), respecting the declared constraints. This makes property tests of
  validation and round-trip serialisation nearly free.
- A fuzz target for the query-string parser, the cursor decoder, and the multipart parser ships in
  the repo and runs in CI nightly.

## ⛔ What runs in CI (generated workflow) - no CI exists

There is no `.github/` directory in the repository and `moso new` generates no workflow. The
pipeline below is intent. Today the equivalent is `cargo fmt --check && cargo clippy --all-targets
-- -D warnings && cargo test --workspace`, run by hand.

```yaml
- moso check                                   # lints, drift
- cargo fmt --check
- cargo clippy -- -D warnings
- moso test --all-features                     # nextest, template DBs
- moso openapi check                           # API drift
- moso db check                                # schema drift
- cargo deny check                             # licences, advisories, bans
- cargo audit
- xtask bench-compile --gate                   # compile-time budgets
```

Total target for the reference app: **under 6 minutes** on a standard runner. A CI pipeline slower
than that stops being run on every push, and then it stops being trusted.

## Acceptance criteria (WP-21)

*(The document said WP-26; the work package that builds `moso-test` is WP-21.)*

1. ⛔ `TestApp::spawn()` under 200 ms with the template strategy - unmeasured, and there is no
   template strategy. The in-process transport spawns in single-digit milliseconds by observation.
2. ⛔ 100 parallel tests with full DB isolation - no database.
3. ✅ Failure output includes the request, the response, and the server logs for that request.
4. ⛔ `assert_queries!` - no ORM.
5. ⛔ `app.jobs().drain()` - no job queue.
6. ⛔ Factories.
7. ✅ `assert_matches_openapi` catches a handler returning an undocumented field, and
   `TestAppBuilder::assert_openapi` applies it to every response in a test.
8. ⛔ `moso test --coverage`.
