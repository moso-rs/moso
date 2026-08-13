//! The Moso tutorial application: a small blog API, backed by the framework's
//! own ORM over SQLite.
//!
//! ```text
//! cargo run -p example-crud
//! open http://localhost:3000/docs
//! ```
//!
//! Everything the application *is* is visible in [`build`]: its configuration,
//! its providers, its routes, its middleware and its API metadata, in one
//! expression. `App::build()` then validates the whole thing — every
//! `Inject<T>` has a provider, every route pattern is well formed, no two
//! routes collide, no two operations share an id — and returns a boot error
//! listing everything that is wrong at once, rather than panicking on the first
//! request that happens to reach a hole.
//!
//! # What this example is for
//!
//! | Feature | Where to look |
//! | --- | --- |
//! | a real `#[derive(Entity)]` over a table | [`models::Post`] |
//! | entities are not schemas (`#[schema(from = Post)]`) | [`models::PostOut`] |
//! | the ORM: `query`/`insert`/`update`/`delete` | [`store`] |
//! | signed keyset (cursor) pagination | [`store::list`] |
//! | validated input with field-pathed 422s | [`models::CreatePost`] |
//! | an API-key guard from the `moso-auth` battery | [`auth::ApiKeyGuard`] |
//! | a hand-written dependency | [`auth::Actor`] |
//! | a derived dependency, as authorisation | [`auth::Editor`] |
//! | a `#[middleware]` | [`middleware::observe`] |
//! | `Created` / `NoContent` / `Page` | [`routes::posts`] |
//! | a custom error type | [`error::BlogError`] |
//! | typed, layered configuration | [`config::AppConfig`] |

pub mod auth;
pub mod config;
pub mod error;
pub mod middleware;
pub mod models;
pub mod routes;
pub mod store;

use moso::config::SecretString;
use moso::db::{Backend, DatabaseConfig, Db};
use moso::http_config::ServerConfig;
use moso::openapi::SecurityScheme;
use moso::prelude::*;
use moso::response::cursor::CursorCodec;
use moso::{BoxFuture, HealthCheck, HealthStatus, Resolver};

pub use crate::config::AppConfig;
pub use crate::error::BlogError;
pub use crate::middleware::{Metrics, ObserveLayer};

/// The one-time secret of the API key this example seeds at boot.
///
/// A generated key is random and shown exactly once, so it is provided here for
/// the seeding hook, the CLI and the tests to read. It is a [`SecretString`], so
/// it is redacted in `Debug` and in any log line — the same treatment a real
/// credential gets.
#[derive(Debug, Clone)]
pub struct DemoApiKey(pub SecretString);

/// Build the application from the ambient configuration, seeded with one post.
///
/// The one line `main` calls. Every field of [`AppConfig`] has a default, so
/// this works with an empty environment.
///
/// # Errors
/// A configuration problem, a database that will not open, or a boot error from
/// `App::build()`.
pub async fn app() -> Result<App> {
    seeded(AppConfig::load()?).await
}

/// Build the application from an explicit configuration.
///
/// Separate from [`app`] so that a test can pin the configuration without
/// touching the process environment — see [`AppConfig::defaults`]. Every call
/// opens its own fresh SQLite database, so two applications in one process — a
/// test and the thing it tests — never share rows.
///
/// # Errors
/// As [`app`].
pub async fn build(config: AppConfig) -> Result<App> {
    // Read what the server, the document and the cursor codec need before the
    // configuration moves into the builder, where it becomes a provider.
    let public_url = config.public_url.to_string();
    let bind = config.bind;
    let cursor_secret = config.cursor_secret.expose().to_owned();

    // Open an isolated SQLite database and create the one table, before the app
    // answers a request. A production application runs `moso db migrate`
    // instead; the example creates its schema inline so it needs no setup.
    let db = open_database().await?;
    store::create_schema(&db).await?;

    // The `moso-auth` battery authenticates the client; seed one key so a fresh
    // run and every test has a working credential.
    let (authenticator, api_key) = auth::seed_api_key().await?;

    App::new(config)
        .provide(db)
        .provide(authenticator)
        .provide(CursorCodec::new(cursor_secret))
        .provide(DemoApiKey(SecretString::new(api_key)))
        .provide(Metrics::default())
        .mount(routes::router().layer(ObserveLayer::new()))
        .server_config(ServerConfig {
            bind,
            ..ServerConfig::default()
        })
        .health_check("database", DatabaseIsReachable)
        .openapi(move |document| {
            document
                .title("Moso blog API")
                .version(env!("CARGO_PKG_VERSION"))
                .description(
                    "The Moso tutorial application. Posts live in SQLite through Moso's own \
                     ORM; the example creates a fresh database on every start.",
                )
                .server(public_url, "this instance")
                .security_scheme(
                    crate::auth::API_KEY_SCHEME,
                    SecurityScheme::api_key_header(crate::auth::API_KEY_HEADER),
                )
                .tag_description("posts", "Everything you can do with a post.")
                .tag_description("status", "What this instance is doing.");
        })
        .build()
}

