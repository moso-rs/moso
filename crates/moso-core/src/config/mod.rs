//! Typed, layered, discoverable configuration.
//!
//! You probably want `#[derive(moso::Config)]` on one struct per section and
//! [`prelude`] in scope. The layering rules — defaults, then files, then the
//! environment — are in `docs/00-foundations/04-project-structure.md`.
//!
//! ```
//! use moso::config::prelude::*;
//!
//! /// How the mailer is wired.
//! #[derive(moso::Config, Clone, Debug)]
//! pub struct MailConfig {
//!     /// SMTP endpoint.
//!     #[config(default = "localhost:25")]
//!     pub smtp: String,
//! }
//!
//! /// Everything this application reads from its environment.
//! #[derive(moso::Config, Clone, Debug)]
//! pub struct AppConfig {
//!     /// Human-readable service name, used in logs and the OpenAPI title.
//!     #[config(default = "shop")]
//!     pub name: String,
//!
//!     /// Where the server listens.
//!     #[config(default = "0.0.0.0:3000")]
//!     pub bind: SocketAddr,
//!
//!     /// Base URL used to build absolute links in emails and Location headers.
//!     #[config(default = "https://shop.example")]
//!     pub public_url: String,
//!
//!     /// Signing key; never logged.
//!     #[config(secret)]
//!     pub secret_key: SecretString,
//!
//!     /// Mailer settings, under the `mail` prefix.
//!     #[config(nested)]
//!     pub mail: MailConfig,
//! }
//! # fn main() {
//! use moso::prelude::Config as _;
//! let descriptor = AppConfig::descriptor();
//! // Nested sections contribute their keys under a prefix.
//! assert!(descriptor.fields.iter().any(|f| f.name == "mail"));
//! // Secrets are marked, so `moso config` can redact them.
//! assert!(descriptor.fields.iter().any(|f| f.name == "secret_key" && f.secret));
//! # }
//! ```
//!
//! The framework's own sections — [`HttpConfig`](crate::http_config::HttpConfig),
//! [`ServerConfig`](crate::http_config::ServerConfig) — are supplied through
//! [`AppBuilder::http_config`](crate::AppBuilder::http_config) and
//! [`AppBuilder::server_config`](crate::AppBuilder::server_config) rather than
//! by nesting, because `moso-core` cannot depend on `moso-macros` to derive
//! them.
//!
//! # Principles
//!
//! - **Typed, not `HashMap<String, String>`.** A missing or malformed value is
//!   a boot error naming the key, the source and the expected type.
//! - **Layered, with a documented precedence** — see [`source`].
//! - **Profiles change defaults, never semantics.** `dev` does not mean
//!   "skip the checks".
//! - **Secrets are a distinct type** — see [`secret`].
//! - **Every key is discoverable.** [`ConfigDescriptor`] is what `moso config`
//!   prints and what `.env.example` is generated from, so neither can rot.
//!
//! # The eight levels, highest first
//!
//! | # | Level | Where it lives |
//! | --- | --- | --- |
//! | 1 | overrides set in code | [`OverrideSource`] |
//! | 2 | command-line flags | [`CliSource`] |
//! | 3 | environment variables | [`EnvSource`] |
//! | 4 | `.env`, in `dev` and `test` only | [`DotEnvSource`] |
//! | 5 | `config/{profile}.toml` | [`TomlSource`] |
//! | 6 | `config/default.toml` | [`TomlSource`] |
//! | 7 | `#[config(profile(..))]` | [`FieldSpec::profile_default`] |
//! | 8 | `#[config(default = ..)]` | [`FieldSpec::default_value`] |
//!
//! Levels 1 to 6 are sources in [`ConfigLoader`]'s stack; 7 and 8 are literals
//! the derive knows and hands over in a [`FieldSpec`]. [`DefaultsSource`] can
//! stand in for 7 and 8 when there is no generated code — `moso config` and the
//! precedence test both use it.
//!
//! # All problems at once
//!
//! Loading collects every problem before failing, exactly as `App::build` does:
//!
//! ```text
//! error: configuration is invalid (2 problems)
//!
//!   ✗ missing required value: `public_url`
//!       env       SHOP__PUBLIC_URL
//!       or file   config/production.toml  →  public_url = "https://…"
//!       type      Url
//!
//!   ✗ invalid value for `database.max_connections`
//!       source    env SHOP__DATABASE__MAX_CONNECTIONS = "many"
//!       expected  integer in 1..=1000
//!
//!   4 sources were consulted, in order:
//!       cli flags, env, .env (not found), config/production.toml, config/default.toml
//! ```
//!
//! This is why [`Config::load_nested`] returns `Option<Self>` and writes into a
//! [`BootErrors`] rather than returning a `Result`: a `?` after the first
//! missing field would report one problem per compile-run-fix cycle.

pub mod secret;
pub mod source;
pub mod value;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::{BootError, BootErrors, Error, Result};

pub use crate::config::secret::{
    FileSecretProvider, REDACTED, SecretBytes, SecretProvider, SecretRef, SecretString, SecretValue,
};
pub use crate::config::source::{
    CliSource, ConfigSource, DOTENV_FILE, DefaultsSource, DotEnvSource, EnvSource, MapSource,
    OverrideSource, TomlSource, WELL_KNOWN_ALIASES,
};
pub use crate::config::value::{
    Coerce, CoerceError, ConfigKey, ConfigValue, DISPLAY_WIDTH, FALSY, Origin, RawValue, TRUTHY,
};

/// The environment variable naming the profile.
pub const PROFILE_ENV: &str = "MOSO_PROFILE";

/// The environment variable naming the application's key prefix.
///
/// `SHOP` makes `SHOP__DATABASE__URL` the canonical spelling of `database.url`.
/// Unset means no prefix, which is right for a single-service repository and
/// wrong for a machine that runs several.
pub const PREFIX_ENV: &str = "MOSO_CONFIG_PREFIX";

/// The environment variable naming the directory the committed TOML lives in.
pub const CONFIG_DIR_ENV: &str = "MOSO_CONFIG_DIR";

/// The default directory the committed TOML lives in.
pub const CONFIG_DIR: &str = "config";

/// The shared, profile-independent file.
pub const DEFAULT_CONFIG_FILE: &str = "default.toml";

// ---------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------

/// Which set of defaults is in force.
///
/// A profile changes *defaults*, never *semantics*. `dev` does not disable
/// validation, skip authorization, or relax a limit that protects the process;
/// it renders richer errors, loads `.env`, and picks friendlier defaults. That
/// boundary is what keeps "it worked in dev" from being a category of bug.
///
/// ```
/// use moso::config::Profile;
///
/// assert_eq!(Profile::default(), Profile::Dev);
/// assert_eq!(Profile::parse("prod"), Some(Profile::Production));
/// assert_eq!(Profile::Production.as_str(), "production");
///
/// // `.env` is a development convenience, and deliberately not a deployed one.
/// assert!(Profile::Dev.loads_dotenv());
/// assert!(Profile::Test.loads_dotenv());
/// assert!(!Profile::Production.loads_dotenv());
/// ```
///
/// Detected from `MOSO_PROFILE`, then `RUST_ENV`, then the build kind. Set it
/// explicitly with `App::new(cfg).profile(Profile::Production)` when the process
/// knows better than the environment does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Profile {
    /// Local development. Loads `.env`, renders the developer error page,
    /// exposes the docs UI, omits HSTS.
    #[default]
    Dev,
    /// Automated tests. Loads `.env`, but is otherwise production-shaped so a
    /// test exercises what will actually run.
    Test,
    /// Deployed. No `.env`, no error details, no docs UI unless asked for.
    Production,
}

impl Profile {
    /// Detect the profile.
    ///
    /// `MOSO_PROFILE` wins. Otherwise, in order:
    ///
    /// 1. a binary under `target/**/deps/` — a `cargo test` harness — is `test`;
    /// 2. a debug build, or a process cargo started (`CARGO` is in the
    ///    environment), is `dev`;
    /// 3. a release binary with a `.env` beside it is `dev`, because someone put
    ///    that file there on purpose;
    /// 4. anything else is `production`.
    ///
    /// The result is logged prominently at boot by [`Profile::log_resolved`],
    /// because "which configuration am I running" is a recurring production
    /// confusion.
    pub fn detect() -> Self {
        if let Some(raw) = std::env::var_os(PROFILE_ENV) {
            let raw = raw.to_string_lossy().trim().to_lowercase();
            if let Some(profile) = Profile::parse(&raw) {
                return profile;
            }
            tracing::warn!(
                target: "moso::config",
                value = %raw,
                "{PROFILE_ENV} is not one of `dev`, `test` or `production`; detecting instead"
            );
        }

        if cfg!(test) || under_cargo_test() {
            return Profile::Test;
        }
        if cfg!(debug_assertions) || std::env::var_os("CARGO").is_some() {
            return Profile::Dev;
        }
        if dotenv_present() {
            return Profile::Dev;
        }
        Profile::Production
    }

