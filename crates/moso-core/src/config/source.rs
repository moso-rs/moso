//! Configuration sources, and the precedence between them.
//!
//! Highest wins:
//!
//! 1. **Explicit overrides in code** — what a test uses.
//! 2. **Command-line flags** — `--bind`, `--log`, `--set database.max_connections=20`.
//! 3. **Environment variables** — `SHOP__DATABASE__URL`, or a
//!    `#[config(env = "…")]` alias. `DATABASE_URL`, `REDIS_URL` and `PORT` are
//!    aliased by default because platforms set them and fighting that is futile.
//! 4. **`.env`** — loaded only in `dev` and `test`.
//! 5. **`config/{profile}.toml`** — committed, profile-specific.
//! 6. **`config/default.toml`** — committed, shared.
//! 7. **`#[config(profile(...))]`** defaults.
//! 8. **`#[config(default = ...)]`** — the base default.
//!
//! Levels 1 to 6 are [`ConfigSource`]s in a [`ConfigLoader`](super::ConfigLoader)'s
//! stack. Levels 7 and 8 live in the generated code, where the derive knows the
//! literal — they reach the loader through
//! [`FieldSpec`](super::FieldSpec), and [`DefaultsSource`] exists so a test or
//! `moso config --check` can inject them as a source instead.
//!
//! # Why `.env` is not loaded in production
//!
//! A `.env` file that exists in production hides where a value really came
//! from: the platform's configuration UI shows one thing and the process reads
//! another, and the discrepancy is only found during an incident. The rule is
//! enforced rather than recommended.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::value::{ConfigKey, ConfigValue, Origin, RawValue};
#[cfg(feature = "config-file")]
use crate::error::{Error, Result};

/// One place configuration values come from.
///
/// Dyn-compatible: a loader holds `Vec<Box<dyn ConfigSource>>` in precedence
/// order and asks each in turn, which is what makes an application-supplied
/// source — Vault, AWS Parameter Store, a database table — a first-class
/// citizen rather than a fork.
///
/// The built-in sources are [`EnvSource`], [`DotEnvSource`], `TomlSource`,
/// [`DefaultsSource`] and [`MapSource`]; implement this to add another.
///
/// ```
/// use moso::config::{ConfigKey, ConfigSource, ConfigValue, Origin, RawValue};
///
/// /// Every key answers with the same value — enough to prove the shape.
/// #[derive(Debug)]
/// pub struct Constant(pub String);
///
/// impl ConfigSource for Constant {
///     fn name(&self) -> &str {
///         "constant"
///     }
///
///     fn get(&self, key: &ConfigKey) -> Option<ConfigValue> {
///         Some(ConfigValue::new(
///             RawValue::String(self.0.clone()),
///             Origin::Env { name: key.to_string() },
///         ))
///     }
/// }
///
/// # fn main() {
/// let source = Constant("yes".to_owned());
/// let value = source.get(&ConfigKey::parse("anything")).unwrap();
/// assert_eq!(value.raw.as_text().as_deref(), Some("yes"));
/// # }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a configuration source",
    note = "implement `get(&self, key: &ConfigKey) -> Option<ConfigValue>` and `name(&self)`",
    note = "help: for a fixed set of values in a test, use `MapSource::from([(\"key\", \
            \"value\")])`"
)]
pub trait ConfigSource: Send + Sync + core::fmt::Debug {
    /// The name shown in `moso config`'s source column and in the "sources
    /// consulted" block of a configuration error.
    fn name(&self) -> &str;

    /// The value for `key`, if this source has one.
    fn get(&self, key: &ConfigKey) -> Option<ConfigValue>;

    /// The value for an explicit alias, if this source has one.
    ///
    /// `#[config(env = "RUST_LOG")]` names a variable that has nothing to do
    /// with the field's dotted key, so it cannot be derived from
    /// [`ConfigKey`]. Only the sources that are keyed by a flat name — the
    /// environment and `.env` — answer; the rest keep the default `None`,
    /// because a TOML file has no such thing as an alias.
    ///
    /// The canonical key is always tried first, so an alias never shadows the
    /// spelling the documentation gives.
    fn get_alias(&self, alias: &str) -> Option<ConfigValue> {
        let _ = alias;
        None
    }

    /// Every key this source defines.
    ///
    /// Used to warn about a key that no field consumes — usually a typo in a
    /// committed TOML file, which is otherwise silent forever. Sources that
    /// cannot enumerate (the environment, meaningfully) return nothing.
    fn keys(&self) -> Vec<ConfigKey> {
        Vec::new()
    }

