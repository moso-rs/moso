#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "Test harness for Moso applications."]
//!
//! `TestApp` boots the **real** application — the real dependency-injection
//! graph, the real middleware stack, the real OpenAPI document — and
//! [`TestClient`] drives it, with assertions that print a report instead of
//! `left == right`.
//!
//! ```
//! use moso_test::prelude::*;
//! # /// A user, as the API accepts one.
//! # #[derive(moso::Schema)] pub struct CreateUser {
//! #     /// Public handle.
//! #     #[schema(len = 3..=32)] pub username: String,
//! #     /// Contact address.
//! #     pub email: moso::schema::Email }
//! # /// A user, as the API returns one.
//! # #[derive(moso::Schema)] pub struct UserOut {
//! #     /// Stable identifier.
//! #     pub id: u64,
//! #     /// Public handle.
//! #     pub username: String }
//! # /// Everything this application reads from its environment.
//! # #[derive(moso::Config, Clone, Debug)] pub struct AppConfig {
//! #     /// Service name.
//! #     #[config(default = "users")] pub name: String }
//! # /// Create a user.
//! # #[moso::endpoint]
//! # async fn create(moso::extract::Json(body): moso::extract::Json<CreateUser>)
//! #     -> moso::Result<moso::response::Created<UserOut>>
//! # {
//! #     Ok(moso::response::Created::at(
//! #         "/users/1",
//! #         UserOut { id: 1, username: body.username },
//! #     ))
//! # }
//! # /// The composition root every Moso application exposes.
//! # fn app() -> moso::AppBuilder {
//! #     moso::App::new(AppConfig { name: "users".to_owned() })
//! #         .mount(moso::routes! { POST "/users" => create })
//! # }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> moso::Result<()> { creating_a_user_returns_201().await }
//! // #[tokio::test]
//! async fn creating_a_user_returns_201() -> moso::Result<()> {
//!     let app = TestApp::builder().app(app()).spawn().await?;
//!
//!     app.client()
//!         .post("/users")
//!         .json(&serde_json::json!({ "username": "ada", "email": "ada@example.com" }))
//!         .send()
//!         .await
//!         .assert_status(201)
//!         .assert_header_present("location")
//!         .assert_json_path("/username", "ada")
//!         .assert_matches_openapi();
//!
//!     app.logs().assert_no_errors();
//!     Ok(())
//! }
//! ```
//!
//! # The one idea
//!
//! **Test through HTTP, not by calling the handler.** A handler takes
//! extractors; constructing them by hand is awkward and tests the wrong thing,
//! because it skips middleware, validation and serialisation — which is exactly
//! where the bugs are. Everything in this crate is arranged to make the HTTP
//! route the path of least resistance.
//!
//! # What a failure prints
//!
//! Every assertion that fails prints the request, the response, a JSON diff
//! where one applies, **and the server-side log lines for that request id**:
//!
//! ```text
//! ── moso-test: assertion failed ────────────────────────────────────────
//!   expected status 201 Created, got 422 Unprocessable Entity
//!
//!   request:
//!     POST http://localhost/users
//!   request body:
//!     { "username": "a", "email": "ada@example.com" }
//!
//!   response:
//!     422 Unprocessable Entity  (1.1 ms, in-process)
//!   response body:
//!     { "type": "https://moso.rs/errors/validation", "status": 422,
//!       "errors": [ { "pointer": "/username", "code": "len", … } ] }
//!
//!   server logs for request_id moso-test-0000abc1-000000000003:
//!     INFO  moso::http  422 POST /users  1.1ms
//! ──────────────────────────────────────────────────────────────────────
//! ```
//!
//! Attaching the server's own view of the failing request is the difference
//! between a five-second and a fifteen-minute debugging session. It is why
//! [`logs`] installs a `tracing` subscriber and why every response remembers the
//! correlation id it was sent with.
//!
//! # Map of the crate
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`app`] | [`TestApp`], [`TestAppBuilder`] — booting the application |
//! | [`client`] | [`TestClient`], [`RequestBuilder`], [`Multipart`] |
//! | [`response`] | [`TestResponse`] and every assertion |
//! | [`contract`] | validating a body against the documented schema |
//! | [`logs`] | capturing the server's log lines, per request |
//! | [`diff`] | the structural JSON diff the failures print |
//! | [`clock`] | [`TestClock`], the clock `advance_time` moves |
//! | [`report`] | the failure-report formatting |
// The `db` and `factory` modules only exist when the `db` feature is on, so
// the rows that name them are written twice: once as links, for the build that
// has them, and once as plain code spans, for the build that does not. A
// permanent link to a `#[cfg]`-ed module is an unresolved link half the time,
// and `rustdoc::broken_intra_doc_links` is denied workspace-wide.
#![cfg_attr(
    feature = "db",
    doc = "| [`db`]† | per-test databases, the three strategies, [`assert_queries!`] |"
)]
#![cfg_attr(
    feature = "db",
    doc = "| [`factory`]† | seeded fake data, factories, `PasswordHash::test()` |"
)]
#![cfg_attr(
    not(feature = "db"),
    doc = "| `db`† | per-test databases, the three strategies, `assert_queries!` |"
)]
#![cfg_attr(
    not(feature = "db"),
    doc = "| `factory`† | seeded fake data, factories, `PasswordHash::test()` |"
)]
//!
//! † behind the `db` cargo feature.
//!
//! # Writing a test
//!
//! There is no `#[moso::test]` attribute in this build. There does not need to
//! be: `#[tokio::test]` plus one line is the whole ceremony.
//!
//! ```
//! # /// A user, as the API accepts one.
//! # #[derive(moso::Schema)] pub struct CreateUser {
//! #     /// Public handle.
//! #     #[schema(len = 3..=32)] pub username: String,
//! #     /// Contact address.
//! #     pub email: moso::schema::Email }
//! # /// A user, as the API returns one.
//! # #[derive(moso::Schema)] pub struct UserOut {
//! #     /// Stable identifier.
//! #     pub id: u64,
//! #     /// Public handle.
//! #     pub username: String }
//! # /// Everything this application reads from its environment.
//! # #[derive(moso::Config, Clone, Debug)] pub struct AppConfig {
//! #     /// Service name.
//! #     #[config(default = "users")] pub name: String }
//! # /// Create a user.
//! # #[moso::endpoint]
//! # async fn create(moso::extract::Json(body): moso::extract::Json<CreateUser>)
//! #     -> moso::Result<moso::response::Created<UserOut>>
//! # {
//! #     Ok(moso::response::Created::at(
//! #         "/users/1",
//! #         UserOut { id: 1, username: body.username },
//! #     ))
//! # }
//! # /// The composition root every Moso application exposes.
//! # fn app() -> moso::AppBuilder {
//! #     moso::App::new(AppConfig { name: "users".to_owned() })
//! #         .mount(moso::routes! { POST "/users" => create })
//! # }
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> moso::Result<()> { it_works().await }
//! // #[tokio::test]
//! async fn it_works() -> moso::Result<()> {
//!     let app = moso_test::test_app!(app()).await?;
//!     app.client().post("/users")
//!         .json(&serde_json::json!({ "username": "ada", "email": "a@b.example" }))
//!         .send().await
//!         .assert_status(201);
//!     Ok(())
//! }
//! ```
//!
//! [`test_app!`] is a two-token convenience over
//! [`TestApp::builder`]; use the builder directly the moment the test needs to
//! override a provider or bind a real port.
//!
//! # Feature flags
//!
//! | Feature | Default | Effect |
//! | --- | --- | --- |
//! | `server` | yes | [`TestAppBuilder::bind`] and the socket transport (pulls in `reqwest`) |
#![cfg_attr(
    feature = "db",
    doc = "| `db` | no | [`db`] and [`factory`]: per-test databases, factories, [`assert_queries!`] |"
)]
#![cfg_attr(
    not(feature = "db"),
    doc = "| `db` | no | `db` and `factory`: per-test databases, factories, `assert_queries!` |"
)]
//! | `kv` | no | `TestApp::kv` — the application's key-value store |
//! | `mail` | no | `TestApp::mail` and `TestAppBuilder::capture_mail`: a capturing mailer and `assert_sent` |
//! | `jobs` | no | `TestApp::jobs` — the application's job queue |
//! | `storage` | no | `TestApp::storage` — the application's object store |
//!
//! Without `server` the harness still works: it drives the composed
//! `tower::Service` in process, which is the default and the faster path.
//!
//! The four battery accessors each pull only their own battery crate with its
//! service-free default backend; none is on by default, so a suite that tests
//! HTTP alone compiles none of them. The `battery` module documents the shape
//! they share.
//!
//! `db` is off by default because it pulls in `sqlx`, whose bundled SQLite is a
//! multi-minute C compile. A harness user who only tests HTTP must not pay for
//! it. Turn it on where it is used:
//!
//! ```toml
//! [dev-dependencies]
//! moso-test = { version = "0.1", features = ["db"] }
//! ```
//!
//! # Testing against a database
//!
//! Every test gets its own database, created from a template in about fifty
//! milliseconds and dropped when the handle goes away:
//!
//! ```text
//! use moso_test::db::{SqlMigrator, TestDb};
//!
//! let db = TestDb::builder()
//!     .migrator(SqlMigrator::from_dir("migrations")?)
//!     .acquire()
//!     .await?;
//!
//! // Point the application at it, then drive the application as usual.
//! let url = db.url().to_owned();
//! ```
//!
//! It is `text` rather than a doctest because this page is compiled with and
//! without the `db` feature; the `db` and `factory` modules carry the compiled
//! versions.
//!
//! `skip_without_database!` gates a test on `DATABASE_URL`, so a suite still
//! passes on a machine with no server running, and `assert_queries!` is the
//! N+1 guard.
//!
//! # Batteries
//!
//! `43-testing.md`'s `db()`, `kv()`, `mail()`, `jobs()` and `storage()` are the
//! `battery` module, each behind its own off-by-default feature so the default
//! build stays lean: `TestApp::kv`, `TestApp::jobs` and `TestApp::storage` hand
//! back the handle the application resolved at boot, and `TestApp::mail` — after
//! `TestAppBuilder::capture_mail` installs a capturing mailer — answers
//! `assert_sent::<T>(n)` and `assert_none_sent()`.
//!
//! # What is still out of scope in this build
//!
//! There is no WebSocket test client. `moso-core`'s `ws` feature only
//! re-exposes Axum's socket surface — there is no Moso-native WebSocket
//! abstraction to drive or to document a contract for — so a test drives a
//! socket the way an Axum test would, through `TestApp::service`, rather than
//! through a harness type that would imply a contract this build does not model.
//! Server-sent events, which *are* a Moso response type, do have a client:
//! `RequestBuilder::sse`.

