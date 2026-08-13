//! Keys, values, and the coercion rules between them.
//!
//! Every source produces text or something close to it — an environment
//! variable is a `String`, a TOML file is nearly typed, a CLI flag is a
//! `String`. [`ConfigValue`] is the common currency, and [`Coerce`] is the one
//! place the "is `\"true\"` a `bool`" question is answered, so two sources
//! cannot answer it differently.

use std::borrow::Cow;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use crate::config::Reloadable;
use crate::config::secret::{SecretBytes, SecretString};

/// How much of a value a boot error or `moso config` row will print.
///
/// Long enough for a connection string, short enough that a base64 blob does
/// not wrap the terminal.
pub const DISPLAY_WIDTH: usize = 60;

/// A dotted configuration path: `database.max_connections`.
///
/// Rendered differently by each source — `SHOP__DATABASE__MAX_CONNECTIONS` for
/// the environment, `database.max_connections` for TOML — but written once
/// here, so a boot error can name every spelling of the key it is complaining
/// about.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConfigKey {
    segments: Vec<Cow<'static, str>>,
}

impl ConfigKey {
    /// The empty key, the root of a configuration tree.
    pub fn root() -> Self {
        Self::default()
    }

    /// A key from a dotted string.
    ///
    /// Empty segments are dropped, so `""`, `"."` and `"a..b"` all behave, and
    /// a stray separator in a configuration file is not a panic.
    pub fn parse(dotted: &str) -> Self {
        Self {
            segments: dotted
                .split('.')
                .filter(|segment| !segment.is_empty())
                .map(|segment| Cow::Owned(segment.to_owned()))
                .collect(),
        }
    }

    /// This key with `segment` appended.
    pub fn child(&self, segment: impl Into<Cow<'static, str>>) -> Self {
        let mut segments = self.segments.clone();
        segments.push(segment.into());
        Self { segments }
    }

    /// The segments, outermost first.
    pub fn segments(&self) -> &[Cow<'static, str>] {
        &self.segments
    }

    /// Whether this is the root key.
    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// The dotted rendering: `database.max_connections`.
    pub fn dotted(&self) -> String {
        self.segments.join(".")
    }

    /// The environment-variable rendering, with an application prefix:
    /// `SHOP__DATABASE__MAX_CONNECTIONS`.
    ///
    /// Double underscore separates levels so that a single underscore inside a
    /// key name — `max_connections` — is unambiguous.
    pub fn env_name(&self, prefix: &str) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(self.segments.len() + 1);
        if !prefix.is_empty() {
            parts.push(prefix.to_uppercase());
        }
        parts.extend(self.segments.iter().map(|segment| segment.to_uppercase()));
        parts.join("__")
    }

    /// Whether `prefix` is a prefix of this key, segment-wise.
    ///
    /// Segment-wise rather than textual, so `database` is not a prefix of
    /// `database_replica`.
    pub fn starts_with(&self, prefix: &ConfigKey) -> bool {
        prefix.segments.len() <= self.segments.len()
            && prefix
                .segments
                .iter()
                .zip(&self.segments)
                .all(|(a, b)| a == b)
    }
}

impl fmt::Display for ConfigKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.dotted())
    }
}

impl From<&str> for ConfigKey {
    fn from(dotted: &str) -> Self {
        ConfigKey::parse(dotted)
    }
}

impl From<String> for ConfigKey {
    fn from(dotted: String) -> Self {
        ConfigKey::parse(&dotted)
    }
}

/// A configuration value, with the source it came from.
///
/// The source travels with the value because `moso config` prints it and every
/// configuration error quotes it — "invalid value for `database.max_connections`"
/// is half an error message without "from `SHOP__DATABASE__MAX_CONNECTIONS`".
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigValue {
    /// The value itself.
    pub raw: RawValue,
    /// Where it came from, rendered for the report: `env SHOP__BIND`,
    /// `config/production.toml:8`, `default`.
    pub origin: Origin,
}

impl ConfigValue {
    /// A value from `origin`.
    pub fn new(raw: RawValue, origin: Origin) -> Self {
        Self { raw, origin }
    }

    /// A string value.
    pub fn string(value: impl Into<String>, origin: Origin) -> Self {
        Self::new(RawValue::String(value.into()), origin)
    }
}