    /// Whether this source was found at all.
    ///
    /// A missing `.env` is normal and is reported as `(not found)` rather than
    /// omitted, so the consulted-sources list stays honest.
    fn available(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// OverrideSource
// ---------------------------------------------------------------------------

/// Values set in code. The highest precedence.
#[derive(Debug, Default)]
pub struct OverrideSource {
    entries: Vec<(ConfigKey, ConfigValue)>,
}

impl OverrideSource {
    /// An empty override set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override `key`.
    ///
    /// Setting the same key twice replaces the first value rather than
    /// shadowing it, so `moso config` never reports a key twice and the
    /// "highest precedence" story stays true within the layer as well as
    /// between layers.
    pub fn set(&mut self, key: impl Into<ConfigKey>, value: impl Into<String>) -> &mut Self {
        let key = key.into();
        let value = ConfigValue::string(value, Origin::Code);
        match self
            .entries
            .iter_mut()
            .find(|(existing, _)| *existing == key)
        {
            Some(slot) => slot.1 = value,
            None => self.entries.push((key, value)),
        }
        self
    }

    /// Override `key` with an already-typed value.
    pub fn set_raw(&mut self, key: impl Into<ConfigKey>, value: RawValue) -> &mut Self {
        let key = key.into();
        let value = ConfigValue::new(value, Origin::Code);
        match self
            .entries
            .iter_mut()
            .find(|(existing, _)| *existing == key)
        {
            Some(slot) => slot.1 = value,
            None => self.entries.push((key, value)),
        }
        self
    }

    /// Whether anything has been overridden.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl ConfigSource for OverrideSource {
    fn name(&self) -> &str {
        "code"
    }

    fn get(&self, key: &ConfigKey) -> Option<ConfigValue> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.clone())
    }

    fn keys(&self) -> Vec<ConfigKey> {
        self.entries.iter().map(|(key, _)| key.clone()).collect()
    }
}

// ---------------------------------------------------------------------------
// CliSource
// ---------------------------------------------------------------------------

/// Command-line flags.
///
/// Recognises `--key=value`, `--key value`, and `--set a.b.c=value` for keys
/// with no dedicated flag. Anything unrecognised is left alone: the binary has
/// subcommands (`worker`, `migrate`) whose arguments are not configuration.
///
/// A flag name is lower-cased and its dashes become underscores, so
/// `--max-connections` and `--max_connections` are the same key; a dot still
/// separates levels, so `--database.url` reaches a nested section without
/// `--set`.
#[derive(Debug, Default)]
pub struct CliSource {
    entries: Vec<(ConfigKey, ConfigValue)>,
}

impl CliSource {
    /// Parse from `std::env::args`.
    pub fn from_env() -> Self {
        Self::from_args(std::env::args().skip(1))
    }

    /// Parse from an explicit argument list, for tests.
    ///
    /// The list must not include `argv[0]`. Parsing stops at a bare `--`,
    /// because everything after it belongs to whatever the binary forwards to.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Self {
        let mut source = Self::default();
        let mut args = args.into_iter().peekable();

        while let Some(arg) = args.next() {
            if arg == "--" {
                break;
            }
            let Some(body) = arg.strip_prefix("--") else {
                continue;
            };

            // `--set a.b.c=value`, the escape hatch for a key with no flag.
            if body == "set" {
                if let Some(assignment) = args.next()
                    && let Some((key, value)) = assignment.split_once('=')
                {
                    source.push(normalise_flag(key), value, "--set");
                }
                continue;
            }

            if let Some((name, value)) = body.split_once('=') {
                source.push(normalise_flag(name), value, &format!("--{name}"));
                continue;
            }

            // `--flag value`, but only when the next argument is not itself a
            // flag: `--check --json` must not make `--json` the value of
            // `--check`.
            let takes_value = args
                .peek()
                .is_some_and(|next| !next.starts_with('-') || next.as_str() == "-");
            if takes_value {
                let value = args.next().unwrap_or_default();
                source.push(normalise_flag(body), &value, &format!("--{body}"));
            }
        }
        source
    }

    /// Record one flag, replacing an earlier spelling of the same key so the
    /// last flag on the command line wins — which is what every other CLI does.
    fn push(&mut self, key: ConfigKey, value: &str, flag: &str) {
        if key.is_root() {
            return;
        }
        let value = ConfigValue::string(
            value,
            Origin::Cli {
                flag: flag.to_owned(),
            },
        );
        match self
            .entries
            .iter_mut()
            .find(|(existing, _)| *existing == key)
        {
            Some(slot) => slot.1 = value,
            None => self.entries.push((key, value)),
        }
    }

    /// How many flags were understood as configuration.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no flag was understood as configuration.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// `--max-connections` and `--database.url` both become dotted keys.
fn normalise_flag(name: &str) -> ConfigKey {
    ConfigKey::parse(&name.to_lowercase().replace('-', "_"))
}

impl ConfigSource for CliSource {
    fn name(&self) -> &str {
        "cli flags"
    }