pub mod app;
#[cfg(any(
    feature = "db",
    feature = "kv",
    feature = "jobs",
    feature = "storage",
    feature = "mail"
))]
pub mod battery;
pub mod client;
pub mod clock;
pub mod contract;
#[cfg(feature = "db")]
pub mod db;
pub mod diff;
#[cfg(feature = "db")]
pub mod factory;
pub mod logs;
pub mod report;
pub mod response;

#[doc(inline)]
pub use crate::app::{TestApp, TestAppBuilder};
#[cfg(feature = "mail")]
#[doc(inline)]
pub use crate::battery::Mail;
#[doc(inline)]
pub use crate::client::{Multipart, Part, RequestBuilder, SendFailure, SseResponse, TestClient};
#[doc(inline)]
pub use crate::clock::TestClock;
#[doc(inline)]
pub use crate::contract::Options as ContractOptions;
#[cfg(feature = "db")]
#[doc(inline)]
pub use crate::db::{QueryLog, QuerySource, RecordedStatement, Strategy, TestDb, TestDbBuilder};
#[doc(inline)]
pub use crate::diff::{DiffKind, Difference};
#[cfg(feature = "db")]
#[doc(inline)]
pub use crate::factory::{EntityFactory, Factory, Faker, PasswordHash, Seed};
#[doc(inline)]
pub use crate::logs::{Level, LogAssertions, LogRecord};
#[doc(inline)]
pub use crate::report::RequestRecord;
#[doc(inline)]
pub use crate::response::{IntoStatus, TestResponse};

