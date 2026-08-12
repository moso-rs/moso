//! @@CRATE_NAME@@ — the application.
//!
//! `main.rs` is four lines; everything real lives here so that the integration
//! tests in `tests/` can boot the same application the binary boots. That is
//! the whole reason this crate has a library target.

pub mod dump;
pub mod routes;@@DB_MOD@@@@AUTH_MOD@@

use moso::prelude::*;

/// The prefix every environment variable for this application carries.
///
/// `AppConfig::greeting` is read from `@@ENV_PREFIX@@__GREETING`. Nesting a
/// section adds another `__`, so a `database.url` field would be
/// `@@ENV_PREFIX@@__DATABASE__URL`. Keep it in one place: `.env.example`, the
/// loader and `moso config` all quote this constant.
pub const ENV_PREFIX: &str = "@@ENV_PREFIX@@";

/// Everything @@CRATE_NAME@@ reads from its environment.
///
/// The doc comment on each field is not decoration: it becomes the comment
/// above the key in `.env.example` and the description in `moso config`, so a
/// field cannot be added without being explained.
///
/// `.env.example` is *generated from this struct*. After adding a field:
///
/// ```sh
/// moso config --env-example --out .env.example
/// ```
///
/// which rewrites the file byte for byte, so the committed example cannot drift.
#[derive(Config, Debug, Clone)]
pub struct AppConfig {
    /// The greeting `GET /` returns.
    #[config(default = "hello")]
    pub greeting: String,

    /// The address to listen on.
    //
    // Configuration rather than a constant because port 3000 is the most
    // contended port on a developer's machine — another service, a container, a
    // second copy of this one — and "change it" should not mean "edit this file
    // and recompile": `@@ENV_PREFIX@@__BIND=127.0.0.1:8080 cargo run`.
    //
    // Written with `//` and not `///` on purpose. The doc comment above becomes
    // the comment above this key in `.env.example`, so it stays to one line;
    // this rationale is for whoever reads the source.
    #[config(default = "0.0.0.0:3000")]
    pub bind: std::net::SocketAddr,@@DB_CONFIG@@@@AUTH_CONFIG@@
}

/// The configuration stack, in precedence order.
///
/// Environment beats `.env` beats `config/<profile>.toml` beats
/// `config/default.toml` beats the `#[config(default = ..)]` in the struct.
/// `moso config` prints which one won for every key.
///
/// # Errors
/// When a committed TOML file exists but does not parse.
pub fn loader() -> Result<moso::config::ConfigLoader> {
    Ok(moso::config::ConfigLoader::standard()?.with_prefix(ENV_PREFIX))
}

/// Assemble the application: configuration, providers, routes.
///
/// One function, called by `main`, by every test, and by the `--dump-*` flags
/// the `moso` CLI uses — so none of them can drift from what you ship.
///
/// # Errors
/// [`moso::Error`] carrying every boot problem at once: a bad configuration
/// value, a route registered twice, a handler asking for a provider nobody
/// registered.
pub fn build() -> Result<App> {
    let config = AppConfig::load_from(&loader()?)?;

    // Read what the server needs before the configuration moves into the
    // builder, where it becomes a provider that handlers reach with `Inject`.
    let bind = config.bind;

    // `pretty` logs in `dev`, one-JSON-object-per-line elsewhere. `serve`
    // installs the subscriber from this at the top of the serving lifetime and
    // holds the `TracingGuard` until the process drains, so the last batch of
    // spans is flushed on the way out. `RUST_LOG` still wins at runtime.
    let tracing = moso::http_config::TracingConfig::for_profile(moso::config::Profile::detect());
@@AUTH_SETUP@@
    App::new(config)
        .server_config(moso::http_config::ServerConfig {
            bind,
            ..Default::default()
        })
        .tracing_config(tracing)@@AUTH_WIRING@@
        .mount(routes::router())
        .build()
}