/// The untyped value a source produced.
#[derive(Debug, Clone, PartialEq)]
pub enum RawValue {
    /// Text, from an environment variable, a flag, or a TOML string.
    String(String),
    /// A TOML integer.
    Integer(i64),
    /// A TOML float.
    Float(f64),
    /// A TOML boolean.
    Bool(bool),
    /// A list.
    List(Vec<RawValue>),
    /// A table, for a nested configuration section.
    Table(Vec<(String, RawValue)>),
}

impl RawValue {
    /// The name used in a type-mismatch error: `string`, `integer`, `table`.
    pub fn type_name(&self) -> &'static str {
        match self {
            RawValue::String(_) => "string",
            RawValue::Integer(_) => "integer",
            RawValue::Float(_) => "float",
            RawValue::Bool(_) => "boolean",
            RawValue::List(_) => "list",
            RawValue::Table(_) => "table",
        }
    }

    /// The text of a scalar, for the coercions that parse.
    ///
    /// A number or a boolean read from TOML answers with its rendering, so
    /// `port = 3000` satisfies a `String` field and `debug = true` satisfies a
    /// `bool` field spelled `"true"` in the environment. Collections answer
    /// `None`, because "the string form of a table" is not a thing a
    /// configuration file author meant.
    pub fn as_text(&self) -> Option<Cow<'_, str>> {
        match self {
            RawValue::String(value) => Some(Cow::Borrowed(value)),
            RawValue::Integer(value) => Some(Cow::Owned(value.to_string())),
            RawValue::Float(value) => Some(Cow::Owned(value.to_string())),
            RawValue::Bool(value) => Some(Cow::Owned(value.to_string())),
            RawValue::List(_) | RawValue::Table(_) => None,
        }
    }

    /// The value rendered for `moso config`, at most `max_len` characters.
    ///
    /// Strings are quoted and escaped, so a trailing space or an embedded
    /// newline is visible rather than invisible; collections are abbreviated to
    /// their shape; everything is truncated with an ASCII ellipsis, because a
    /// report that wraps is a report nobody reads.
    pub fn display(&self, max_len: usize) -> String {
        let rendered = match self {
            RawValue::String(value) => format!("{value:?}"),
            RawValue::Integer(value) => value.to_string(),
            RawValue::Float(value) => value.to_string(),
            RawValue::Bool(value) => value.to_string(),
            RawValue::List(items) => {
                let inner = items
                    .iter()
                    .map(|item| item.display(max_len))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("[{inner}]")
            }
            RawValue::Table(entries) => {
                let inner = entries
                    .iter()
                    .map(|(key, _)| key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{{inner}}}")
            }
        };
        truncate(&rendered, max_len)
    }
}

/// Shorten `text` to `max_len` characters, marking the cut with `...`.
fn truncate(text: &str, max_len: usize) -> String {
    let length = text.chars().count();
    if length <= max_len {
        return text.to_owned();
    }
    let keep = max_len.saturating_sub(3);
    let head: String = text.chars().take(keep).collect();
    format!("{head}...")
}

/// Where a value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Set in code, which overrides everything.
    Code,
    /// A command-line flag.
    Cli {
        /// The flag as it was written.
        flag: String,
    },
    /// An environment variable.
    Env {
        /// The variable's name.
        name: String,
    },
    /// A `.env` file, loaded only in the `dev` and `test` profiles.
    DotEnv {
        /// The variable's name.
        name: String,
    },
    /// A TOML file.
    File {
        /// The file's path.
        path: String,
        /// The line, when the parser reported one.
        line: Option<u32>,
    },
    /// A per-profile default declared with `#[config(profile(..))]`.
    ProfileDefault,
    /// The base default declared with `#[config(default = ..)]`.
    Default,
}

impl Origin {
    /// Whether this origin is a default rather than a value someone supplied.
    ///
    /// `moso config --check` uses it to answer "is anything still on its
    /// default in production?", which is the question behind most of the
    /// configuration incidents worth preventing.
    pub fn is_default(&self) -> bool {
        matches!(self, Origin::ProfileDefault | Origin::Default)
    }
}