    /// Parse a profile name. Accepts `prod` for `production`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "dev" | "development" => Some(Profile::Dev),
            "test" => Some(Profile::Test),
            "prod" | "production" => Some(Profile::Production),
            _ => None,
        }
    }

    /// The canonical name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Profile::Dev => "dev",
            Profile::Test => "test",
            Profile::Production => "production",
        }
    }

    /// Whether `.env` is loaded.
    pub const fn loads_dotenv(self) -> bool {
        matches!(self, Profile::Dev | Profile::Test)
    }

    /// Whether error responses may carry internal detail *by default*.
    ///
    /// `http.expose_internal_errors` overrides this in either direction.
    pub const fn exposes_errors(self) -> bool {
        matches!(self, Profile::Dev)
    }

    /// The file this profile reads: `config/dev.toml`.
    pub const fn config_file(self) -> &'static str {
        match self {
            Profile::Dev => "config/dev.toml",
            Profile::Test => "config/test.toml",
            Profile::Production => "config/production.toml",
        }
    }

    /// The file name alone, for a deployment that moved the directory with
    /// `MOSO_CONFIG_DIR`.
    pub const fn config_file_name(self) -> &'static str {
        match self {
            Profile::Dev => "dev.toml",
            Profile::Test => "test.toml",
            Profile::Production => "production.toml",
        }
    }

    /// Log the resolved profile at `INFO`, at most once per process.
    ///
    /// Deliberately loud and deliberately structured: the fields are what an
    /// operator greps for when a deployment behaves like the wrong environment,
    /// and the message is what they see when they are only skimming.
    pub fn log_resolved(self) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::info!(
                target: "moso::config",
                profile = self.as_str(),
                dotenv = self.loads_dotenv(),
                exposes_errors = self.exposes_errors(),
                config_file = self.config_file(),
                "=== moso profile: {} ===",
                self.as_str()
            );
        });
    }
}

impl core::fmt::Display for Profile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Coerce for Profile {
    const TYPE_NAME: &'static str = "profile (`dev`, `test` or `production`)";

    fn coerce(value: &RawValue) -> core::result::Result<Self, CoerceError> {
        let text = value
            .as_text()
            .ok_or_else(|| CoerceError::mismatch::<Self>(value))?;
        Profile::parse(text.trim().to_lowercase().as_str())
            .ok_or_else(|| CoerceError::mismatch::<Self>(value))
    }
}

/// Whether this process looks like a `cargo test` harness.
///
/// Cargo does not set a variable that distinguishes `cargo test` from
/// `cargo run` — both get `CARGO` and `CARGO_MANIFEST_DIR` — but it does put
/// test binaries under `target/<profile>/deps/` and nothing else there that a
/// person runs on purpose.
fn under_cargo_test() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    exe.parent()
        .and_then(|parent| parent.file_name())
        .is_some_and(|name| name == "deps")
}

/// Whether a `.env` exists in the working directory or an ancestor.
fn dotenv_present() -> bool {
    let Ok(start) = std::env::current_dir() else {
        return false;
    };
    start
        .ancestors()
        .any(|directory| directory.join(DOTENV_FILE).is_file())
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// A type that can be loaded from layered configuration sources.
///
/// Implemented by `#[derive(Config)]`. A hand-written impl is possible and
/// documented, but the derive is what generates the [`ConfigDescriptor`] that
/// `moso config` and `.env.example` depend on.
///
/// `App::new(config)` takes the application's root `Config` and registers it as
/// a provider, so any handler can reach it with `Inject<AppConfig>`.
///
/// ```
/// use moso::config::prelude::*;
/// use moso::config::{ConfigLoader, MapSource};
///
/// /// Everything this application reads from its environment.
/// #[derive(moso::Config, Debug)]
/// pub struct AppConfig {
///     /// Where the server listens.
///     #[config(default = "0.0.0.0:3000")]
///     pub bind: SocketAddr,
///     /// Connection string; never logged.
///     #[config(secret)]
///     pub database_url: SecretString,
/// }
///
/// # fn main() {
/// // The descriptor exists without an instance — this is what `moso config`
/// // and `.env.example` are generated from.
/// let descriptor = AppConfig::descriptor();
/// assert!(descriptor.fields.iter().any(|f| f.name == "bind" && f.default.is_some()));
///
/// // And a value can be loaded from any set of sources.
/// let loader = ConfigLoader::from_sources([
///     Box::new(MapSource::from([("database_url", "sqlite::memory:")])) as _,
/// ]);
/// let config = AppConfig::load_from(&loader).expect("a complete configuration");
/// assert_eq!(config.bind.port(), 3000);
/// # }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a configuration type",
    label = "not configuration",
    note = "help: add `#[derive(moso::Config)]` to `{Self}`",
    note = "every field needs a type that implements `Coerce`, a `#[config(default = ..)]`, or \
            `#[config(nested)]` on another `Config` type",
    note = "`App::new(config)` takes the application's root `Config`; nested sections are \
            reached through it rather than registered separately"
)]
pub trait Config: Sized + Send + Sync + 'static {
    /// Field metadata: names, types, defaults, doc comments, secrecy.
    ///
    /// A `static`, so it costs nothing at runtime and is available without an
    /// instance — which is what lets `moso config --env-example` run without
    /// booting the application.
    fn descriptor() -> &'static ConfigDescriptor;

    /// Read this type from `loader`, rooted at `prefix`.
    ///
    /// Returns `None` after recording problems in `errors`, rather than
    /// short-circuiting, so one run reports every bad field. This is the method
    /// the derive implements; the other two are conveniences built on it.
    fn load_nested(
        loader: &ConfigLoader,
        prefix: &ConfigKey,
        errors: &mut BootErrors,
    ) -> Option<Self>;

    /// Load from `loader`, failing with the full report.
    ///
    /// # Errors
    /// [`Error::boot`] carrying every problem found, plus the list of sources
    /// that were consulted — "where would this value have come from" is the
    /// first question a reader of a configuration error has.
    fn load_from(loader: &ConfigLoader) -> Result<Self> {
        let mut errors = BootErrors::new();
        let loaded = Self::load_nested(loader, &ConfigKey::root(), &mut errors);

        if !errors.is_empty() {
            errors.push(loader.consulted_sources_note());
            return Err(Error::boot(errors));
        }

        loaded.ok_or_else(|| {
            // A `load_nested` that answers `None` without recording anything is
            // a bug in a hand-written impl. Say so rather than unwrapping.
            Error::internal_msg(format!(
                "`{}` failed to load without reporting a problem; `Config::load_nested` must \
                 record a `BootError` before returning `None`",
                Self::descriptor().type_name
            ))
        })
    }

    /// Load using the standard source stack for the detected profile.
    ///
    /// # Errors
    /// [`Error::boot`], as [`Config::load_from`].
    fn load() -> Result<Self> {
        Self::load_from(&ConfigLoader::standard()?)
    }
}

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

/// Everything `moso config` and `.env.example` need to know about a type.
#[derive(Debug, Clone)]
pub struct ConfigDescriptor {
    /// The type's name, for error messages.
    pub type_name: &'static str,
    /// Its fields, in declaration order.
    pub fields: &'static [FieldDescriptor],
}

impl ConfigDescriptor {
    /// Find a field by name.
    pub fn field(&self, name: &str) -> Option<&FieldDescriptor> {
        self.fields.iter().find(|field| field.name == name)
    }

    /// Every key this type defines, flattened through nested sections.
    ///
    /// Declaration order, depth first, so `.env.example` and `moso config` both
    /// read in the order the struct was written — which is the order the author
    /// thought about them in.
    pub fn keys(&self, prefix: &ConfigKey) -> Vec<ConfigKey> {
        let mut keys = Vec::new();
        self.collect_keys(prefix, &mut keys, 0);
        keys
    }

    /// Depth-first walk with a recursion guard.
    ///
    /// A `&'static ConfigDescriptor` graph can be made cyclic by a hand-written
    /// impl — the derive cannot produce one, because a struct cannot contain
    /// itself by value — and a stack overflow during `moso config` would be a
    /// poor way to find out.
    fn collect_keys(&self, prefix: &ConfigKey, out: &mut Vec<ConfigKey>, depth: usize) {
        if depth > MAX_NESTING {
            return;
        }
        for field in self.fields {
            let key = prefix.child(field.name);
            match field.nested {
                Some(nested) => nested.collect_keys(&key, out, depth + 1),
                None => out.push(key),
            }
        }
    }