/// Build the application and seed it with one post, so a fresh `cargo run` has
/// something to show at `/api/v1/posts`.
///
/// Reaching the database through `app.resolver()` is how anything outside a
/// request gets at a provider — a startup hook, a CLI subcommand, a test.
///
/// # Errors
/// As [`app`].
pub async fn seeded(config: AppConfig) -> Result<App> {
    let app = build(config).await?;
    let db = app.resolver().get::<Db>()?;
    store::create(&db, welcome_post(), "moso").await?;
    Ok(app)
}

/// The one-time secret of the seeded API key, reachable outside a request.
///
/// A `cargo run` prints it so the operator can try a write; a test presents it.
///
/// # Errors
/// A boot error, if the application does not build.
pub fn demo_api_key(app: &App) -> Result<String> {
    Ok(app.resolver().get::<DemoApiKey>()?.0.expose().to_owned())
}

/// Open the SQLite database this instance owns.
///
/// A fresh file per call, dropped first so the run starts empty. A file rather
/// than `:memory:`, because every connection in a pool gets its own in-memory
/// database and the schema would vanish between statements.
async fn open_database() -> Result<Db> {
    let path = std::env::temp_dir().join(format!(
        "moso-crud-{}-{}.sqlite",
        std::process::id(),
        Id::<models::Post>::new()
    ));
    let _ = std::fs::remove_file(&path);
    let config = DatabaseConfig::from_url(format!("sqlite://{}?mode=rwc", path.display()));
    Db::connect(&config).await.map_err(Error::internal)
}

/// The post a freshly started instance contains.
fn welcome_post() -> models::CreatePost {
    models::CreatePost {
        title: "Hello from Moso".to_owned(),
        body: "This post lives in SQLite through Moso's ORM. Restart the process and it is gone."
            .to_owned(),
        publish: true,
    }
}

/// The readiness probe for the database.
///
/// A `select count(*)` over `posts` is the cheapest thing that proves the pool
/// answers and the schema is there. A real deployment swaps the count for a
/// bare `SELECT 1` — `Db::health()` — and `/readyz` starts reporting on the
/// database with no other change.
#[derive(Debug, Clone, Copy)]
struct DatabaseIsReachable;

impl HealthCheck for DatabaseIsReachable {
    fn check<'a>(&'a self, resolver: &'a Resolver) -> BoxFuture<'a, HealthStatus> {
        Box::pin(async move {
            let db = match resolver.get::<Db>() {
                Ok(db) => db,
                Err(error) => return HealthStatus::Down(error.to_string()),
            };
            match models::Post::query().count(&*db).await {
                Ok(count) => HealthStatus::Degraded(format!("{count} posts")),
                Err(error) => HealthStatus::Down(error.to_string()),
            }
        })
    }

    fn critical(&self) -> bool {
        // Reporting the row count is informational; a non-critical check must
        // not take the instance out of rotation. `Backend::Sqlite` is always
        // local, so "unreachable" here would mean the file vanished.
        let _ = Backend::Sqlite;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_application_builds_from_its_defaults() {
        let app = build(AppConfig::defaults().expect("defaults load"))
            .await
            .expect("builds");
        assert_eq!(app.router_info().len(), 7);
    }

    #[tokio::test]
    async fn the_document_names_every_path_and_the_security_scheme() {
        let app = build(AppConfig::defaults().expect("defaults load"))
            .await
            .expect("builds");
        let json = serde_json::to_string(app.openapi()).expect("serialises");

        assert!(json.contains("/api/v1/posts"), "{json}");
        assert!(json.contains("/api/v1/posts/{id}/publish"), "{json}");
        assert!(json.contains("CreatePost"), "{json}");
        assert!(json.contains(crate::auth::API_KEY_HEADER), "{json}");
    }

    #[tokio::test]
    async fn seeding_puts_one_post_in_the_database() {
        let app = seeded(AppConfig::defaults().expect("defaults"))
            .await
            .expect("builds");
        let db = app.resolver().get::<Db>().expect("provided");
        let count = models::Post::query().count(&*db).await.expect("counts");
        assert_eq!(count, 1);
    }
}