impl fmt::Display for Origin {
    /// The rendering `moso config` and every configuration error use.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Code => f.write_str("code"),
            Origin::Cli { flag } => write!(f, "cli {flag}"),
            Origin::Env { name } => write!(f, "env {name}"),
            Origin::DotEnv { name } => write!(f, ".env {name}"),
            Origin::File {
                path,
                line: Some(line),
            } => write!(f, "{path}:{line}"),
            Origin::File { path, line: None } => f.write_str(path),
            Origin::ProfileDefault => f.write_str("profile default"),
            Origin::Default => f.write_str("default"),
        }
    }
}

/// Turn a [`RawValue`] into a Rust type.
///
/// Implemented for the scalar types configuration fields actually have. The
/// derive generates a call per field, so this trait is where "a `bool` field
/// accepts `1`" is decided — once, rather than per source.
///
/// ```
/// use moso::config::{Coerce, RawValue};
/// use std::time::Duration;
///
/// // The same written value is read differently by different field types …
/// assert!(bool::coerce(&RawValue::String("1".to_owned())).unwrap());
/// assert!(bool::coerce(&RawValue::String("yes".to_owned())).unwrap());
///
/// // … and a duration accepts the humane spelling.
/// assert_eq!(
///     Duration::coerce(&RawValue::String("30s".to_owned())).unwrap(),
///     Duration::from_secs(30),
/// );
///
/// // A failure says what was expected, and is what a boot report prints.
/// let error = u16::coerce(&RawValue::String("nope".to_owned())).unwrap_err();
/// assert!(error.to_string().contains(<u16 as Coerce>::TYPE_NAME));
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be a configuration value",
    note = "supported: String, bool, the integer and float types, SocketAddr, IpAddr, PathBuf, \
            Duration, Url, SecretString, SecretBytes, and Option/Vec of those",
    note = "help: for a nested section, add `#[derive(moso::Config)]` to `{Self}` and mark the \
            field `#[config(nested)]`",
    note = "help: for anything else, implement `FromStr` for `{Self}` and mark the field \
            `#[config(parse)]`"
)]
pub trait Coerce: Sized {
    /// The type name a boot error prints: `integer in 1..=1000`, `URL`.
    const TYPE_NAME: &'static str;

    /// Coerce, or explain what was expected.
    ///
    /// # Errors
    /// [`CoerceError`] naming what was expected and what was found. The `found`
    /// half is rendered by [`RawValue::display`], so it is bounded in length —
    /// an implementation must never hand back the whole value.
    fn coerce(value: &RawValue) -> Result<Self, CoerceError>;
}

/// Why a value could not be coerced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoerceError {
    /// What the field expected: `integer in 1..=1000`.
    pub expected: String,
    /// What was found, rendered.
    pub found: String,
}

impl CoerceError {
    /// An error saying `expected`, having found `found`.
    pub fn new(expected: impl Into<String>, found: impl Into<String>) -> Self {
        Self {
            expected: expected.into(),
            found: found.into(),
        }
    }

    /// The common case: `T::TYPE_NAME` was expected, and this is what arrived.
    ///
    /// Centralised so no implementation accidentally interpolates the raw value
    /// at full length into a message a log will keep forever.
    pub fn mismatch<T: Coerce>(value: &RawValue) -> Self {
        Self::new(T::TYPE_NAME, value.display(DISPLAY_WIDTH))
    }
}

impl fmt::Display for CoerceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "expected {}, found {}", self.expected, self.found)
    }
}

impl core::error::Error for CoerceError {}

/// The strings a configuration `bool` accepts as true.
///
/// Deliberately wider than Rust's `FromStr`: platforms set `1`, Docker Compose
/// sets `yes`, and a configuration that rejects them is a configuration people
/// fight.
pub const TRUTHY: &[&str] = &["1", "true", "yes", "on", "y"];

/// The strings a configuration `bool` accepts as false.
pub const FALSY: &[&str] = &["0", "false", "no", "off", "n"];

// ---------------------------------------------------------------------------
// Coerce — scalars
// ---------------------------------------------------------------------------

impl Coerce for String {
    const TYPE_NAME: &'static str = "string";

    fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
        value
            .as_text()
            .map(Cow::into_owned)
            .ok_or_else(|| CoerceError::mismatch::<Self>(value))
    }
}