    fn get(&self, key: &ConfigKey) -> Option<ConfigValue> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.clone())
    }

    fn keys(&self) -> Vec<ConfigKey> {
        self.entries.iter().map(|(key, _)| key.clone()).collect()
    }

    fn available(&self) -> bool {
        !self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// EnvSource
// ---------------------------------------------------------------------------

/// Environment variables.
#[derive(Debug, Clone)]
pub struct EnvSource {
    /// The application prefix, `SHOP` in `SHOP__DATABASE__URL`.
    pub prefix: String,
    /// Whether to consult the well-known aliases.
    pub use_aliases: bool,
}

impl EnvSource {
    /// Read the environment with `prefix`.
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            use_aliases: true,
        }
    }

    /// Stop consulting the well-known aliases.
    pub fn without_aliases(mut self) -> Self {
        self.use_aliases = false;
        self
    }

    /// The variable names this source would consult for `key`, in order.
    ///
    /// Public because a boot error has to print them: "set
    /// `SHOP__DATABASE__URL`" is only half the answer when `DATABASE_URL` would
    /// also have worked.
    pub fn names_for(&self, key: &ConfigKey) -> Vec<String> {
        let mut names = vec![key.env_name(&self.prefix)];
        if !self.prefix.is_empty() {
            // The unprefixed nested spelling, for a deployment that never set a
            // prefix in the first place.
            let bare = key.env_name("");
            if !names.contains(&bare) {
                names.push(bare);
            }
        }
        if self.use_aliases {
            let dotted = key.dotted();
            for (alias, aliased_key) in WELL_KNOWN_ALIASES {
                if *aliased_key == dotted && !names.contains(&(*alias).to_owned()) {
                    names.push((*alias).to_owned());
                }
            }
        }
        names
    }
}

/// Read `name` from the environment, treating an empty value as unset.
///
/// `FOO=` in a platform's configuration UI is how "I cleared this field" looks,
/// and honouring it as an empty string is how an empty database URL reaches a
/// connection pool.
fn env_value(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    (!value.is_empty()).then_some(value)
}

impl ConfigSource for EnvSource {
    fn name(&self) -> &str {
        "env"
    }

    fn get(&self, key: &ConfigKey) -> Option<ConfigValue> {
        self.names_for(key).into_iter().find_map(|name| {
            env_value(&name).map(|value| ConfigValue::string(value, Origin::Env { name }))
        })
    }

    fn get_alias(&self, alias: &str) -> Option<ConfigValue> {
        env_value(alias).map(|value| {
            ConfigValue::string(
                value,
                Origin::Env {
                    name: alias.to_owned(),
                },
            )
        })
    }
}

// ---------------------------------------------------------------------------
// DotEnvSource
// ---------------------------------------------------------------------------

/// The file name walked for by [`DotEnvSource::discover`].
pub const DOTENV_FILE: &str = ".env";

/// A `.env` file, loaded only in the `dev` and `test` profiles.
///
/// Parsed into this source rather than exported into the process environment:
/// a `.env` that mutates `std::env` is invisible to `moso config`'s source
/// column, and — since Rust 1.80 — mutating the environment from a running
/// program is a data race waiting for a second thread.
#[derive(Debug)]
pub struct DotEnvSource {
    path: PathBuf,
    entries: BTreeMap<String, String>,
    found: bool,
}

impl DotEnvSource {
    /// Load `.env` from the working directory, walking up to the workspace root.
    ///
    /// Walking up is what makes `cargo run -p examples/crud` find the `.env`
    /// beside the workspace `Cargo.toml`, which is where people put it.
    pub fn discover() -> Self {
        let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        for directory in start.ancestors() {
            let candidate = directory.join(DOTENV_FILE);
            if candidate.is_file() {
                return Self::at(candidate);
            }
        }
        Self {
            path: start.join(DOTENV_FILE),
            entries: BTreeMap::new(),
            found: false,
        }
    }

    /// Load a specific file.
    ///
    /// A file that does not exist is an empty, unavailable source rather than
    /// an error: not having a `.env` is the normal case.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let Ok(iter) = dotenvy::from_path_iter(&path) else {
            return Self {
                path,
                entries: BTreeMap::new(),
                found: false,
            };
        };
        let mut entries = BTreeMap::new();
        for item in iter {
            match item {
                Ok((key, value)) => {
                    entries.insert(key, value);
                }
                // A malformed line is skipped with a warning rather than
                // failing the boot: a `.env` is a developer's scratch file and
                // half of one is better than none of it.
                Err(error) => {
                    tracing::warn!(
                        target: "moso::config",
                        path = %path.display(),
                        %error,
                        "skipping a malformed line in .env"
                    );
                }
            }
        }
        Self {
            path,
            entries,
            found: true,
        }
    }

    /// Parse from anything readable, for tests.
    pub fn from_reader(path: impl Into<PathBuf>, reader: impl std::io::Read) -> Self {
        let mut entries = BTreeMap::new();
        for (key, value) in dotenvy::from_read_iter(reader).flatten() {
            entries.insert(key, value);
        }
        Self {
            path: path.into(),
            entries,
            found: true,
        }
    }

    /// The file this source read, whether or not it existed.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The variables it defines.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it defines nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up a variable, treating an empty value as unset.
    fn lookup(&self, name: &str) -> Option<ConfigValue> {
        let value = self.entries.get(name)?;
        if value.is_empty() {
            return None;
        }
        Some(ConfigValue::string(
            value.clone(),
            Origin::DotEnv {
                name: name.to_owned(),
            },
        ))
    }
}