/// The names a test file actually types.
///
/// ```
/// use moso_test::prelude::*;
///
/// // Everything a test file needs, and nothing else.
/// fn takes_a_client(_: &TestClient) {}
/// fn takes_a_response(_: &TestResponse) {}
/// ```
pub mod prelude {
    #[cfg(feature = "mail")]
    pub use crate::Mail;
    pub use crate::{
        ContractOptions, Level, LogAssertions, Multipart, RequestBuilder, TestApp, TestAppBuilder,
        TestClient, TestClock, TestResponse,
    };
    #[cfg(feature = "db")]
    pub use crate::{
        EntityFactory, Factory, Faker, PasswordHash, QueryLog, QuerySource, Seed, Strategy, TestDb,
    };
}

/// Boot a [`TestApp`] from an [`AppBuilder`](moso::AppBuilder).
///
/// ```
/// # /// A user, as the API accepts one.
/// # #[derive(moso::Schema)] pub struct CreateUser {
/// #     /// Public handle.
/// #     #[schema(len = 3..=32)] pub username: String,
/// #     /// Contact address.
/// #     pub email: moso::schema::Email }
/// # /// A user, as the API returns one.
/// # #[derive(moso::Schema)] pub struct UserOut {
/// #     /// Stable identifier.
/// #     pub id: u64,
/// #     /// Public handle.
/// #     pub username: String }
/// # /// Everything this application reads from its environment.
/// # #[derive(moso::Config, Clone, Debug)] pub struct AppConfig {
/// #     /// Service name.
/// #     #[config(default = "users")] pub name: String }
/// # /// Create a user.
/// # #[moso::endpoint]
/// # async fn create(moso::extract::Json(body): moso::extract::Json<CreateUser>)
/// #     -> moso::Result<moso::response::Created<UserOut>>
/// # {
/// #     Ok(moso::response::Created::at(
/// #         "/users/1",
/// #         UserOut { id: 1, username: body.username },
/// #     ))
/// # }
/// # /// The composition root every Moso application exposes.
/// # fn app() -> moso::AppBuilder {
/// #     moso::App::new(AppConfig { name: "users".to_owned() })
/// #         .mount(moso::routes! { POST "/users" => create })
/// # }
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso::Result<()> {
/// let app = moso_test::test_app!(app()).await?;
/// assert!(app.openapi().paths.contains_key("/users"));
/// # Ok(())
/// # }
/// ```
///
/// With no argument it calls `app()` in the caller's scope, which is the
/// composition root `00-foundations/04` asks every Moso application to expose:
///
/// ```
/// # /// A user, as the API accepts one.
/// # #[derive(moso::Schema)] pub struct CreateUser {
/// #     /// Public handle.
/// #     #[schema(len = 3..=32)] pub username: String,
/// #     /// Contact address.
/// #     pub email: moso::schema::Email }
/// # /// A user, as the API returns one.
/// # #[derive(moso::Schema)] pub struct UserOut {
/// #     /// Stable identifier.
/// #     pub id: u64,
/// #     /// Public handle.
/// #     pub username: String }
/// # /// Everything this application reads from its environment.
/// # #[derive(moso::Config, Clone, Debug)] pub struct AppConfig {
/// #     /// Service name.
/// #     #[config(default = "users")] pub name: String }
/// # /// Create a user.
/// # #[moso::endpoint]
/// # async fn create(moso::extract::Json(body): moso::extract::Json<CreateUser>)
/// #     -> moso::Result<moso::response::Created<UserOut>>
/// # {
/// #     Ok(moso::response::Created::at(
/// #         "/users/1",
/// #         UserOut { id: 1, username: body.username },
/// #     ))
/// # }
/// # /// The composition root every Moso application exposes.
/// # fn app() -> moso::AppBuilder {
/// #     moso::App::new(AppConfig { name: "users".to_owned() })
/// #         .mount(moso::routes! { POST "/users" => create })
/// # }
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso::Result<()> {
/// // `app` is in scope, so the macro needs no argument.
/// let app = moso_test::test_app!().await?;
/// # let _ = app;
/// # Ok(())
/// # }
/// ```
///
/// The macro expands to an *expression* of type `impl Future<Output = moso::Result<TestApp>>`,
/// so it is awaited, and `?` applies to the result. It exists only to save the
/// two-line builder chain; reach for [`TestApp::builder`] the moment the test
/// needs to override anything.
#[macro_export]
macro_rules! test_app {
    () => {
        $crate::TestApp::builder().app(app()).spawn()
    };
    ($builder:expr $(,)?) => {
        $crate::TestApp::builder().app($builder).spawn()
    };
}

/// Returns from the test unless a database is configured.
///
/// A test that needs PostgreSQL cannot run on a laptop with no server, and the
/// two usual answers are both bad: failing makes the suite unusable, and
/// silently passing hides a regression. This prints why it skipped and returns.
///
/// ```
/// # async fn users_can_be_created() {
/// // #[tokio::test]
/// moso_test::skip_without_database!();
///
/// let db = moso_test::TestDb::acquire().await.expect("a test database");
/// # let _ = db;
/// # }
/// ```
///
/// The message names `DATABASE_URL` and the command that starts the container.
/// The no-argument form expands to a bare `return`, so a test that returns a
/// `Result` has to say what to return:
///
/// ```
/// # async fn example() -> moso::Result<()> {
/// moso_test::skip_without_database!(Ok(()));
/// # Ok(())
/// # }
/// ```
#[cfg(feature = "db")]
#[macro_export]
macro_rules! skip_without_database {
    () => {
        if !$crate::db::database_is_available() {
            eprintln!(
                "moso-test: skipping {} — {}",
                core::module_path!(),
                $crate::db::skip_reason()
            );
            return;
        }
    };
    ($value:expr $(,)?) => {
        if !$crate::db::database_is_available() {
            eprintln!(
                "moso-test: skipping {} — {}",
                core::module_path!(),
                $crate::db::skip_reason()
            );
            return $value;
        }
    };
}

/// Asserts that a block runs exactly `n` database statements.
///
/// The N+1 guard of `43-testing.md` and `22-relations.md`. On a mismatch it
/// prints **the statements**, numbered, and names the one that repeated —
/// which is the whole diagnosis:
///
/// ```text
/// ── moso-test: assert_queries! ─────────────────────────────────────────
///   expected exactly 2 statements, 12 ran
///   at tests/posts.rs:41:5
///
///   statements (12):
///      1  select "posts"."id", "posts"."title" from "posts" limit $1
///      2  select "users".* from "users" where "users"."id" = $1
///     ...
///
///   10 of them were identical — this is an N+1:
///           select "users".* from "users" where "users"."id" = $1
///   help: preload the relation instead of touching it in a loop:
///          `Post::query().with(Post::AUTHOR)`
/// ──────────────────────────────────────────────────────────────────────
/// ```
///
/// The first argument is anything implementing
/// [`QuerySource`](crate::db::QuerySource): a `&TestDb` for statements the test
/// itself runs, a `&TestApp` for statements the *server* ran, or a
/// `&QueryLog`.
///
/// ```
/// use moso_test::{QueryLog, assert_queries};
///
/// let log = QueryLog::new();
/// assert_queries!(&log, 2, {
///     log.record_sql("select 1");
///     log.record_sql("select 2");
/// });
/// ```
///
/// `begin`, `commit` and `rollback` are not counted — a number that changes
/// because the pool decided to open a transaction is a number nobody can
/// assert on. Add `+ transactions` to count them:
///
/// ```
/// use moso_test::{QueryLog, assert_queries};
///
/// let log = QueryLog::new();
/// assert_queries!(&log, 2, + transactions, {
///     log.record_sql("begin");
///     log.record_sql("select 1");
/// });
/// ```
///
/// `at most` asserts a budget instead of an exact count, for when the precise
/// number is an implementation detail but "one per row" is a bug:
///
/// ```
/// use moso_test::{QueryLog, assert_queries};
///
/// let log = QueryLog::new();
/// assert_queries!(&log, at most 5, {
///     log.record_sql("select 1");
/// });
/// ```
///
/// The block's value is the macro's value, so it can wrap an expression that
/// produces something:
///
/// ```
/// use moso_test::{QueryLog, assert_queries};
///
/// let log = QueryLog::new();
/// let doubled = assert_queries!(&log, 0, { 21 * 2 });
/// assert_eq!(doubled, 42);
/// ```
#[cfg(feature = "db")]
#[macro_export]
macro_rules! assert_queries {
    ($source:expr, at most $budget:expr, $body:block) => {{
        let __moso_source = &$source;
        let __moso_scope = $crate::db::QuerySource::begin_queries(__moso_source);
        let __moso_value = $body;
        __moso_scope.finish().assert_at_most(
            $budget,
            core::file!(),
            core::line!(),
            core::column!(),
        );
        __moso_value
    }};
    ($source:expr, at most $budget:expr, + transactions, $body:block) => {{
        let __moso_source = &$source;
        let __moso_scope =
            $crate::db::QuerySource::begin_queries(__moso_source).including_transaction_control();
        let __moso_value = $body;
        __moso_scope.finish().assert_at_most(
            $budget,
            core::file!(),
            core::line!(),
            core::column!(),
        );
        __moso_value
    }};
    ($source:expr, $expected:expr, + transactions, $body:block) => {{
        let __moso_source = &$source;
        let __moso_scope =
            $crate::db::QuerySource::begin_queries(__moso_source).including_transaction_control();
        let __moso_value = $body;
        __moso_scope.finish().assert_exactly(
            $expected,
            core::file!(),
            core::line!(),
            core::column!(),
        );
        __moso_value
    }};
    ($source:expr, $expected:expr, $body:block) => {{
        let __moso_source = &$source;
        let __moso_scope = $crate::db::QuerySource::begin_queries(__moso_source);
        let __moso_value = $body;
        __moso_scope.finish().assert_exactly(
            $expected,
            core::file!(),
            core::line!(),
            core::column!(),
        );
        __moso_value
    }};
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;

    /// The prelude has to be usable on its own; naming each item is the check.
    #[test]
    fn the_prelude_exports_what_a_test_file_needs() {
        let _: Option<TestApp> = None;
        let _: Option<TestAppBuilder> = None;
        let _: Option<TestClient> = None;
        let _: Option<TestResponse> = None;
        let _: Option<TestClock> = None;
        let _: Option<LogAssertions> = None;
        let _: Option<RequestBuilder> = None;
        let _: Multipart = Multipart::new();
        let _: ContractOptions = ContractOptions::default();
        let _: Level = Level::WARN;
    }
}