impl Coerce for bool {
    const TYPE_NAME: &'static str = "boolean";

    fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
        if let RawValue::Bool(flag) = value {
            return Ok(*flag);
        }
        let text = value
            .as_text()
            .ok_or_else(|| CoerceError::mismatch::<Self>(value))?;
        let lowered = text.trim().to_ascii_lowercase();
        if TRUTHY.contains(&lowered.as_str()) {
            return Ok(true);
        }
        if FALSY.contains(&lowered.as_str()) {
            return Ok(false);
        }
        Err(CoerceError::new(
            "a boolean (true/false, 1/0, yes/no, on/off)",
            value.display(DISPLAY_WIDTH),
        ))
    }
}

/// Implement [`Coerce`] for an integer type.
///
/// Every one of them accepts a TOML integer (range-checked), a string that
/// parses, and a float with no fractional part — because `port = 3000.0` in a
/// hand-edited file is a typo, not a type error worth a boot failure.
macro_rules! coerce_integer {
    ($($ty:ty => $name:literal),* $(,)?) => {$(
        impl Coerce for $ty {
            const TYPE_NAME: &'static str = $name;

            fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
                match value {
                    RawValue::Integer(number) => <$ty>::try_from(*number)
                        .map_err(|_| CoerceError::mismatch::<Self>(value)),
                    RawValue::Float(number) if number.fract() == 0.0 && number.is_finite() => {
                        <$ty>::try_from(*number as i64)
                            .map_err(|_| CoerceError::mismatch::<Self>(value))
                    }
                    RawValue::String(text) => text
                        .trim()
                        .parse::<$ty>()
                        .map_err(|_| CoerceError::mismatch::<Self>(value)),
                    other => Err(CoerceError::mismatch::<Self>(other)),
                }
            }
        }
    )*};
}

coerce_integer! {
    i8 => "integer", i16 => "integer", i32 => "integer", i64 => "integer",
    i128 => "integer", isize => "integer",
    u8 => "non-negative integer", u16 => "non-negative integer",
    u32 => "non-negative integer", u64 => "non-negative integer",
    u128 => "non-negative integer", usize => "non-negative integer",
}

/// Implement [`Coerce`] for a float type.
macro_rules! coerce_float {
    ($($ty:ty),* $(,)?) => {$(
        impl Coerce for $ty {
            const TYPE_NAME: &'static str = "number";

            fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
                match value {
                    #[allow(
                        clippy::cast_possible_truncation,
                        clippy::cast_precision_loss,
                        reason = "narrowing an f64 to an f32, or an i64 past f32's exact \
                                  integer range, is what the operator asked for by declaring \
                                  the field that type; refusing would make `f32` unusable"
                    )]
                    RawValue::Float(number) => Ok(*number as $ty),
                    #[allow(
                        clippy::cast_precision_loss,
                        reason = "same trade in the other direction: an i64 beyond 2^53 loses \
                                  precision as an f64, and a configuration value that large is \
                                  not one anybody wrote by hand"
                    )]
                    RawValue::Integer(number) => Ok(*number as $ty),
                    RawValue::String(text) => text
                        .trim()
                        .parse::<$ty>()
                        .map_err(|_| CoerceError::mismatch::<Self>(value)),
                    other => Err(CoerceError::mismatch::<Self>(other)),
                }
            }
        }
    )*};
}

coerce_float!(f32, f64);

/// Implement [`Coerce`] by delegating to `FromStr`.
macro_rules! coerce_from_str {
    ($($ty:ty => $name:literal),* $(,)?) => {$(
        impl Coerce for $ty {
            const TYPE_NAME: &'static str = $name;

            fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
                let text = value
                    .as_text()
                    .ok_or_else(|| CoerceError::mismatch::<Self>(value))?;
                text.trim()
                    .parse::<$ty>()
                    .map_err(|_| CoerceError::mismatch::<Self>(value))
            }
        }
    )*};
}

coerce_from_str! {
    SocketAddr => "socket address, such as `0.0.0.0:3000`",
    IpAddr => "IP address",
    Ipv4Addr => "IPv4 address",
    Ipv6Addr => "IPv6 address",
}