    /// The leaf fields, paired with their full keys.
    pub fn leaves(&self, prefix: &ConfigKey) -> Vec<(ConfigKey, &'static FieldDescriptor)> {
        let mut leaves = Vec::new();
        self.collect_leaves(prefix, &mut leaves, 0);
        leaves
    }

    /// The walk behind [`ConfigDescriptor::leaves`].
    fn collect_leaves(
        &self,
        prefix: &ConfigKey,
        out: &mut Vec<(ConfigKey, &'static FieldDescriptor)>,
        depth: usize,
    ) {
        if depth > MAX_NESTING {
            return;
        }
        for field in self.fields {
            let key = prefix.child(field.name);
            match field.nested {
                Some(nested) => nested.collect_leaves(&key, out, depth + 1),
                None => out.push((key, field)),
            }
        }
    }

    /// Read every key this descriptor declares, for `moso config`.
    ///
    /// Resolution only — nothing is coerced and nothing fails, because the
    /// point of `moso config` is to show what *is* set, including the values
    /// that are set to something unusable.
    pub fn resolve(&self, loader: &ConfigLoader) -> ResolvedConfig {
        let entries = self
            .leaves(&ConfigKey::root())
            .into_iter()
            .map(|(key, field)| {
                let spec = FieldSpec::from_descriptor(field);
                let found = loader.value_for(&key, &spec);
                ResolvedEntry {
                    value: match &found {
                        Some(_) if field.secret => REDACTED.to_owned(),
                        Some(found) => found.raw.display(DISPLAY_WIDTH),
                        None => "(not set)".to_owned(),
                    },
                    origin: found.map(|found| found.origin),
                    key,
                    secret: field.secret,
                }
            })
            .collect();

        ResolvedConfig {
            profile: loader.profile(),
            entries,
        }
    }

    /// Regenerate `.env.example` from the descriptor.
    ///
    /// Doc comments become the comment above each key and defaults become the
    /// value, so the file cannot drift from the struct: CI regenerates it and
    /// compares, exactly as it does with `openapi.json`.
    ///
    /// A secret never gets a value, whatever its default, because a default
    /// secret in a committed file is a secret in a committed file.
    pub fn render_env_example(&self, prefix: &str) -> String {
        let mut out = String::new();
        for (key, field) in self.leaves(&ConfigKey::root()) {
            if !out.is_empty() {
                out.push('\n');
            }
            if let Some(doc) = field.doc {
                let mut lines: Vec<&str> = doc.lines().map(str::trim).collect();
                while lines.last().is_some_and(|line| line.is_empty()) {
                    lines.pop();
                }
                for (index, line) in lines.iter().enumerate() {
                    let last = index + 1 == lines.len();
                    if last && field.is_required() {
                        out.push_str(&format!("# {line}  [required]\n"));
                    } else {
                        out.push_str(&format!("# {line}\n"));
                    }
                }
                if lines.is_empty() && field.is_required() {
                    out.push_str("# [required]\n");
                }
            } else if field.is_required() {
                out.push_str("# [required]\n");
            }

            // An explicit alias is the name the platform sets, so it is the
            // name the example must show: writing `SHOP__DATABASE__URL` beside
            // a platform that only ever sets `DATABASE_URL` is worse than
            // writing nothing.
            let name = field
                .env_alias
                .map_or_else(|| key.env_name(prefix), str::to_owned);
            let default = if field.secret {
                ""
            } else {
                field.default.unwrap_or("")
            };
            out.push_str(&format!("{name}={default}\n"));
        }
        out
    }

    /// Render the `moso config` table.
    ///
    /// ```text
    /// profile: production
    ///
    /// name                       "shop"                       config/default.toml:2
    /// database.url               ***                          env DATABASE_URL
    /// ```
    pub fn render_table(&self, resolved: &ResolvedConfig) -> String {
        let mut out = format!("profile: {}\n\n", resolved.profile);
        if resolved.entries.is_empty() {
            return out;
        }

        let key_width = resolved
            .entries
            .iter()
            .map(|entry| entry.key.dotted().chars().count())
            .max()
            .unwrap_or(0);
        let value_width = resolved
            .entries
            .iter()
            .map(|entry| entry.value.chars().count())
            .max()
            .unwrap_or(0);

        for entry in &resolved.entries {
            let key = entry.key.dotted();
            let origin = entry
                .origin
                .as_ref()
                .map_or_else(|| "-".to_owned(), Origin::to_string);
            out.push_str(&format!(
                "{key:<key_width$}  {value:<value_width$}  {origin}\n",
                value = entry.value,
            ));
        }
        out
    }
}

/// How deep a nested configuration graph may go before the walk gives up.
///
/// Sixteen is far past anything a person writes and far short of a stack
/// overflow.
const MAX_NESTING: usize = 16;

/// One configuration field.
#[derive(Debug, Clone)]
pub struct FieldDescriptor {
    /// The field's name, as written in Rust.
    pub name: &'static str,
    /// The type name shown in errors and in `.env.example`: `Url`,
    /// `integer in 1..=1000`.
    pub type_name: &'static str,
    /// The doc comment, which becomes the comment above the key in
    /// `.env.example`.
    pub doc: Option<&'static str>,
    /// The rendered default, or `None` when the field is required.
    pub default: Option<&'static str>,
    /// Whether the value is a secret, and must be redacted everywhere.
    pub secret: bool,
    /// Whether this field is another `Config` type.
    pub nested: Option<&'static ConfigDescriptor>,
    /// An explicit environment-variable alias from `#[config(env = "…")]`.
    pub env_alias: Option<&'static str>,
    /// Whether the value can be reloaded on `SIGHUP`.
    pub reloadable: bool,
}

impl FieldDescriptor {
    /// Whether the field must be supplied by some source.
    pub fn is_required(&self) -> bool {
        self.default.is_none() && self.nested.is_none()
    }
}

/// What the derive tells the loader about one field, per load.
///
/// [`FieldDescriptor`] is the static, tooling-facing description; `FieldSpec` is
/// the load-time one, and carries the two things that are not static: the
/// per-profile default the derive already chose, and nothing else. They are
/// separate because a descriptor must be nameable without a profile in hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec {
    /// The field's name — the last segment of its key.
    pub name: &'static str,
    /// The type name a `missing configuration` error prints.
    pub type_name: &'static str,
    /// Whether the value must be redacted, and whether `${KEY}_FILE` is
    /// consulted.
    pub secret: bool,
    /// An explicit `#[config(env = "…")]` alias.
    pub env_alias: Option<&'static str>,
    /// Level 7: the `#[config(profile(..))]` default for the active profile,
    /// already selected by the derive.
    pub profile_default: Option<&'static str>,
    /// Level 8: the `#[config(default = ..)]` default.
    pub default: Option<&'static str>,
}

impl FieldSpec {
    /// A required field of `type_name`.
    pub const fn new(name: &'static str, type_name: &'static str) -> Self {
        Self {
            name,
            type_name,
            secret: false,
            env_alias: None,
            profile_default: None,
            default: None,
        }
    }

    /// Mark the field secret.
    #[must_use]
    pub const fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    /// Set the `#[config(env = "…")]` alias.
    #[must_use]
    pub const fn env(mut self, alias: &'static str) -> Self {
        self.env_alias = Some(alias);
        self
    }

    /// Set the base default, level 8.
    #[must_use]
    pub const fn default_value(mut self, rendered: &'static str) -> Self {
        self.default = Some(rendered);
        self
    }

    /// Set the per-profile default, level 7.
    #[must_use]
    pub const fn profile_default(mut self, rendered: &'static str) -> Self {
        self.profile_default = Some(rendered);
        self
    }

    /// The spec a static descriptor implies, for the tooling paths that have no
    /// generated code to ask.
    ///
    /// Level 7 is absent: a descriptor does not know which profile is active.
    pub const fn from_descriptor(field: &'static FieldDescriptor) -> Self {
        Self {
            name: field.name,
            type_name: field.type_name,
            secret: field.secret,
            env_alias: field.env_alias,
            profile_default: None,
            default: field.default,
        }
    }

    /// Whether the field must be supplied by some source.
    pub const fn is_required(&self) -> bool {
        self.profile_default.is_none() && self.default.is_none()
    }
}

/// One resolved key, as `moso config` prints it.
#[derive(Debug, Clone)]
pub struct ResolvedEntry {
    /// The dotted key.
    pub key: ConfigKey,
    /// The value, already redacted when the field is secret and already
    /// truncated to [`DISPLAY_WIDTH`].
    pub value: String,
    /// Where it came from, or `None` when nothing supplied it.
    pub origin: Option<Origin>,
    /// Whether the value was redacted.
    pub secret: bool,
}