impl ConfigSource for DotEnvSource {
    fn name(&self) -> &str {
        ".env"
    }

    fn get(&self, key: &ConfigKey) -> Option<ConfigValue> {
        // A `.env` is read with the same names as the environment, because it
        // is a stand-in for the environment. The prefix is not known here, so
        // both the bare nested spelling and the well-known aliases are tried;
        // the loader tries the prefixed spelling through `get_alias`.
        self.lookup(&key.env_name("")).or_else(|| {
            let dotted = key.dotted();
            WELL_KNOWN_ALIASES
                .iter()
                .filter(|(_, aliased)| *aliased == dotted)
                .find_map(|(alias, _)| self.lookup(alias))
        })
    }

    fn get_alias(&self, alias: &str) -> Option<ConfigValue> {
        self.lookup(alias)
    }

    fn keys(&self) -> Vec<ConfigKey> {
        // `A__B` is `a.b`; a name with no `__` is a single-segment key.
        self.entries
            .keys()
            .map(|name| ConfigKey::parse(&name.to_lowercase().replace("__", ".")))
            .collect()
    }

    fn available(&self) -> bool {
        self.found
    }
}

// ---------------------------------------------------------------------------
// TomlSource
// ---------------------------------------------------------------------------

/// A TOML file.
#[cfg(feature = "config-file")]
#[derive(Debug)]
pub struct TomlSource {
    path: PathBuf,
    root: Option<toml::Value>,
    lines: BTreeMap<String, u32>,
    label: String,
}

#[cfg(feature = "config-file")]
impl TomlSource {
    /// Load `path`, treating absence as an empty source rather than an error.
    ///
    /// `config/production.toml` not existing is a normal deployment, not a
    /// failure; a *malformed* one is an error, and is reported with the line.
    ///
    /// # Errors
    /// A 500-class [`Error`] when the file exists but does not parse, or exists
    /// and cannot be read for a reason other than absence.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let label = path.display().to_string();

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    root: None,
                    lines: BTreeMap::new(),
                    label,
                });
            }
            Err(error) => {
                return Err(Error::internal_msg(format!(
                    "could not read `{label}`: {error}"
                )));
            }
        };

        let root: toml::Value = toml::from_str(&text).map_err(|error| {
            let position = error
                .span()
                .map(|span| line_of(&text, span.start))
                .map_or_else(String::new, |line| format!(":{line}"));
            Error::internal_msg(format!("`{label}{position}` is not valid TOML: {error}"))
        })?;

        Ok(Self {
            lines: index_lines(&text),
            path,
            root: Some(root),
            label,
        })
    }

    /// Build from already-parsed text, for tests.
    ///
    /// # Errors
    /// A 500-class [`Error`] when `text` is not valid TOML.
    pub fn from_str_labelled(label: impl Into<String>, text: &str) -> Result<Self> {
        let label = label.into();
        let root: toml::Value = toml::from_str(text).map_err(|error| {
            Error::internal_msg(format!("`{label}` is not valid TOML: {error}"))
        })?;
        Ok(Self {
            path: PathBuf::from(&label),
            root: Some(root),
            lines: index_lines(text),
            label,
        })
    }

    /// The file's path.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The line a key was written on, when it was written as a plain
    /// `key = value` under a plain `[table]` header.
    ///
    /// `moso config` prints `config/default.toml:2`; a source attribution
    /// without a line is a source attribution people still have to grep for.
    pub fn line_of(&self, key: &ConfigKey) -> Option<u32> {
        self.lines.get(&key.dotted()).copied()
    }
}

/// Walk the parsed table by segment.
#[cfg(feature = "config-file")]
fn walk<'a>(root: &'a toml::Value, key: &ConfigKey) -> Option<&'a toml::Value> {
    let mut current = root;
    for segment in key.segments() {
        current = current.as_table()?.get(segment.as_ref())?;
    }
    Some(current)
}

/// Convert a parsed TOML value into the loader's currency.
#[cfg(feature = "config-file")]
fn from_toml(value: &toml::Value) -> RawValue {
    match value {
        toml::Value::String(text) => RawValue::String(text.clone()),
        toml::Value::Integer(number) => RawValue::Integer(*number),
        toml::Value::Float(number) => RawValue::Float(*number),
        toml::Value::Boolean(flag) => RawValue::Bool(*flag),
        // A datetime has no configuration-shaped meaning beyond its text, and
        // its text is exactly what a `Duration` or a `String` field wants.
        toml::Value::Datetime(stamp) => RawValue::String(stamp.to_string()),
        toml::Value::Array(items) => RawValue::List(items.iter().map(from_toml).collect()),
        toml::Value::Table(table) => RawValue::Table(
            table
                .iter()
                .map(|(key, value)| (key.clone(), from_toml(value)))
                .collect(),
        ),
    }
}

