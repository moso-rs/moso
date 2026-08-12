//! Typed configuration, loaded from the environment and `config/*.toml`.
//!
//! Every field has a default here, so `cargo run -p example-crud` works with an
//! empty environment. Override any of them:
//!
//! ```text
//! NAME="my blog" PUBLIC_URL=https://blog.example POSTS__PAGE_SIZE=5 cargo run
//! ```
//!
//! `#[derive(Config)]` turns each field into a `FieldDescriptor`, so a bad
//! value is reported at boot with its key, its source and its default — all of
//! them at once, rather than one panic per run.

use std::net::SocketAddr;

use moso::prelude::*;
use moso::schema::Url;

/// Everything this application can be configured with.
///
/// Registered as a provider by `App::new`, so a handler can take
/// `Inject<AppConfig>` and a guard can read it out of the request context.
#[derive(Config, Debug)]
pub struct AppConfig {
    /// The name this instance reports at `/status` and in the API document.
    #[config(default = "moso blog")]
    pub name: String,

    /// The address to listen on. Override with `BIND=127.0.0.1:8080`.
    ///
    /// The application owns this key and hands it to the server with
    /// `.server_config(…)`: the framework's `ServerConfig` is a plain struct,
    /// so where the address comes from is the application's decision rather
    /// than a magic environment variable.
    #[config(default = "0.0.0.0:3000")]
    pub bind: SocketAddr,

    /// The public base URL, used for `Location` headers and the OpenAPI server
    /// entry. Override with `PUBLIC_URL`.
    #[config(default = "http://localhost:3000")]
    pub public_url: Url,

    /// The secret the pagination cursors are signed with.
    ///
    /// A `SecretString`, so it is redacted in `Debug`, in `moso config` output
    /// and in any log line that happens to contain it. A cursor is signed with
    /// it and carries the ordering it was issued for, so a tampered cursor is
    /// refused rather than producing a strange page. The default exists so the
    /// example runs out of the box; a real deployment sets `CURSOR_SECRET`.
    ///
    /// The client's *authentication* is a separate concern, handled by the
    /// `moso-auth` battery — see [`crate::auth`] — which mints and stores its
    /// own keys rather than reading a shared one from configuration.
    #[config(default = "example-crud-cursor-signing-secret-please-override", secret)]
    pub cursor_secret: SecretString,

    /// Listing behaviour. A nested section: `POSTS__PAGE_SIZE=5`.
    #[config(nested)]
    pub posts: PostsConfig,
}

/// How the listing endpoint behaves.
#[derive(Config, Debug)]
pub struct PostsConfig {
    /// The page size used when a request does not ask for one.
    ///
    /// The `range` is checked after coercion, so `POSTS__PAGE_SIZE=0` is a boot
    /// error naming the key — not a runtime division by zero.
    #[config(default = 20, range = 1..=100)]
    pub page_size: u32,
}

impl AppConfig {
    /// The configuration a test wants: the declared defaults, and nothing from
    /// the ambient environment.
    ///
    /// `ConfigLoader::from_sources([])` is the whole trick — a loader with no
    /// sources falls through to the `#[config(default = …)]` layer, so a test
    /// cannot be broken by a `PUBLIC_URL` somebody exported in their shell.
    ///
    /// # Errors
    /// Only if a declared default does not satisfy its own constraint, which is
    /// a bug in this file.
    pub fn defaults() -> Result<Self> {
        Self::load_from(&moso::config::ConfigLoader::from_sources([]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_declared_defaults_load_without_any_environment() {
        let config = AppConfig::defaults().expect("every field has a default");
        assert_eq!(config.name, "moso blog");
        assert_eq!(config.public_url.as_str(), "http://localhost:3000/");
        assert_eq!(config.posts.page_size, 20);
    }

    #[test]
    fn the_cursor_secret_is_redacted_in_debug_output() {
        let config = AppConfig::defaults().expect("defaults load");
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("please-override"),
            "a secret must not reach a log line: {rendered}"
        );
    }

    #[test]
    fn every_field_is_described_for_the_config_command() {
        let keys: Vec<&str> = AppConfig::descriptor()
            .fields
            .iter()
            .map(|field| field.name)
            .collect();
        assert!(keys.contains(&"public_url"), "{keys:?}");
        assert!(keys.contains(&"cursor_secret"), "{keys:?}");
    }
}