/// Every key of a [`ConfigDescriptor`], resolved against a loader.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// The profile the values were resolved under.
    pub profile: Profile,
    /// The keys, in declaration order.
    pub entries: Vec<ResolvedEntry>,
}

impl ResolvedConfig {
    /// One entry by dotted key.
    pub fn get(&self, key: &str) -> Option<&ResolvedEntry> {
        self.entries.iter().find(|entry| entry.key.dotted() == key)
    }

    /// The keys still sitting on a default.
    ///
    /// `moso config --check` prints these in `production`: a value nobody set
    /// is the one most likely to be wrong.
    pub fn defaulted(&self) -> Vec<&ResolvedEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.origin.as_ref().is_none_or(Origin::is_default))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// ConfigLoader
// ---------------------------------------------------------------------------

/// The ordered source stack a [`Config`] is read from.
///
/// Holds the sources, the profile, the application prefix and the secret
/// providers. Constructed once and consulted per field, so the environment is
/// read and the TOML parsed exactly once each.
///
/// ```
/// use moso::config::prelude::*;
/// use moso::config::{ConfigLoader, MapSource};
///
/// /// Everything this application reads from its environment.
/// #[derive(moso::Config, Debug)]
/// pub struct AppConfig {
///     /// Where the server listens.
///     #[config(default = "0.0.0.0:3000")]
///     pub bind: SocketAddr,
/// }
///
/// # fn main() {
/// // Sources are consulted in order; the first with a value wins.
/// let loader = ConfigLoader::from_sources([
///     Box::new(MapSource::from([("bind", "127.0.0.1:8080")])) as _,
/// ]);
///
/// let config = AppConfig::load_from(&loader).expect("a complete configuration");
/// assert_eq!(config.bind.port(), 8080);
/// # }
/// ```
///
/// [`ConfigLoader::standard`] builds the production stack — CLI, environment,
/// `.env` in dev and test, profile TOML files, then declared defaults. Building one
/// by hand, as above, is what a test does to pin values without touching the
/// process environment.
pub struct ConfigLoader {
    sources: Vec<Box<dyn ConfigSource>>,
    profile: Profile,
    prefix: String,
    secret_providers: Vec<Arc<dyn SecretProvider>>,
}

impl core::fmt::Debug for ConfigLoader {
    /// Prints the source names in precedence order, which is the thing anyone
    /// debugging configuration actually wants to see.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ConfigLoader")
            .field("profile", &self.profile)
            .field("prefix", &self.prefix)
            .field(
                "sources",
                &self.sources.iter().map(|s| s.name()).collect::<Vec<_>>(),
            )
            .field("secret_providers", &self.secret_providers.len())
            .finish()
    }
}

impl ConfigLoader {
    /// The standard stack for the detected profile, in the documented
    /// precedence order.
    ///
    /// # Errors
    /// A 500-class [`Error`] when a committed TOML file exists but does not
    /// parse. A file that does not exist is not an error.
    pub fn standard() -> Result<Self> {
        let profile = Profile::detect();
        profile.log_resolved();
        Self::for_profile(profile)
    }

    /// The standard stack for an explicit profile.
    ///
    /// # Errors
    /// As [`ConfigLoader::standard`].
    pub fn for_profile(profile: Profile) -> Result<Self> {
        let prefix = std::env::var(PREFIX_ENV).unwrap_or_default();
        let directory =
            std::env::var(CONFIG_DIR_ENV).map_or_else(|_| PathBuf::from(CONFIG_DIR), PathBuf::from);

        let mut sources: Vec<Box<dyn ConfigSource>> = Vec::with_capacity(6);
        sources.push(Box::new(OverrideSource::new()));
        sources.push(Box::new(CliSource::from_env()));
        sources.push(Box::new(EnvSource::new(prefix.clone())));
        // Level 4 is skipped entirely in `production` rather than loaded and
        // ignored: a source that exists but is never consulted is a source
        // somebody will eventually consult by accident.
        if profile.loads_dotenv() {
            sources.push(Box::new(DotEnvSource::discover()));
        }
        sources.push(Box::new(TomlSource::load(
            directory.join(profile.config_file_name()),
        )?));
        sources.push(Box::new(TomlSource::load(
            directory.join(DEFAULT_CONFIG_FILE),
        )?));

        Ok(Self {
            sources,
            profile,
            prefix,
            secret_providers: vec![Arc::new(FileSecretProvider)],
        })
    }

    /// A stack of exactly these sources, highest precedence first.
    ///
    /// What a test uses to pin configuration without touching the environment.
    pub fn from_sources(sources: impl IntoIterator<Item = Box<dyn ConfigSource>>) -> Self {
        Self {
            sources: sources.into_iter().collect(),
            profile: Profile::Test,
            prefix: String::new(),
            secret_providers: Vec::new(),
        }
    }