impl Coerce for PathBuf {
    const TYPE_NAME: &'static str = "path";

    fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
        value
            .as_text()
            .map(|text| PathBuf::from(text.as_ref()))
            .ok_or_else(|| CoerceError::mismatch::<Self>(value))
    }
}

impl Coerce for Duration {
    const TYPE_NAME: &'static str = "duration, such as `30s`, `5m` or `1h30m`";

    fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
        // A bare number is seconds. Every orchestrator's configuration UI emits
        // one, and `timeout = 30` meaning half a minute is what the reader of
        // the file expects.
        match value {
            RawValue::Integer(seconds) if *seconds >= 0 => {
                return Ok(Duration::from_secs(*seconds as u64));
            }
            RawValue::Float(seconds) if seconds.is_finite() && *seconds >= 0.0 => {
                return Duration::try_from_secs_f64(*seconds)
                    .map_err(|_| CoerceError::mismatch::<Self>(value));
            }
            _ => {}
        }

        let text = value
            .as_text()
            .ok_or_else(|| CoerceError::mismatch::<Self>(value))?;
        let text = text.trim();
        if let Ok(seconds) = text.parse::<u64>() {
            return Ok(Duration::from_secs(seconds));
        }
        humantime::parse_duration(text).map_err(|_| CoerceError::mismatch::<Self>(value))
    }
}

impl Coerce for moso_schema::Url {
    const TYPE_NAME: &'static str = "URL";

    fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
        let text = value
            .as_text()
            .ok_or_else(|| CoerceError::mismatch::<Self>(value))?;
        moso_schema::Url::parse(text.trim()).map_err(|_| CoerceError::mismatch::<Self>(value))
    }
}

impl Coerce for SecretString {
    const TYPE_NAME: &'static str = "secret string";

    fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
        match value.as_text() {
            Some(text) => Ok(SecretString::new(text.into_owned())),
            // Deliberately reports the *type* and not the value: a secret that
            // failed to coerce is still a secret, and a boot error is written
            // to a log that outlives the process.
            None => Err(CoerceError::new(
                Self::TYPE_NAME,
                format!("a {} value", value.type_name()),
            )),
        }
    }
}

impl Coerce for SecretBytes {
    const TYPE_NAME: &'static str = "secret bytes, optionally `base64:…` or `hex:…`";

    fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
        let text = value.as_text().ok_or_else(|| {
            CoerceError::new(Self::TYPE_NAME, format!("a {} value", value.type_name()))
        })?;
        let text = text.trim();
        // Explicit prefixes rather than sniffing: "is this hex or is it a
        // password that happens to be sixteen hex characters" is not a question
        // a framework should answer by guessing.
        if let Some(encoded) = text.strip_prefix("base64:") {
            return SecretBytes::from_base64(encoded)
                .map_err(|_| CoerceError::new(Self::TYPE_NAME, "an invalid base64 value"));
        }
        if let Some(encoded) = text.strip_prefix("hex:") {
            return SecretBytes::from_hex(encoded)
                .map_err(|_| CoerceError::new(Self::TYPE_NAME, "an invalid hex value"));
        }
        Ok(SecretBytes::new(text.as_bytes().to_vec()))
    }
}

// ---------------------------------------------------------------------------
// Coerce — wrappers
// ---------------------------------------------------------------------------

impl<T: Coerce> Coerce for Option<T> {
    const TYPE_NAME: &'static str = T::TYPE_NAME;

    /// An empty string is absence, not a value.
    ///
    /// `FOO=` in a `.env` or a platform UI means "I did not set this"; treating
    /// it as `Some("")` turns an unset variable into a silent empty database
    /// URL, which is the failure mode this exists to prevent.
    fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
        if let RawValue::String(text) = value
            && text.trim().is_empty()
        {
            return Ok(None);
        }
        T::coerce(value).map(Some)
    }
}

impl<T: Coerce> Coerce for Vec<T> {
    const TYPE_NAME: &'static str = "list";