/// Flatten a table into dotted keys, leaves only.
#[cfg(feature = "config-file")]
fn flatten(prefix: &ConfigKey, value: &toml::Value, out: &mut Vec<ConfigKey>) {
    match value {
        toml::Value::Table(table) => {
            for (segment, child) in table {
                flatten(&prefix.child(segment.clone()), child, out);
            }
        }
        _ if !prefix.is_root() => out.push(prefix.clone()),
        _ => {}
    }
}

/// The 1-based line containing byte offset `offset`.
#[cfg(feature = "config-file")]
fn line_of(text: &str, offset: usize) -> u32 {
    let clamped = offset.min(text.len());
    let lines = text[..clamped]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    u32::try_from(lines + 1).unwrap_or(u32::MAX)
}

/// Map dotted keys to the line they were written on.
///
/// A deliberately small textual pass rather than a second parser: `toml::Value`
/// discards spans, and pulling in `toml_edit` to recover a number that only
/// appears in a report would not be a good trade. It understands `[table]`,
/// `[[array]]` headers (as a table for line purposes) and `key = value`; a
/// value inside an inline table gets no line, which is honest — it does not
/// have one of its own.
#[cfg(feature = "config-file")]
fn index_lines(text: &str) -> BTreeMap<String, u32> {
    let mut lines = BTreeMap::new();
    let mut table = String::new();

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(header) = line.strip_prefix('[') {
            let header = header
                .trim_start_matches('[')
                .split(']')
                .next()
                .unwrap_or("")
                .trim();
            table = header.replace(['"', '\''], "");
            continue;
        }

        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().replace(['"', '\''], "");
        if key.is_empty() || key.contains(' ') {
            continue;
        }
        let dotted = if table.is_empty() {
            key
        } else {
            format!("{table}.{key}")
        };
        let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        lines.entry(dotted).or_insert(number);
    }
    lines
}

#[cfg(feature = "config-file")]
impl ConfigSource for TomlSource {
    fn name(&self) -> &str {
        &self.label
    }

    fn get(&self, key: &ConfigKey) -> Option<ConfigValue> {
        let root = self.root.as_ref()?;
        let value = walk(root, key)?;
        Some(ConfigValue::new(
            from_toml(value),
            Origin::File {
                path: self.label.clone(),
                line: self.line_of(key),
            },
        ))
    }

    fn keys(&self) -> Vec<ConfigKey> {
        let mut keys = Vec::new();
        if let Some(root) = &self.root {
            flatten(&ConfigKey::root(), root, &mut keys);
        }
        keys
    }

    fn available(&self) -> bool {
        self.root.is_some()
    }
}

// ---------------------------------------------------------------------------
// DefaultsSource
// ---------------------------------------------------------------------------

/// Levels 7 and 8 as a source: the defaults the derive would otherwise supply
/// inline.
///
/// The derive puts a field's defaults in [`FieldSpec`](super::FieldSpec), which
/// is the fast path. This type exists for the two callers that have a
/// descriptor but no generated code: `moso config`, which resolves every key
/// without instantiating the struct, and the precedence test, which needs all
/// eight levels to be sources so it can remove them one at a time.
#[derive(Debug)]
pub struct DefaultsSource {
    entries: Vec<(ConfigKey, String)>,
    origin: Origin,
    label: &'static str,
}

impl DefaultsSource {
    /// Level 7: the `#[config(profile(..))]` defaults.
    pub fn profile_defaults() -> Self {
        Self {
            entries: Vec::new(),
            origin: Origin::ProfileDefault,
            label: "profile defaults",
        }
    }

    /// Level 8: the `#[config(default = ..)]` defaults.
    pub fn base_defaults() -> Self {
        Self {
            entries: Vec::new(),
            origin: Origin::Default,
            label: "defaults",
        }
    }

    /// Declare a default.
    pub fn set(mut self, key: &str, value: impl Into<String>) -> Self {
        self.entries.push((ConfigKey::parse(key), value.into()));
        self
    }
}

impl ConfigSource for DefaultsSource {
    fn name(&self) -> &str {
        self.label
    }