    /// Set the environment-variable prefix: `SHOP` in `SHOP__DATABASE__URL`.
    ///
    /// Any [`EnvSource`] already in the stack is rebuilt with the new prefix,
    /// so the setting applies wherever it is called — including after
    /// [`ConfigLoader::standard`]. A source that had its well-known aliases
    /// switched off gets them back; call `with_prefix` first if that matters.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        for source in &mut self.sources {
            if source.name() == "env" {
                *source = Box::new(EnvSource::new(self.prefix.clone()));
            }
        }
        self
    }

    /// Replace the profile, for a test that pins one.
    #[must_use]
    pub fn with_profile(mut self, profile: Profile) -> Self {
        self.profile = profile;
        self
    }

    /// Add a source at the bottom of the stack, below everything already there.
    #[must_use]
    pub fn with_source(mut self, source: Box<dyn ConfigSource>) -> Self {
        self.sources.push(source);
        self
    }

    /// Register a secret provider.
    pub fn with_secret_provider(mut self, provider: Arc<dyn SecretProvider>) -> Self {
        self.secret_providers.push(provider);
        self
    }

    /// The first source that has `key`, in precedence order.
    ///
    /// The plain lookup, with no field metadata to consult: it misses
    /// `#[config(env = "…")]` aliases and the two levels of defaults. Use
    /// [`ConfigLoader::value_for`] when a [`FieldSpec`] is in hand, which is
    /// every path the derive takes.
    pub fn get(&self, key: &ConfigKey) -> Option<ConfigValue> {
        self.sources.iter().find_map(|source| source.get(key))
    }

    /// The full eight-level lookup for one field.
    ///
    /// Levels 1 to 6 come from the source stack; then the `${KEY}_FILE` secret
    /// convention; then level 7 and level 8 from `spec`.
    pub fn value_for(&self, key: &ConfigKey, spec: &FieldSpec) -> Option<ConfigValue> {
        for source in &self.sources {
            if let Some(value) = self.lookup(source.as_ref(), key, spec) {
                return Some(value);
            }
        }

        if spec.secret
            && let Some(value) = self.secret_from_file(key)
        {
            return Some(value);
        }

        if let Some(rendered) = spec.profile_default {
            return Some(ConfigValue::string(rendered, Origin::ProfileDefault));
        }
        spec.default
            .map(|rendered| ConfigValue::string(rendered, Origin::Default))
    }

    /// One source, tried by every spelling of `key`.
    fn lookup(
        &self,
        source: &dyn ConfigSource,
        key: &ConfigKey,
        spec: &FieldSpec,
    ) -> Option<ConfigValue> {
        // The prefixed environment spelling first, for the flat-named sources
        // (`.env`) whose own `get` cannot know the prefix.
        if !self.prefix.is_empty()
            && let Some(value) = source.get_alias(&key.env_name(&self.prefix))
        {
            return Some(value);
        }
        if let Some(value) = source.get(key) {
            return Some(value);
        }
        spec.env_alias.and_then(|alias| source.get_alias(alias))
    }

    /// The Docker and Kubernetes secret-mount convention: `${KEY}_FILE` names a
    /// file whose contents are the value.
    ///
    /// Consulted for every secret field, not only one that asked for it: the
    /// convention is universal, the cost is one absent environment variable,
    /// and a deployment that mounts a secret file should not also have to
    /// annotate the struct.
    fn secret_from_file(&self, key: &ConfigKey) -> Option<ConfigValue> {
        let name = format!("{}_FILE", key.env_name(&self.prefix));
        let path = std::env::var(&name).ok().filter(|path| !path.is_empty())?;
        match FileSecretProvider::read(&path) {
            Ok(secret) => Some(ConfigValue::string(
                secret.expose().to_owned(),
                Origin::Env { name },
            )),
            Err(error) => {
                tracing::warn!(
                    target: "moso::config",
                    key = %key,
                    variable = %name,
                    %error,
                    "the secret file named by this variable could not be read"
                );
                None
            }
        }
    }

    /// Resolve a secret through the registered providers.
    ///
    /// The asynchronous escape hatch: `file` is handled inline during the
    /// synchronous load, and anything that needs a network round trip — Vault,
    /// AWS Secrets Manager — goes through here, from `App::build`.
    ///
    /// # Errors
    /// A 500-class [`Error`] when no provider claims `scheme`, or when the one
    /// that does fails.
    pub async fn resolve_secret(
        &self,
        scheme: &str,
        reference: &SecretRef,
    ) -> Result<SecretString> {
        let provider = self
            .secret_providers
            .iter()
            .find(|provider| provider.scheme() == scheme)
            .ok_or_else(|| {
                Error::internal_msg(format!(
                    "no secret provider is registered for the `{scheme}` scheme; register one with \
                     `App::new(cfg).secret_provider(..)`"
                ))
            })?;
        provider.resolve(reference).await
    }

    /// Read one required field, recording a problem rather than failing fast.
    ///
    /// The method `#[derive(Config)]` emits one call to per leaf field.
    pub fn field<T: Coerce>(
        &self,
        prefix: &ConfigKey,
        spec: &FieldSpec,
        errors: &mut BootErrors,
    ) -> Option<T> {
        let key = prefix.child(spec.name);
        match self.value_for(&key, spec) {
            Some(found) => match T::coerce(&found.raw) {
                Ok(value) => Some(value),
                Err(error) => {
                    errors.push(self.invalid(&key, spec, &found, &error));
                    None
                }
            },
            None => {
                errors.push(self.missing(&key, spec));
                None
            }
        }
    }

    /// Read one optional field.
    ///
    /// Absence is a value here, so the outer `Option` is success and the inner
    /// one is presence. Splitting them is what lets `Option<T>` mean "may be
    /// unset" without `None` also meaning "and that was a boot error".
    pub fn optional_field<T: Coerce>(
        &self,
        prefix: &ConfigKey,
        spec: &FieldSpec,
        errors: &mut BootErrors,
    ) -> Option<Option<T>> {
        let key = prefix.child(spec.name);
        match self.value_for(&key, spec) {
            Some(found) => match Option::<T>::coerce(&found.raw) {
                Ok(value) => Some(value),
                Err(error) => {
                    errors.push(self.invalid(&key, spec, &found, &error));
                    None
                }
            },
            None => Some(None),
        }
    }

    /// Read a nested section.
    ///
    /// A thin wrapper over [`Config::load_nested`] that appends the segment, so
    /// the derive never has to build a key itself.
    pub fn section<C: Config>(
        &self,
        prefix: &ConfigKey,
        name: &'static str,
        errors: &mut BootErrors,
    ) -> Option<C> {
        C::load_nested(self, &prefix.child(name), errors)
    }

    /// The `missing configuration` problem for a field nothing supplied.
    fn missing(&self, key: &ConfigKey, spec: &FieldSpec) -> BootError {
        BootError::MissingConfig {
            key: key.dotted(),
            env: self.env_names(key, spec).join(" or "),
            file_key: format!("{}  ->  {} = …", self.file_hint(), key.dotted()),
            expected_type: spec.type_name,
        }
    }

    /// The `invalid configuration` problem for a value that would not coerce.
    fn invalid(
        &self,
        key: &ConfigKey,
        spec: &FieldSpec,
        found: &ConfigValue,
        error: &CoerceError,
    ) -> BootError {
        let alternatives = self.env_names(key, spec);
        BootError::InvalidConfig {
            key: key.dotted(),
            source: found.origin.to_string(),
            expected: error.expected.clone(),
            // A secret that failed to coerce is still a secret, and a boot
            // report is written to a log that outlives the process.
            found: if spec.secret {
                REDACTED.to_owned()
            } else {
                error.found.clone()
            },
            note: (alternatives.len() > 1)
                .then(|| format!("also settable as {}", alternatives[1..].join(" or "))),
        }
    }

    /// Every environment variable that would have supplied `key`, in order.
    fn env_names(&self, key: &ConfigKey, spec: &FieldSpec) -> Vec<String> {
        let mut names = EnvSource::new(self.prefix.clone()).names_for(key);
        if let Some(alias) = spec.env_alias
            && !names.iter().any(|name| name == alias)
        {
            names.push(alias.to_owned());
        }
        if spec.secret {
            names.push(format!("{}_FILE", key.env_name(&self.prefix)));
        }
        names
    }

    /// The committed file a missing value should be written into.
    fn file_hint(&self) -> String {
        self.sources
            .iter()
            .find(|source| source.name().ends_with(".toml"))
            .map_or_else(
                || self.profile.config_file().to_owned(),
                |source| source.name().to_owned(),
            )
    }

    /// The active profile.
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// The environment-variable prefix.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The sources, in precedence order. `moso config` prints this list, and so
    /// does every configuration error, because "where would this value have
    /// come from" is the first question a reader has.
    pub fn sources(&self) -> &[Box<dyn ConfigSource>] {
        &self.sources
    }

    /// The consulted-sources line every configuration error ends with.
    ///
    /// ```text
    /// 5 sources were consulted, in order:
    ///     code, cli flags, env, .env (not found), config/production.toml
    /// ```
    pub fn source_report(&self) -> String {
        self.sources
            .iter()
            .map(|source| {
                if source.available() {
                    source.name().to_owned()
                } else {
                    format!("{} (not found)", source.name())
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The consulted-sources block, as a [`BootError`] the report can render.
    ///
    /// It is not a problem in its own right, and it is rendered in the problem
    /// list anyway: it is the context every configuration problem above it
    /// needs, and [`BootErrors`] has no other place to put a trailer.
    pub fn consulted_sources_note(&self) -> BootError {
        let count = self.sources.len();
        let noun = if count == 1 { "source" } else { "sources" };
        BootError::Other {
            message: format!("{count} configuration {noun} were consulted, in order"),
            notes: vec![self.source_report()],
            fix: None,
        }
    }

    /// Keys present in a source that no field consumes.
    ///
    /// Almost always a typo in a committed TOML file, which is otherwise silent
    /// forever. Reported as a warning rather than an error: a shared file may
    /// legitimately carry keys for a sibling service.
    pub fn unused_keys(&self, descriptor: &ConfigDescriptor) -> Vec<ConfigKey> {
        let known: BTreeSet<String> = descriptor
            .keys(&ConfigKey::root())
            .iter()
            .map(ConfigKey::dotted)
            .collect();

        let mut unused: Vec<ConfigKey> = Vec::new();
        for source in &self.sources {
            for key in source.keys() {
                if !known.contains(&key.dotted()) && !unused.contains(&key) {
                    unused.push(key);
                }
            }
        }
        unused
    }
}

// ---------------------------------------------------------------------------
// Reloadable
// ---------------------------------------------------------------------------

/// A configuration value that can change without a restart.
///
/// Read through [`Reloadable::get`] rather than as a plain field, so the
/// indirection is visible at every use site.
///
/// Only a few things should be reloadable — a log level, a feature flag, a rate
/// limit. Making a database URL reloadable is a trap: the pool was built at
/// boot and will not notice, so the derive rejects `reloadable` on a nested
/// section that a battery consumes at boot.
///
/// # Why `ArcSwap` and not `RwLock`
///
/// A reloadable is read on the hot path — once per request for a log level,
/// once per check for a feature flag — and written approximately never. An
/// `ArcSwap` read is a pointer load and a refcount bump with no lock at all, so
/// a reload cannot stall a request and a request cannot stall a reload.
#[derive(Debug)]
pub struct Reloadable<T> {
    value: arc_swap::ArcSwap<T>,
}

impl<T> Reloadable<T> {
    /// A reloadable holding `value`.
    pub fn new(value: T) -> Self {
        Self {
            value: arc_swap::ArcSwap::from_pointee(value),
        }
    }

    /// A reloadable sharing an existing `Arc`.
    pub fn from_arc(value: Arc<T>) -> Self {
        Self {
            value: arc_swap::ArcSwap::new(value),
        }
    }

    /// The current value.
    ///
    /// Returns an `Arc` rather than a guard, so a caller can never hold a lock
    /// across an `.await` — which would deadlock a reload against a slow
    /// handler.
    pub fn get(&self) -> Arc<T> {
        self.value.load_full()
    }

    /// Replace the value. Called by the `SIGHUP` handler and by the dev-mode
    /// file watcher.
    pub fn set(&self, value: T) {
        self.value.store(Arc::new(value));
    }

    /// Replace the value with an existing `Arc`.
    pub fn set_arc(&self, value: Arc<T>) {
        self.value.store(value);
    }
}

impl<T: Clone> Clone for Reloadable<T> {
    /// Clones the *current value* into an independent cell.
    ///
    /// Two clones do not track each other. A reloadable is meant to live in the
    /// configuration struct that the provider map holds; cloning one is
    /// something the derive does while building it, not a way to share it.
    fn clone(&self) -> Self {
        Self::from_arc(self.get())
    }
}

impl<T: Default> Default for Reloadable<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> From<T> for Reloadable<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: core::fmt::Display> core::fmt::Display for Reloadable<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&*self.get(), f)
    }
}