    /// A TOML array, or a comma-separated string.
    ///
    /// The string form exists because an environment variable cannot be an
    /// array, and `trusted_proxies` has to be settable from one.
    fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
        let items: Vec<RawValue> = match value {
            RawValue::List(items) => items.clone(),
            RawValue::String(text) => {
                let text = text.trim();
                if text.is_empty() {
                    return Ok(Vec::new());
                }
                text.split(',')
                    .map(|part| RawValue::String(part.trim().to_owned()))
                    .collect()
            }
            other => {
                return Err(CoerceError::new(
                    format!("a list of {}", T::TYPE_NAME),
                    other.display(DISPLAY_WIDTH),
                ));
            }
        };

        items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                T::coerce(item).map_err(|error| {
                    CoerceError::new(format!("{} at index {index}", error.expected), error.found)
                })
            })
            .collect()
    }
}

impl<T: Coerce> Coerce for Reloadable<T> {
    const TYPE_NAME: &'static str = T::TYPE_NAME;

    fn coerce(value: &RawValue) -> Result<Self, CoerceError> {
        T::coerce(value).map(Reloadable::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_nest_and_render() {
        let key = ConfigKey::root().child("database").child("max_connections");
        assert_eq!(key.dotted(), "database.max_connections");
        assert_eq!(key.segments().len(), 2);
        assert!(!key.is_root());
    }

    #[test]
    fn the_root_key_is_empty() {
        assert!(ConfigKey::root().is_root());
        assert_eq!(ConfigKey::root().dotted(), "");
    }

    #[test]
    fn parsing_drops_empty_segments() {
        assert_eq!(ConfigKey::parse("a..b").dotted(), "a.b");
        assert!(ConfigKey::parse("").is_root());
        assert!(ConfigKey::parse(".").is_root());
    }

    #[test]
    fn env_names_use_a_double_underscore_between_levels() {
        let key = ConfigKey::parse("database.max_connections");
        assert_eq!(key.env_name("shop"), "SHOP__DATABASE__MAX_CONNECTIONS");
        assert_eq!(key.env_name(""), "DATABASE__MAX_CONNECTIONS");
    }

    #[test]
    fn prefixes_are_compared_segment_wise() {
        let key = ConfigKey::parse("database.url");
        assert!(key.starts_with(&ConfigKey::parse("database")));
        assert!(key.starts_with(&ConfigKey::root()));
        assert!(!key.starts_with(&ConfigKey::parse("database_replica")));
        assert!(!key.starts_with(&ConfigKey::parse("database.url.extra")));
    }

    #[test]
    fn raw_values_name_their_type() {
        assert_eq!(RawValue::Integer(1).type_name(), "integer");
        assert_eq!(RawValue::String(String::new()).type_name(), "string");
    }

    #[test]
    fn truthy_and_falsy_do_not_overlap() {
        for value in TRUTHY {
            assert!(!FALSY.contains(value));
        }
    }

    // ── display ──────────────────────────────────────────────────────────

    #[test]
    fn display_quotes_strings_and_leaves_scalars_bare() {
        assert_eq!(RawValue::String("shop".into()).display(40), "\"shop\"");
        assert_eq!(RawValue::Integer(3000).display(40), "3000");
        assert_eq!(RawValue::Bool(true).display(40), "true");
    }

    #[test]
    fn display_makes_invisible_characters_visible() {
        let value = RawValue::String("trailing ".into());
        assert_eq!(value.display(40), "\"trailing \"");
        assert_eq!(RawValue::String("a\nb".into()).display(40), "\"a\\nb\"");
    }

    #[test]
    fn display_truncates_with_an_ascii_ellipsis() {
        let value = RawValue::String("x".repeat(200));
        let rendered = value.display(20);
        assert_eq!(rendered.chars().count(), 20);
        assert!(rendered.ends_with("..."));
        assert!(rendered.is_ascii());
    }

    #[test]
    fn display_abbreviates_collections() {
        let list = RawValue::List(vec![RawValue::Integer(1), RawValue::Integer(2)]);
        assert_eq!(list.display(40), "[1, 2]");
        let table = RawValue::Table(vec![
            ("url".to_owned(), RawValue::String("x".into())),
            ("pool".to_owned(), RawValue::Integer(5)),
        ]);
        assert_eq!(table.display(40), "{url, pool}");
    }

    // ── origins ──────────────────────────────────────────────────────────

    #[test]
    fn origins_render_as_documented() {
        assert_eq!(Origin::Code.to_string(), "code");
        assert_eq!(
            Origin::Env {
                name: "SHOP__BIND".into()
            }
            .to_string(),
            "env SHOP__BIND"
        );
        assert_eq!(
            Origin::DotEnv {
                name: "DATABASE_URL".into()
            }
            .to_string(),
            ".env DATABASE_URL"
        );
        assert_eq!(
            Origin::Cli {
                flag: "--bind".into()
            }
            .to_string(),
            "cli --bind"
        );
        assert_eq!(
            Origin::File {
                path: "config/production.toml".into(),
                line: Some(8)
            }
            .to_string(),
            "config/production.toml:8"
        );
        assert_eq!(
            Origin::File {
                path: "config/default.toml".into(),
                line: None
            }
            .to_string(),
            "config/default.toml"
        );
        assert_eq!(Origin::ProfileDefault.to_string(), "profile default");
        assert_eq!(Origin::Default.to_string(), "default");
    }

    #[test]
    fn only_the_two_default_origins_are_defaults() {
        assert!(Origin::Default.is_default());
        assert!(Origin::ProfileDefault.is_default());
        assert!(!Origin::Code.is_default());
        assert!(!Origin::Env { name: "X".into() }.is_default());
    }

    // ── coercion ─────────────────────────────────────────────────────────

    fn text(value: &str) -> RawValue {
        RawValue::String(value.to_owned())
    }

    #[test]
    fn strings_accept_every_scalar() {
        assert_eq!(String::coerce(&text("shop")).unwrap(), "shop");
        assert_eq!(String::coerce(&RawValue::Integer(3000)).unwrap(), "3000");
        assert_eq!(String::coerce(&RawValue::Bool(true)).unwrap(), "true");
        assert!(String::coerce(&RawValue::List(Vec::new())).is_err());
    }

    #[test]
    fn booleans_accept_the_documented_spellings() {
        for value in TRUTHY {
            assert!(bool::coerce(&text(value)).unwrap(), "{value}");
            assert!(
                bool::coerce(&text(&value.to_uppercase())).unwrap(),
                "{value}"
            );
        }
        for value in FALSY {
            assert!(!bool::coerce(&text(value)).unwrap(), "{value}");
        }
        assert!(bool::coerce(&RawValue::Bool(true)).unwrap());
    }

    #[test]
    fn a_boolean_error_lists_what_is_accepted() {
        let error = bool::coerce(&text("maybe")).unwrap_err();
        assert!(error.expected.contains("yes/no"), "{error}");
        assert_eq!(error.found, "\"maybe\"");
    }

    #[test]
    fn integers_range_check_rather_than_wrap() {
        assert_eq!(u16::coerce(&RawValue::Integer(3000)).unwrap(), 3000);
        assert!(u16::coerce(&RawValue::Integer(70_000)).is_err());
        assert!(u32::coerce(&RawValue::Integer(-1)).is_err());
        assert_eq!(i64::coerce(&text(" 42 ")).unwrap(), 42);
        assert!(i64::coerce(&text("many")).is_err());
    }

    #[test]
    fn integers_accept_a_whole_float() {
        assert_eq!(u16::coerce(&RawValue::Float(3000.0)).unwrap(), 3000);
        assert!(u16::coerce(&RawValue::Float(3000.5)).is_err());
    }

    #[test]
    fn floats_accept_integers() {
        assert!((f64::coerce(&RawValue::Integer(2)).unwrap() - 2.0).abs() < f64::EPSILON);
        assert!((f64::coerce(&text("0.25")).unwrap() - 0.25).abs() < f64::EPSILON);
        assert!(f64::coerce(&text("lots")).is_err());
    }

    #[test]
    fn socket_addresses_and_ips_parse() {
        assert_eq!(
            SocketAddr::coerce(&text("0.0.0.0:3000")).unwrap(),
            SocketAddr::from(([0, 0, 0, 0], 3000))
        );
        assert!(SocketAddr::coerce(&text("0.0.0.0")).is_err());
        assert!(IpAddr::coerce(&text("127.0.0.1")).is_ok());
    }

    #[test]
    fn a_socket_address_error_shows_the_shape() {
        let error = SocketAddr::coerce(&text("localhost")).unwrap_err();
        assert!(error.expected.contains("0.0.0.0:3000"), "{error}");
    }

    #[test]
    fn durations_accept_humantime_and_bare_seconds() {
        assert_eq!(
            Duration::coerce(&text("30s")).unwrap(),
            Duration::from_secs(30)
        );
        assert_eq!(
            Duration::coerce(&text("1h30m")).unwrap(),
            Duration::from_secs(5400)
        );
        assert_eq!(
            Duration::coerce(&text("30")).unwrap(),
            Duration::from_secs(30)
        );
        assert_eq!(
            Duration::coerce(&RawValue::Integer(25)).unwrap(),
            Duration::from_secs(25)
        );
        assert_eq!(
            Duration::coerce(&RawValue::Float(0.5)).unwrap(),
            Duration::from_millis(500)
        );
        assert!(Duration::coerce(&text("soon")).is_err());
        assert!(Duration::coerce(&RawValue::Integer(-1)).is_err());
    }

    #[test]
    fn urls_must_be_absolute() {
        assert_eq!(
            moso_schema::Url::coerce(&text("https://api.shop.example"))
                .unwrap()
                .as_str(),
            "https://api.shop.example/"
        );
        assert!(moso_schema::Url::coerce(&text("/relative")).is_err());
    }

    #[test]
    fn paths_are_taken_verbatim() {
        assert_eq!(
            PathBuf::coerce(&text("/var/lib/shop")).unwrap(),
            PathBuf::from("/var/lib/shop")
        );
    }

    #[test]
    fn optional_fields_treat_an_empty_string_as_absence() {
        assert_eq!(Option::<u32>::coerce(&text("")).unwrap(), None);
        assert_eq!(Option::<u32>::coerce(&text("   ")).unwrap(), None);
        assert_eq!(Option::<u32>::coerce(&text("7")).unwrap(), Some(7));
        assert!(Option::<u32>::coerce(&text("seven")).is_err());
    }

    #[test]
    fn lists_come_from_arrays_or_comma_separated_text() {
        assert_eq!(
            Vec::<String>::coerce(&text("10.0.0.0/8, 192.168.0.0/16")).unwrap(),
            vec!["10.0.0.0/8".to_owned(), "192.168.0.0/16".to_owned()]
        );
        assert_eq!(
            Vec::<String>::coerce(&text("")).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            Vec::<u16>::coerce(&RawValue::List(vec![
                RawValue::Integer(1),
                RawValue::Integer(2)
            ]))
            .unwrap(),
            vec![1, 2]
        );
    }

    #[test]
    fn a_bad_list_element_names_its_index() {
        let error = Vec::<u16>::coerce(&text("1, two, 3")).unwrap_err();
        assert!(error.expected.contains("at index 1"), "{error}");
    }

    #[test]
    fn secret_strings_never_quote_the_value_in_an_error() {
        let error = SecretString::coerce(&RawValue::List(Vec::new())).unwrap_err();
        assert_eq!(error.found, "a list value");
        let secret = SecretString::coerce(&text("hunter2")).unwrap();
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn secret_bytes_decode_only_with_an_explicit_prefix() {
        assert_eq!(
            SecretBytes::coerce(&text("hex:00ff")).unwrap().expose(),
            &[0x00, 0xff]
        );
        assert_eq!(
            SecretBytes::coerce(&text("base64:aGk=")).unwrap().expose(),
            b"hi"
        );
        // No prefix: the bytes are the text, not a guess at an encoding.
        assert_eq!(
            SecretBytes::coerce(&text("00ff")).unwrap().expose(),
            b"00ff"
        );
        let error = SecretBytes::coerce(&text("hex:zz")).unwrap_err();
        assert_eq!(error.found, "an invalid hex value");
    }

    #[test]
    fn reloadables_coerce_their_inner_type() {
        let value: Reloadable<String> = Reloadable::coerce(&text("debug")).unwrap();
        assert_eq!(*value.get(), "debug");
    }

    #[test]
    fn a_coerce_error_reads_as_a_sentence() {
        let error = CoerceError::new("integer in 1..=1000", "\"many\"");
        assert_eq!(
            error.to_string(),
            "expected integer in 1..=1000, found \"many\""
        );
    }
}