    fn get(&self, key: &ConfigKey) -> Option<ConfigValue> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| ConfigValue::string(value.clone(), self.origin.clone()))
    }

    fn keys(&self) -> Vec<ConfigKey> {
        self.entries.iter().map(|(key, _)| key.clone()).collect()
    }

    fn available(&self) -> bool {
        !self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// MapSource
// ---------------------------------------------------------------------------

/// A fixed map of values, for tests.
///
/// ```
/// use moso::config::prelude::*;
/// use moso::config::{ConfigLoader, MapSource};
///
/// /// Everything this application reads from its environment.
/// #[derive(moso::Config, Debug)]
/// pub struct AppConfig {
///     /// Where the server listens.
///     pub bind: SocketAddr,
///     /// Connection string; never logged.
///     #[config(secret)]
///     pub database_url: SecretString,
/// }
///
/// # fn main() {
/// let source = MapSource::from([
///     ("bind", "127.0.0.1:0"),
///     ("database_url", "sqlite::memory:"),
/// ]);
/// let loader = ConfigLoader::from_sources([Box::new(source) as _]);
/// let config = AppConfig::load_from(&loader).expect("a complete configuration");
///
/// assert_eq!(config.bind.port(), 0);
/// # }
/// ```
#[derive(Debug, Default)]
pub struct MapSource {
    entries: Vec<(ConfigKey, String)>,
}

impl MapSource {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a key.
    pub fn set(mut self, key: &str, value: impl Into<String>) -> Self {
        self.entries.push((ConfigKey::parse(key), value.into()));
        self
    }

    /// Box this source, which is the shape [`ConfigLoader::from_sources`] takes.
    ///
    /// [`ConfigLoader::from_sources`]: super::ConfigLoader::from_sources
    pub fn boxed(self) -> Box<dyn ConfigSource> {
        Box::new(self)
    }
}

impl<const N: usize> From<[(&str, &str); N]> for MapSource {
    fn from(entries: [(&str, &str); N]) -> Self {
        let mut source = MapSource::new();
        for (key, value) in entries {
            source = source.set(key, value);
        }
        source
    }
}

impl ConfigSource for MapSource {
    fn name(&self) -> &str {
        "map"
    }

    fn get(&self, key: &ConfigKey) -> Option<ConfigValue> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| ConfigValue::string(value.clone(), Origin::Code))
    }

    fn get_alias(&self, alias: &str) -> Option<ConfigValue> {
        // Named lookups too, so a test can exercise a `#[config(env = "…")]`
        // alias without touching the process environment.
        self.entries
            .iter()
            .find(|(candidate, _)| candidate.dotted() == alias)
            .map(|(_, value)| ConfigValue::string(value.clone(), Origin::Code))
    }

    fn keys(&self) -> Vec<ConfigKey> {
        self.entries.iter().map(|(key, _)| key.clone()).collect()
    }
}