/// Run `reload` every time the process receives `SIGHUP`.
///
/// The wiring `App` installs when any field is `#[config(reloadable)]`. The
/// closure re-reads the configuration and calls [`Reloadable::set`] on whatever
/// changed; nothing else about the process is disturbed, so an in-flight
/// request that already read the old value keeps it and the next one sees the
/// new one.
///
/// The returned handle keeps the listener alive: dropping it stops the process
/// reacting to `SIGHUP`.
///
/// # Errors
/// A 500-class [`Error`] when the platform has no `SIGHUP` (Windows), or when
/// the signal handler cannot be installed.
pub fn on_sighup<F>(reload: F) -> Result<tokio::task::JoinHandle<()>>
where
    F: FnMut() + Send + 'static,
{
    #[cfg(unix)]
    {
        let mut reload = reload;
        let mut stream = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .map_err(|error| {
                Error::internal_msg(format!("could not listen for SIGHUP: {error}"))
            })?;
        Ok(tokio::spawn(async move {
            while stream.recv().await.is_some() {
                tracing::info!(target: "moso::config", "SIGHUP: reloading configuration");
                reload();
            }
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = reload;
        Err(Error::internal_msg(
            "SIGHUP is not available on this platform; reloadable configuration needs a Unix-like \
             system",
        ))
    }
}

/// The prelude a `#[derive(Config)]` module wants in scope.
///
/// ```
/// use moso::config::prelude::*;
/// use moso::prelude::Config as _;
///
/// /// Everything this application reads from its environment.
/// #[derive(moso::Config, Debug)]
/// pub struct AppConfig {
///     /// Where the server listens.
///     #[config(default = "0.0.0.0:3000")]
///     pub bind: SocketAddr,
///     /// How long a request may take.
///     #[config(default = "30s")]
///     pub timeout: Duration,
///     /// Connection string; never logged.
///     #[config(secret)]
///     pub database_url: SecretString,
/// }
/// # fn main() {
/// let descriptor = AppConfig::descriptor();
/// assert!(descriptor.fields.iter().any(|f| f.name == "timeout"));
/// # }
/// ```
pub mod prelude {
    pub use crate::config::{Config, Profile, Reloadable, SecretBytes, SecretString};
    pub use std::net::SocketAddr;
    pub use std::path::PathBuf;
    pub use std::time::Duration;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::source::DefaultsSource;

    #[test]
    fn profiles_round_trip_through_their_names() {
        for profile in [Profile::Dev, Profile::Test, Profile::Production] {
            assert_eq!(Profile::parse(profile.as_str()), Some(profile));
        }
        assert_eq!(Profile::parse("prod"), Some(Profile::Production));
        assert_eq!(Profile::parse("staging"), None);
    }

    #[test]
    fn only_dev_and_test_load_dotenv() {
        assert!(Profile::Dev.loads_dotenv());
        assert!(Profile::Test.loads_dotenv());
        assert!(!Profile::Production.loads_dotenv());
    }

    #[test]
    fn only_dev_exposes_errors_by_default() {
        assert!(Profile::Dev.exposes_errors());
        assert!(!Profile::Test.exposes_errors());
        assert!(!Profile::Production.exposes_errors());
    }

    #[test]
    fn a_field_without_a_default_is_required() {
        let field = FieldDescriptor {
            name: "public_url",
            type_name: "Url",
            doc: None,
            default: None,
            secret: false,
            nested: None,
            env_alias: None,
            reloadable: false,
        };
        assert!(field.is_required());
    }

    // ── profile detection ────────────────────────────────────────────────

    #[test]
    fn detection_under_the_test_harness_is_test() {
        // `cfg!(test)` is true here, so this asserts the first rule directly
        // and the harness heuristic indirectly. `MOSO_PROFILE` would win, so
        // the assertion is conditional on it being unset — which it is, unless
        // someone is deliberately running the suite under another profile.
        if std::env::var_os(PROFILE_ENV).is_none() {
            assert_eq!(Profile::detect(), Profile::Test);
        }
    }

    #[test]
    fn config_file_names_line_up_with_the_paths() {
        for profile in [Profile::Dev, Profile::Test, Profile::Production] {
            assert_eq!(
                profile.config_file(),
                format!("config/{}", profile.config_file_name())
            );
        }
    }

    #[test]
    fn profiles_coerce_from_a_string() {
        assert_eq!(
            Profile::coerce(&RawValue::String("PRODUCTION".into())).unwrap(),
            Profile::Production
        );
        assert!(Profile::coerce(&RawValue::String("staging".into())).is_err());
    }

    // ── the eight levels ─────────────────────────────────────────────────

    /// The acceptance criterion: one key set at all eight levels resolves to
    /// the highest, and to the next one down as each is removed in turn.
    #[test]
    fn the_documented_precedence_holds_at_every_level() {
        let spec = FieldSpec::new("log", "String")
            .profile_default("profile-default")
            .default_value("base-default");
        let key = ConfigKey::parse("log");

        // Levels 1..=6, highest first.
        let build = |from: usize| -> ConfigLoader {
            let mut sources: Vec<Box<dyn ConfigSource>> = Vec::new();
            let mut overrides = OverrideSource::new();
            overrides.set("log", "level-1-code");
            let layers: Vec<Box<dyn ConfigSource>> = vec![
                Box::new(overrides),
                Box::new(CliSource::from_args(["--log=level-2-cli".to_owned()])),
                // Level 3 (the real environment) cannot be set from a threaded
                // test, so a map source stands in at the same position.
                MapSource::from([("log", "level-3-env")]).boxed(),
                Box::new(DotEnvSource::from_reader(
                    ".env",
                    "LOG=level-4-dotenv\n".as_bytes(),
                )),
                Box::new(
                    TomlSource::from_str_labelled(
                        "config/test.toml",
                        "log = \"level-5-profile-toml\"\n",
                    )
                    .unwrap(),
                ),
                Box::new(
                    TomlSource::from_str_labelled(
                        "config/default.toml",
                        "log = \"level-6-default-toml\"\n",
                    )
                    .unwrap(),
                ),
            ];
            for (index, layer) in layers.into_iter().enumerate() {
                if index + 1 >= from {
                    sources.push(layer);
                }
            }
            ConfigLoader::from_sources(sources)
        };

        let expected = [
            (1, "level-1-code"),
            (2, "level-2-cli"),
            (3, "level-3-env"),
            (4, "level-4-dotenv"),
            (5, "level-5-profile-toml"),
            (6, "level-6-default-toml"),
        ];
        for (from, value) in expected {
            let loader = build(from);
            let found = loader.value_for(&key, &spec).expect("a value");
            assert_eq!(
                String::coerce(&found.raw).unwrap(),
                value,
                "with levels {from}..=8 present"
            );
        }

        // Level 7, then level 8.
        let loader = build(7);
        let found = loader.value_for(&key, &spec).expect("a value");
        assert_eq!(String::coerce(&found.raw).unwrap(), "profile-default");
        assert_eq!(found.origin, Origin::ProfileDefault);

        let base_only = FieldSpec::new("log", "String").default_value("base-default");
        let found = loader.value_for(&key, &base_only).expect("a value");
        assert_eq!(String::coerce(&found.raw).unwrap(), "base-default");
        assert_eq!(found.origin, Origin::Default);

        // Nothing at all.
        let required = FieldSpec::new("log", "String");
        assert!(loader.value_for(&key, &required).is_none());
    }

    #[test]
    fn defaults_sources_can_stand_in_for_levels_seven_and_eight() {
        let loader = ConfigLoader::from_sources([
            Box::new(DefaultsSource::profile_defaults().set("expose_docs", "true"))
                as Box<dyn ConfigSource>,
            Box::new(DefaultsSource::base_defaults().set("expose_docs", "false")),
        ]);
        let spec = FieldSpec::new("expose_docs", "bool");
        let found = loader
            .value_for(&ConfigKey::parse("expose_docs"), &spec)
            .unwrap();
        assert_eq!(found.origin, Origin::ProfileDefault);
        assert!(bool::coerce(&found.raw).unwrap());
    }

    #[test]
    fn an_explicit_env_alias_is_consulted_after_the_canonical_key() {
        let loader = ConfigLoader::from_sources([
            MapSource::from([("RUST_LOG", "from-alias")]).boxed(),
            MapSource::from([("log", "from-key")]).boxed(),
        ]);
        let spec = FieldSpec::new("log", "String").env("RUST_LOG");
        let found = loader.value_for(&ConfigKey::parse("log"), &spec).unwrap();
        // The alias is in the *higher* source, so it wins the source race; the
        // canonical key only wins within one source.
        assert_eq!(String::coerce(&found.raw).unwrap(), "from-alias");

        let one_source = ConfigLoader::from_sources([MapSource::from([
            ("RUST_LOG", "alias"),
            ("log", "canonical"),
        ])
        .boxed()]);
        let found = one_source
            .value_for(&ConfigKey::parse("log"), &spec)
            .unwrap();
        assert_eq!(String::coerce(&found.raw).unwrap(), "canonical");
    }

    // ── boot errors ──────────────────────────────────────────────────────

    fn loader_with(entries: &[(&str, &str)]) -> ConfigLoader {
        let mut source = MapSource::new();
        for (key, value) in entries {
            source = source.set(key, *value);
        }
        ConfigLoader::from_sources([source.boxed()]).with_prefix("shop")
    }

    #[test]
    fn a_missing_and_a_malformed_key_are_reported_together() {
        let loader = loader_with(&[("database.max_connections", "many")]);
        let mut errors = BootErrors::new();
        let root = ConfigKey::root();

        let missing: Option<moso_schema::Url> =
            loader.field(&root, &FieldSpec::new("public_url", "Url"), &mut errors);
        let malformed: Option<u32> = loader.field(
            &ConfigKey::parse("database"),
            &FieldSpec::new("max_connections", "u32"),
            &mut errors,
        );

        assert!(missing.is_none());
        assert!(malformed.is_none());
        assert_eq!(errors.len(), 2, "{errors}");

        let rendered = errors.render(false);
        assert!(
            rendered.contains("missing configuration: public_url"),
            "{rendered}"
        );
        assert!(rendered.contains("SHOP__PUBLIC_URL"), "{rendered}");
        assert!(rendered.contains("Url"), "{rendered}");
        assert!(
            rendered.contains("invalid configuration: database.max_connections"),
            "{rendered}"
        );
        assert!(rendered.contains("\"many\""), "{rendered}");
    }

    #[test]
    fn a_missing_key_names_the_env_var_and_the_file_key() {
        let loader = loader_with(&[]);
        let mut errors = BootErrors::new();
        let _: Option<String> = loader.field(
            &ConfigKey::parse("database"),
            &FieldSpec::new("url", "String"),
            &mut errors,
        );
        let rendered = errors.render(false);
        assert!(rendered.contains("SHOP__DATABASE__URL"), "{rendered}");
        // The well-known alias is offered too, because a platform sets it.
        assert!(rendered.contains("DATABASE_URL"), "{rendered}");
        assert!(rendered.contains("database.url = "), "{rendered}");
    }

    #[test]
    fn an_invalid_value_mentions_the_alternative_spelling() {
        let loader = loader_with(&[("database.url", "postgres://x")]);
        let mut errors = BootErrors::new();
        let _: Option<u32> = loader.field(
            &ConfigKey::parse("database"),
            &FieldSpec::new("url", "u32"),
            &mut errors,
        );
        let rendered = errors.render(false);
        assert!(rendered.contains("also settable as"), "{rendered}");
        assert!(rendered.contains("DATABASE_URL"), "{rendered}");
    }

    #[test]
    fn an_invalid_secret_is_never_quoted_in_the_report() {
        let loader = loader_with(&[("secret_key", "hunter2")]);
        let mut errors = BootErrors::new();
        let _: Option<u32> = loader.field(
            &ConfigKey::root(),
            &FieldSpec::new("secret_key", "u32").secret(),
            &mut errors,
        );
        let rendered = errors.render(false);
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
    }

    #[test]
    fn an_optional_field_absent_is_not_a_problem() {
        let loader = loader_with(&[]);
        let mut errors = BootErrors::new();
        let value: Option<Option<u32>> = loader.optional_field(
            &ConfigKey::root(),
            &FieldSpec::new("workers", "u32"),
            &mut errors,
        );
        assert_eq!(value, Some(None));
        assert!(errors.is_empty());
    }

    #[test]
    fn an_optional_field_that_is_present_and_malformed_is_a_problem() {
        let loader = loader_with(&[("workers", "many")]);
        let mut errors = BootErrors::new();
        let value: Option<Option<u32>> = loader.optional_field(
            &ConfigKey::root(),
            &FieldSpec::new("workers", "u32"),
            &mut errors,
        );
        assert_eq!(value, None);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn the_consulted_sources_are_listed_in_order_with_their_availability() {
        let loader = ConfigLoader::from_sources([
            MapSource::from([("a", "1")]).boxed(),
            Box::new(DotEnvSource::at("/nonexistent/.env")),
            Box::new(TomlSource::load("/nonexistent.toml").unwrap()),
        ]);
        assert_eq!(
            loader.source_report(),
            "map, .env (not found), /nonexistent.toml (not found)"
        );
        let note = loader.consulted_sources_note();
        assert!(note.headline().contains("3 configuration sources"));
    }

    // ── loader plumbing ──────────────────────────────────────────────────

    #[test]
    fn the_prefix_rebuilds_the_env_source() {
        let loader = ConfigLoader::from_sources([Box::new(EnvSource::new("")) as Box<_>])
            .with_prefix("shop");
        assert_eq!(loader.prefix(), "shop");
        assert_eq!(loader.sources().len(), 1);
        let mut errors = BootErrors::new();
        let _: Option<String> = loader.field(
            &ConfigKey::root(),
            &FieldSpec::new("public_url", "Url"),
            &mut errors,
        );
        assert!(errors.render(false).contains("SHOP__PUBLIC_URL"));
    }

    #[test]
    fn unused_keys_are_the_difference_from_the_descriptor() {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            name: "name",
            type_name: "String",
            doc: None,
            default: Some("shop"),
            secret: false,
            nested: None,
            env_alias: None,
            reloadable: false,
        }];
        static DESCRIPTOR: ConfigDescriptor = ConfigDescriptor {
            type_name: "AppConfig",
            fields: FIELDS,
        };

        let loader =
            ConfigLoader::from_sources([
                MapSource::from([("name", "shop"), ("nmae", "typo")]).boxed()
            ]);
        let unused: Vec<String> = loader
            .unused_keys(&DESCRIPTOR)
            .iter()
            .map(ConfigKey::dotted)
            .collect();
        assert_eq!(unused, vec!["nmae".to_owned()]);
    }

    #[test]
    fn a_loader_debugs_as_its_source_chain() {
        let loader = ConfigLoader::from_sources([MapSource::new().boxed()]);
        let rendered = format!("{loader:?}");
        assert!(rendered.contains("sources"), "{rendered}");
        assert!(rendered.contains("map"), "{rendered}");
    }

    // ── descriptors ──────────────────────────────────────────────────────

    static DATABASE_FIELDS: &[FieldDescriptor] = &[
        FieldDescriptor {
            name: "url",
            type_name: "String",
            doc: Some("Postgres connection string."),
            default: None,
            secret: false,
            nested: None,
            env_alias: Some("DATABASE_URL"),
            reloadable: false,
        },
        FieldDescriptor {
            name: "max_connections",
            type_name: "u32",
            doc: None,
            default: Some("10"),
            secret: false,
            nested: None,
            env_alias: None,
            reloadable: false,
        },
    ];

    static DATABASE: ConfigDescriptor = ConfigDescriptor {
        type_name: "DatabaseConfig",
        fields: DATABASE_FIELDS,
    };

    static APP_FIELDS: &[FieldDescriptor] = &[
        FieldDescriptor {
            name: "name",
            type_name: "String",
            doc: Some("Human-readable service name, used in logs and the OpenAPI title."),
            default: Some("shop"),
            secret: false,
            nested: None,
            env_alias: None,
            reloadable: false,
        },
        FieldDescriptor {
            name: "public_url",
            type_name: "Url",
            doc: Some("Base URL used to build absolute links in emails and Location headers."),
            default: None,
            secret: false,
            nested: None,
            env_alias: None,
            reloadable: false,
        },
        FieldDescriptor {
            name: "database",
            type_name: "DatabaseConfig",
            doc: None,
            default: None,
            secret: false,
            nested: Some(&DATABASE),
            env_alias: None,
            reloadable: false,
        },
        FieldDescriptor {
            name: "secret_key",
            type_name: "SecretString",
            doc: None,
            default: Some("do-not-use"),
            secret: true,
            nested: None,
            env_alias: None,
            reloadable: false,
        },
    ];

    static APP: ConfigDescriptor = ConfigDescriptor {
        type_name: "AppConfig",
        fields: APP_FIELDS,
    };

    #[test]
    fn keys_flatten_through_nested_sections_in_declaration_order() {
        let keys: Vec<String> = APP
            .keys(&ConfigKey::root())
            .iter()
            .map(ConfigKey::dotted)
            .collect();
        assert_eq!(
            keys,
            vec![
                "name".to_owned(),
                "public_url".to_owned(),
                "database.url".to_owned(),
                "database.max_connections".to_owned(),
                "secret_key".to_owned(),
            ]
        );
    }

    #[test]
    fn a_nested_field_is_not_itself_required() {
        assert!(!APP.field("database").unwrap().is_required());
        assert!(APP.field("public_url").unwrap().is_required());
    }

    #[test]
    fn env_example_carries_the_docs_the_defaults_and_the_required_marker() {
        let rendered = APP.render_env_example("shop");
        let expected = "\
# Human-readable service name, used in logs and the OpenAPI title.
SHOP__NAME=shop

# Base URL used to build absolute links in emails and Location headers.  [required]
SHOP__PUBLIC_URL=

# Postgres connection string.  [required]
DATABASE_URL=

SHOP__DATABASE__MAX_CONNECTIONS=10

SHOP__SECRET_KEY=
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn env_example_never_writes_a_secrets_default() {
        let rendered = APP.render_env_example("shop");
        assert!(!rendered.contains("do-not-use"), "{rendered}");
    }

    #[test]
    fn env_example_uses_the_alias_when_there_is_one() {
        let rendered = APP.render_env_example("shop");
        assert!(rendered.contains("DATABASE_URL="), "{rendered}");
        assert!(!rendered.contains("SHOP__DATABASE__URL"), "{rendered}");
    }

    #[test]
    fn env_example_drifts_when_a_field_is_added() {
        // The CI drift check in one assertion: the rendering is a pure function
        // of the descriptor, so adding a field changes it.
        static EXTRA: &[FieldDescriptor] = &[FieldDescriptor {
            name: "extra",
            type_name: "String",
            doc: None,
            default: Some("x"),
            secret: false,
            nested: None,
            env_alias: None,
            reloadable: false,
        }];
        static WIDER: ConfigDescriptor = ConfigDescriptor {
            type_name: "AppConfig",
            fields: EXTRA,
        };
        assert_ne!(
            APP.render_env_example("shop"),
            WIDER.render_env_example("shop")
        );
    }

    #[test]
    fn the_table_shows_the_resolved_value_and_its_source() {
        let loader = ConfigLoader::from_sources([MapSource::from([
            ("public_url", "https://api.shop.example"),
            ("database.url", "postgres://app:pw@db/shop"),
            ("secret_key", "hunter2"),
        ])
        .boxed()])
        .with_profile(Profile::Production);

        let resolved = APP.resolve(&loader);
        let table = APP.render_table(&resolved);

        assert!(table.starts_with("profile: production\n\n"), "{table}");
        assert!(table.contains("name"), "{table}");
        assert!(table.contains("\"shop\""), "{table}");
        assert!(table.contains("default"), "{table}");
        assert!(table.contains("database.max_connections"), "{table}");
        // A secret is redacted in the table, never rendered.
        assert!(!table.contains("hunter2"), "{table}");
        assert!(table.contains("***"), "{table}");
    }

    #[test]
    fn the_table_columns_line_up() {
        let loader = ConfigLoader::from_sources([MapSource::new().boxed()]);
        let table = APP.render_table(&APP.resolve(&loader));
        let columns: Vec<usize> = table
            .lines()
            .skip(2)
            .filter(|line| !line.is_empty())
            .map(|line| line.rfind("  ").expect("a gap"))
            .collect();
        assert!(columns.windows(2).all(|pair| pair[0] == pair[1]), "{table}");
    }

    #[test]
    fn resolution_reports_what_is_still_on_a_default() {
        let loader =
            ConfigLoader::from_sources([
                MapSource::from([("public_url", "https://x.example")]).boxed()
            ]);
        let resolved = APP.resolve(&loader);
        assert_eq!(resolved.get("name").unwrap().origin, Some(Origin::Default));
        assert_eq!(
            resolved.get("public_url").unwrap().origin,
            Some(Origin::Code)
        );
        assert_eq!(resolved.get("database.url").unwrap().origin, None);
        assert!(
            resolved
                .get("database.url")
                .unwrap()
                .value
                .contains("not set")
        );

        let defaulted: Vec<String> = resolved
            .defaulted()
            .iter()
            .map(|entry| entry.key.dotted())
            .collect();
        assert!(defaulted.contains(&"name".to_owned()));
        assert!(!defaulted.contains(&"public_url".to_owned()));
    }

    #[test]
    fn a_cyclic_descriptor_does_not_overflow_the_stack() {
        // A hand-written impl can build one; the derive cannot.
        static CYCLE_FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            name: "inner",
            type_name: "Cycle",
            doc: None,
            default: None,
            secret: false,
            nested: Some(&CYCLE),
            env_alias: None,
            reloadable: false,
        }];
        static CYCLE: ConfigDescriptor = ConfigDescriptor {
            type_name: "Cycle",
            fields: CYCLE_FIELDS,
        };
        assert!(CYCLE.keys(&ConfigKey::root()).is_empty());
    }

    // ── field specs ──────────────────────────────────────────────────────

    #[test]
    fn field_specs_build_in_const_context() {
        const SPEC: FieldSpec = FieldSpec::new("log", "String")
            .env("RUST_LOG")
            .default_value("info")
            .profile_default("debug")
            .secret();
        // Read through a binding: the point of the test is that the builders
        // are usable in a `const` item, which the item above has already
        // proved by compiling.
        let spec = SPEC;
        assert_eq!(spec.env_alias, Some("RUST_LOG"));
        assert_eq!(spec.default, Some("info"));
        assert_eq!(spec.profile_default, Some("debug"));
        assert!(spec.secret);
        assert!(!spec.is_required());
        assert!(FieldSpec::new("x", "String").is_required());
    }

    #[test]
    fn a_spec_from_a_descriptor_has_no_profile_default() {
        let spec = FieldSpec::from_descriptor(&APP_FIELDS[0]);
        assert_eq!(spec.name, "name");
        assert_eq!(spec.default, Some("shop"));
        assert_eq!(spec.profile_default, None);
    }

    // ── reloadable ───────────────────────────────────────────────────────

    #[test]
    fn a_reloadable_swaps_without_disturbing_an_existing_reader() {
        let value = Reloadable::new("info".to_owned());
        let held = value.get();
        value.set("debug".to_owned());
        // The reader that already loaded keeps what it loaded — which is the
        // whole point: an in-flight request is not rewritten mid-flight.
        assert_eq!(*held, "info");
        assert_eq!(*value.get(), "debug");
    }

    #[test]
    fn cloning_a_reloadable_makes_an_independent_cell() {
        let original = Reloadable::new(1u32);
        let clone = original.clone();
        original.set(2);
        assert_eq!(*original.get(), 2);
        assert_eq!(*clone.get(), 1);
    }

    #[test]
    fn reloadables_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Reloadable<String>>();
        assert_send_sync::<ConfigLoader>();
    }

    #[test]
    fn a_reloadable_displays_its_current_value() {
        let value = Reloadable::new("info".to_owned());
        assert_eq!(value.to_string(), "info");
        assert_eq!(*Reloadable::<u8>::default().get(), 0);
        assert_eq!(*Reloadable::from(7u8).get(), 7);
    }

    #[tokio::test]
    async fn sighup_wiring_installs_or_says_why_it_cannot() {
        let handle = on_sighup(|| {});
        if cfg!(unix) {
            let handle = handle.expect("a SIGHUP listener on a Unix-like system");
            handle.abort();
        } else {
            assert!(handle.is_err());
        }
    }

    // ── secrets, end to end ──────────────────────────────────────────────

    #[test]
    fn a_secret_field_coerces_into_a_secret_string() {
        let loader = loader_with(&[("secret_key", "hunter2")]);
        let mut errors = BootErrors::new();
        let secret: Option<SecretString> = loader.field(
            &ConfigKey::root(),
            &FieldSpec::new("secret_key", "SecretString").secret(),
            &mut errors,
        );
        assert!(errors.is_empty());
        assert_eq!(secret.unwrap().expose(), "hunter2");
    }

    #[test]
    fn only_the_two_secret_types_satisfy_the_marker() {
        fn assert_secret<T: SecretValue>() {}
        assert_secret::<SecretString>();
        assert_secret::<SecretBytes>();
        // `assert_secret::<String>()` is the compile error the derive emits for
        // `#[config(secret)] pub key: String`. It cannot be written here, which
        // is the point.
    }
}