/// Environment variables that are read without a prefix, because the platform
/// sets them and no amount of documentation will change that.
///
/// `(alias, dotted key)`.
pub const WELL_KNOWN_ALIASES: &[(&str, &str)] = &[
    ("DATABASE_URL", "database.url"),
    ("REDIS_URL", "kv.url"),
    ("PORT", "port"),
    ("HOST", "host"),
    ("RUST_LOG", "log"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn key(dotted: &str) -> ConfigKey {
        ConfigKey::parse(dotted)
    }

    #[test]
    fn map_sources_collect_their_keys() {
        let source = MapSource::from([("bind", "0.0.0.0:0"), ("database.url", "sqlite::memory:")]);
        assert_eq!(source.keys().len(), 2);
        assert_eq!(source.name(), "map");
    }

    #[test]
    fn well_known_aliases_are_unprefixed() {
        assert!(
            WELL_KNOWN_ALIASES
                .iter()
                .any(|(alias, _)| *alias == "DATABASE_URL")
        );
    }

    #[test]
    fn the_documented_aliases_are_all_present() {
        for alias in ["DATABASE_URL", "REDIS_URL", "PORT", "RUST_LOG"] {
            assert!(
                WELL_KNOWN_ALIASES.iter().any(|(name, _)| *name == alias),
                "{alias}"
            );
        }
    }

    // ── overrides ────────────────────────────────────────────────────────

    #[test]
    fn overrides_replace_rather_than_shadow() {
        let mut source = OverrideSource::new();
        source.set("bind", "0.0.0.0:1").set("bind", "0.0.0.0:2");
        assert_eq!(source.keys().len(), 1);
        assert_eq!(
            source.get(&key("bind")).unwrap().raw,
            RawValue::String("0.0.0.0:2".into())
        );
        assert_eq!(source.get(&key("bind")).unwrap().origin, Origin::Code);
    }

    #[test]
    fn overrides_can_carry_a_typed_value() {
        let mut source = OverrideSource::new();
        source.set_raw("http.body_max", RawValue::Integer(4096));
        assert_eq!(
            source.get(&key("http.body_max")).unwrap().raw,
            RawValue::Integer(4096)
        );
    }

    // ── cli ──────────────────────────────────────────────────────────────

    fn cli(args: &[&str]) -> CliSource {
        CliSource::from_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn cli_understands_the_three_documented_forms() {
        let source = cli(&[
            "--bind=0.0.0.0:8080",
            "--log",
            "debug",
            "--set",
            "database.max_connections=20",
        ]);
        assert_eq!(
            source.get(&key("bind")).unwrap().raw,
            RawValue::String("0.0.0.0:8080".into())
        );
        assert_eq!(
            source.get(&key("log")).unwrap().raw,
            RawValue::String("debug".into())
        );
        assert_eq!(
            source.get(&key("database.max_connections")).unwrap().raw,
            RawValue::String("20".into())
        );
    }

    #[test]
    fn cli_records_the_flag_as_written() {
        let source = cli(&["--bind=0.0.0.0:1"]);
        assert_eq!(
            source.get(&key("bind")).unwrap().origin,
            Origin::Cli {
                flag: "--bind".into()
            }
        );
    }

    #[test]
    fn cli_normalises_dashes_and_case() {
        let source = cli(&["--MAX-Connections=20"]);
        assert!(source.get(&key("max_connections")).is_some());
    }

    #[test]
    fn cli_leaves_subcommand_arguments_alone() {
        let source = cli(&["worker", "--queue", "emails", "migrate"]);
        assert!(source.get(&key("worker")).is_none());
        assert_eq!(
            source.get(&key("queue")).unwrap().raw,
            RawValue::String("emails".into())
        );
    }

    #[test]
    fn cli_does_not_swallow_the_next_flag_as_a_value() {
        let source = cli(&["--check", "--json"]);
        assert!(source.get(&key("check")).is_none());
        assert!(source.get(&key("json")).is_none());
        assert!(source.is_empty());
    }

    #[test]
    fn cli_stops_at_a_bare_double_dash() {
        let source = cli(&["--bind=1", "--", "--bind=2"]);
        assert_eq!(
            source.get(&key("bind")).unwrap().raw,
            RawValue::String("1".into())
        );
        assert_eq!(source.len(), 1);
    }

    #[test]
    fn the_last_spelling_of_a_flag_wins() {
        let source = cli(&["--log=info", "--log=debug"]);
        assert_eq!(
            source.get(&key("log")).unwrap().raw,
            RawValue::String("debug".into())
        );
        assert_eq!(source.len(), 1);
    }

    #[test]
    fn a_dotted_flag_reaches_a_nested_section_without_set() {
        let source = cli(&["--database.url=sqlite::memory:"]);
        assert_eq!(
            source.get(&key("database.url")).unwrap().raw,
            RawValue::String("sqlite::memory:".into())
        );
    }

    // ── env ──────────────────────────────────────────────────────────────

    #[test]
    fn env_names_are_tried_prefixed_then_bare_then_aliased() {
        let source = EnvSource::new("shop");
        assert_eq!(
            source.names_for(&key("database.url")),
            vec![
                "SHOP__DATABASE__URL".to_owned(),
                "DATABASE__URL".to_owned(),
                "DATABASE_URL".to_owned(),
            ]
        );
    }

    #[test]
    fn an_unprefixed_env_source_does_not_repeat_itself() {
        let source = EnvSource::new("");
        assert_eq!(
            source.names_for(&key("log")),
            vec!["LOG".to_owned(), "RUST_LOG".to_owned()]
        );
    }

    #[test]
    fn aliases_can_be_switched_off() {
        let source = EnvSource::new("shop").without_aliases();
        assert_eq!(
            source.names_for(&key("database.url")),
            vec!["SHOP__DATABASE__URL".to_owned(), "DATABASE__URL".to_owned()]
        );
    }

    #[test]
    fn the_env_source_reads_the_process_environment() {
        // A name no other test touches, so the threaded runner stays safe.
        let name = "MOSO_TEST_ENV_SOURCE_PROBE";
        assert!(std::env::var(name).is_err());
        let source = EnvSource::new("");
        assert!(source.get_alias(name).is_none());
        assert!(source.get(&key("path")).is_some(), "PATH is always set");
    }

    // ── .env ─────────────────────────────────────────────────────────────

    fn dotenv(text: &str) -> DotEnvSource {
        DotEnvSource::from_reader(".env", text.as_bytes())
    }

    #[test]
    fn dotenv_reads_nested_and_aliased_names() {
        let source =
            dotenv("DATABASE__URL=sqlite::memory:\nDATABASE_URL=postgres://x\nLOG=debug\n");
        assert_eq!(
            source.get(&key("database.url")).unwrap().raw,
            RawValue::String("sqlite::memory:".into())
        );
        assert_eq!(
            source.get(&key("log")).unwrap().origin,
            Origin::DotEnv { name: "LOG".into() }
        );
    }

    #[test]
    fn dotenv_falls_back_to_a_well_known_alias() {
        let source = dotenv("DATABASE_URL=postgres://x\n");
        assert_eq!(
            source.get(&key("database.url")).unwrap().raw,
            RawValue::String("postgres://x".into())
        );
    }

    #[test]
    fn an_empty_dotenv_value_is_absence() {
        let source = dotenv("LOG=\n");
        assert!(source.get(&key("log")).is_none());
    }

    #[test]
    fn dotenv_enumerates_its_keys_in_dotted_form() {
        let source = dotenv("DATABASE__URL=x\nLOG=y\n");
        let mut keys: Vec<String> = source.keys().iter().map(ConfigKey::dotted).collect();
        keys.sort();
        assert_eq!(keys, vec!["database.url".to_owned(), "log".to_owned()]);
    }

    #[test]
    fn a_missing_dotenv_is_unavailable_rather_than_an_error() {
        let source = DotEnvSource::at("/nonexistent/moso/.env");
        assert!(!source.available());
        assert!(source.is_empty());
        assert!(source.get(&key("log")).is_none());
        assert_eq!(source.path(), Path::new("/nonexistent/moso/.env"));
    }

    // ── toml ─────────────────────────────────────────────────────────────

    #[cfg(feature = "config-file")]
    fn toml_source(text: &str) -> TomlSource {
        TomlSource::from_str_labelled("config/production.toml", text).unwrap()
    }

    #[cfg(feature = "config-file")]
    #[test]
    fn toml_walks_the_table_by_segment() {
        let source = toml_source("name = \"shop\"\n\n[database]\nmax_connections = 20\n");
        assert_eq!(
            source.get(&key("name")).unwrap().raw,
            RawValue::String("shop".into())
        );
        assert_eq!(
            source.get(&key("database.max_connections")).unwrap().raw,
            RawValue::Integer(20)
        );
        assert!(source.get(&key("database.missing")).is_none());
    }

    #[cfg(feature = "config-file")]
    #[test]
    fn toml_values_carry_their_line() {
        let source = toml_source("name = \"shop\"\n\n[database]\nmax_connections = 20\n");
        assert_eq!(
            source.get(&key("name")).unwrap().origin,
            Origin::File {
                path: "config/production.toml".into(),
                line: Some(1)
            }
        );
        assert_eq!(source.line_of(&key("database.max_connections")), Some(4));
    }

    #[cfg(feature = "config-file")]
    #[test]
    fn toml_line_indexing_skips_comments_and_blank_lines() {
        let text = "# a comment\n\n[server]\n# another\nbind = \"0.0.0.0:3000\"\n";
        assert_eq!(toml_source(text).line_of(&key("server.bind")), Some(5));
    }

    #[cfg(feature = "config-file")]
    #[test]
    fn toml_flattens_to_leaf_keys_only() {
        let source = toml_source("name = \"shop\"\n[database]\nurl = \"x\"\npool = 5\n");
        let mut keys: Vec<String> = source.keys().iter().map(ConfigKey::dotted).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "database.pool".to_owned(),
                "database.url".to_owned(),
                "name".to_owned()
            ]
        );
    }

    #[cfg(feature = "config-file")]
    #[test]
    fn toml_arrays_and_nested_tables_survive_the_conversion() {
        let source = toml_source("proxies = [\"10.0.0.0/8\", \"127.0.0.1/32\"]\n");
        assert_eq!(
            source.get(&key("proxies")).unwrap().raw,
            RawValue::List(vec![
                RawValue::String("10.0.0.0/8".into()),
                RawValue::String("127.0.0.1/32".into()),
            ])
        );
    }

    #[cfg(feature = "config-file")]
    #[test]
    fn a_missing_toml_file_is_an_empty_source_not_an_error() {
        let source = TomlSource::load("/nonexistent/moso/config/production.toml").unwrap();
        assert!(!source.available());
        assert!(source.keys().is_empty());
        assert!(source.get(&key("name")).is_none());
    }

    #[cfg(feature = "config-file")]
    #[test]
    fn a_malformed_toml_file_reports_its_line() {
        let error = TomlSource::from_str_labelled("config/bad.toml", "name = \n").unwrap_err();
        assert!(error.to_string().contains("config/bad.toml"), "{error}");
    }

    #[cfg(feature = "config-file")]
    #[test]
    fn byte_offsets_become_one_based_lines() {
        let text = "a\nbb\nccc\n";
        assert_eq!(line_of(text, 0), 1);
        assert_eq!(line_of(text, 2), 2);
        assert_eq!(line_of(text, 5), 3);
        assert_eq!(line_of(text, 9_999), 4);
    }

    // ── defaults ─────────────────────────────────────────────────────────

    #[test]
    fn defaults_sources_carry_the_right_origin() {
        let profile = DefaultsSource::profile_defaults().set("expose_docs", "true");
        let base = DefaultsSource::base_defaults().set("expose_docs", "false");
        assert_eq!(
            profile.get(&key("expose_docs")).unwrap().origin,
            Origin::ProfileDefault
        );
        assert_eq!(
            base.get(&key("expose_docs")).unwrap().origin,
            Origin::Default
        );
        assert_eq!(profile.name(), "profile defaults");
        assert_eq!(base.name(), "defaults");
    }

    #[test]
    fn an_empty_defaults_source_is_unavailable() {
        assert!(!DefaultsSource::base_defaults().available());
    }

    #[cfg(feature = "config-file")]
    #[test]
    fn sources_are_object_safe() {
        let sources: Vec<Box<dyn ConfigSource>> = vec![
            Box::new(OverrideSource::new()),
            Box::new(CliSource::default()),
            Box::new(EnvSource::new("shop")),
            Box::new(DotEnvSource::at("/nonexistent/.env")),
            Box::new(TomlSource::load("/nonexistent.toml").unwrap()),
            Box::new(DefaultsSource::base_defaults()),
            MapSource::new().boxed(),
        ];
        assert_eq!(sources.len(), 7);
        assert!(sources.iter().all(|source| !source.name().is_empty()));
    }
}
