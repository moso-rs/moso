//! Human-readable validation messages, and how to replace them.
//!
//! A [`FieldError`](crate::FieldError) carries a machine code and a set of
//! parameters; the message is a rendering of those two. Moso ships an English
//! renderer ([`default_message`]) and an extension point ([`MessageProvider`])
//! so an application can translate, reword, or brand every message without
//! touching its models.
//!
//! `MessageProvider` is deliberately **dyn-compatible**: applications register
//! one instance with `.provide_dyn::<dyn MessageProvider>(…)` and Moso stores
//! it behind an `Arc`.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::validate::codes;

/// A BCP 47 language tag, normalised to `language[-REGION]`.
///
/// This is intentionally a thin wrapper rather than a full CLDR
/// implementation: Moso only ever uses it to select a message bundle, and
/// pulling in an ICU stack for that would be disproportionate.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Locale(Cow<'static, str>);

impl Locale {
    /// English — the locale of the bundled default messages.
    pub const EN: Locale = Locale(Cow::Borrowed("en"));

    /// Wrap an already-normalised tag without checking it.
    ///
    /// Prefer [`Locale::parse`] for anything that came from a request.
    #[must_use]
    pub const fn from_static(tag: &'static str) -> Self {
        Self(Cow::Borrowed(tag))
    }

    /// Parse and normalise a BCP 47 tag: `EN-gb` becomes `en-GB`.
    ///
    /// Normalisation follows the conventional casing: the language subtag
    /// lowercase, a two-letter region uppercase, a four-letter script
    /// titlecase, everything else lowercase. Surrounding whitespace is
    /// ignored, so a tag lifted straight out of a header list is accepted.
    ///
    /// # Errors
    /// Returns [`InvalidLocale`] when the tag is empty, has an empty subtag,
    /// has a subtag longer than eight characters, does not start with a
    /// two-to-eight-letter language subtag, or contains anything other than
    /// ASCII alphanumerics and `-`.
    pub fn parse(tag: &str) -> Result<Self, InvalidLocale> {
        let trimmed = tag.trim();
        if !is_well_formed(trimmed) {
            return Err(InvalidLocale::new(tag));
        }
        Ok(Self(Cow::Owned(normalise(trimmed))))
    }

    /// The whole tag, e.g. `"en-GB"`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The primary language subtag, e.g. `"en"`.
    #[must_use]
    pub fn language(&self) -> &str {
        self.0.split('-').next().unwrap_or_default()
    }

    /// The region subtag if present, e.g. `Some("GB")`.
    ///
    /// A four-letter script subtag in second position is skipped, so
    /// `zh-Hant-TW` reports `TW` and `zh-Hant` reports `None`. Both the
    /// alphabetic (`GB`) and the numeric (`419`) forms are recognised.
    #[must_use]
    pub fn region(&self) -> Option<&str> {
        let mut subtags = self.0.split('-').skip(1);
        let mut subtag = subtags.next()?;
        if is_script(subtag) {
            subtag = subtags.next()?;
        }
        is_region(subtag).then_some(subtag)
    }

    /// Pick the best match for an `Accept-Language` header value.
    ///
    /// Quality values are honoured; unparsable entries are skipped. Returns
    /// `None` when nothing in the header is usable, so the caller can apply its
    /// own default rather than being handed a guess.
    ///
    /// `q=0` means "not acceptable" and is skipped rather than ranked last.
    /// Ties keep the earlier entry, which is the order the client expressed.
    /// `*` is not a language tag and is therefore skipped: a wildcard means
    /// "anything", which is the same answer as `None`.
    #[must_use]
    pub fn from_accept_language(header: &str) -> Option<Self> {
        let mut best: Option<(f32, Locale)> = None;
        for entry in header.split(',') {
            let mut parts = entry.split(';');
            let tag = parts.next().unwrap_or_default().trim();
            let Some(quality) = parse_quality(parts) else {
                continue;
            };
            if quality <= 0.0 {
                continue;
            }
            let Ok(locale) = Locale::parse(tag) else {
                continue;
            };
            if best.as_ref().is_none_or(|(best, _)| quality > *best) {
                best = Some((quality, locale));
            }
        }
        best.map(|(_, locale)| locale)
    }
}

/// Read the `q=` parameter out of an `Accept-Language` entry's parameters.
///
/// Returns `None` when a `q` is present but unusable, which discards the whole
/// entry — a malformed weight is not a reason to promote it to `q=1`.
fn parse_quality<'a>(parameters: impl Iterator<Item = &'a str>) -> Option<f32> {
    let mut quality = 1.0;
    for parameter in parameters {
        let parameter = parameter.trim();
        let Some((key, value)) = parameter.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("q") {
            continue;
        }
        match value.trim().parse::<f32>() {
            Ok(value) if (0.0..=1.0).contains(&value) => quality = value,
            _ => return None,
        }
    }
    Some(quality)
}

/// True when every subtag is usable and the first one is a language.
fn is_well_formed(tag: &str) -> bool {
    if tag.is_empty() {
        return false;
    }
    let mut subtags = tag.split('-');
    let language = subtags.next().unwrap_or_default();
    if !(2..=8).contains(&language.len()) || !language.bytes().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    subtags.all(|subtag| {
        (1..=8).contains(&subtag.len()) && subtag.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// Apply the conventional BCP 47 casing.
///
/// Everything after a singleton subtag (`en-u-ca-gregory`, `en-x-private`) is
/// an extension or private-use sequence and stays lowercase, so the `ca` in
/// that example is not mistaken for the region `CA`.
fn normalise(tag: &str) -> String {
    let mut out = String::with_capacity(tag.len());
    let mut in_extension = false;
    for (position, subtag) in tag.split('-').enumerate() {
        if position > 0 {
            out.push('-');
        }
        if position > 0 && subtag.len() == 1 {
            in_extension = true;
        }
        if in_extension {
            out.extend(subtag.chars().map(|c| c.to_ascii_lowercase()));
        } else if position > 0 && is_script(subtag) {
            let mut chars = subtag.chars();
            if let Some(first) = chars.next() {
                out.push(first.to_ascii_uppercase());
            }
            out.extend(chars.map(|c| c.to_ascii_lowercase()));
        } else if position > 0 && is_region(subtag) {
            out.extend(subtag.chars().map(|c| c.to_ascii_uppercase()));
        } else {
            out.extend(subtag.chars().map(|c| c.to_ascii_lowercase()));
        }
    }
    out
}

/// A four-letter subtag — an ISO 15924 script code.
fn is_script(subtag: &str) -> bool {
    subtag.len() == 4 && subtag.bytes().all(|b| b.is_ascii_alphabetic())
}

/// A two-letter or three-digit subtag — an ISO 3166-1 or UN M.49 region code.
fn is_region(subtag: &str) -> bool {
    (subtag.len() == 2 && subtag.bytes().all(|b| b.is_ascii_alphabetic()))
        || (subtag.len() == 3 && subtag.bytes().all(|b| b.is_ascii_digit()))
}

impl Default for Locale {
    fn default() -> Self {
        Self::EN
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Locale {
    type Err = InvalidLocale;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl AsRef<str> for Locale {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A string that is not a usable BCP 47 language tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidLocale {
    /// The rejected input, truncated to 32 characters so a hostile
    /// `Accept-Language` header cannot bloat a log line.
    pub input: String,
}

impl InvalidLocale {
    /// How much of the rejected input is retained.
    pub const MAX_INPUT: usize = 32;

    /// Record `input` as unusable, truncating it on a character boundary.
    #[must_use]
    pub fn new(input: &str) -> Self {
        let end = input
            .char_indices()
            .nth(Self::MAX_INPUT)
            .map_or(input.len(), |(i, _)| i);
        Self {
            input: input[..end].to_owned(),
        }
    }
}

impl fmt::Display for InvalidLocale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` is not a valid BCP 47 language tag", self.input)
    }
}

impl std::error::Error for InvalidLocale {}

/// Supplies human messages for validation codes.
///
/// Return `None` for any code you do not translate; Moso falls back to
/// [`default_message`], so a provider only has to cover what it wants to
/// change.
///
/// `params` holds the constraint's parameters — `min`, `max`, `pattern`,
/// `format`, … — keyed exactly as documented for the code.
///
/// [`DefaultMessages`] is the bundled implementation; supply your own to
/// translate, or to reword a message for a particular audience.
///
/// ```
/// use moso::schema::{Locale, MessageProvider, codes};
/// use std::collections::BTreeMap;
/// use serde_json::Value;
///
/// /// French messages for the one code this application cares about.
/// pub struct French;
///
/// impl MessageProvider for French {
///     fn message(
///         &self,
///         code: &str,
///         params: &BTreeMap<&'static str, Value>,
///         _locale: &Locale,
///     ) -> Option<String> {
///         if code != codes::LEN {
///             return None;   // fall back to the bundled English
///         }
///         let min = params.get("min")?;
///         Some(format!("doit contenir au moins {min} caractères"))
///     }
/// }
///
/// # fn main() {
/// let mut params = BTreeMap::new();
/// params.insert("min", Value::from(3));
///
/// assert_eq!(
///     French.message(codes::LEN, &params, &Locale::default()).as_deref(),
///     Some("doit contenir au moins 3 caractères"),
/// );
/// assert!(French.message(codes::PATTERN, &params, &Locale::default()).is_none());
/// # }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a validation message provider",
    label = "does not implement `MessageProvider`",
    note = "a provider must be `Send + Sync + 'static` because it is shared by every request",
    note = "help: implement the single method:\n    impl moso::MessageProvider for {Self} {{\n        \
            fn message(&self, code: &str, params: &Params, locale: &moso::Locale)\n            \
            -> Option<String> {{ None }}\n    }}"
)]
pub trait MessageProvider: Send + Sync + 'static {
    /// Render `code` for `locale`, or `None` to fall back to the default.
    fn message(
        &self,
        code: &str,
        params: &BTreeMap<&'static str, Value>,
        locale: &Locale,
    ) -> Option<String>;
}

/// The bundled English provider. Always returns a message, so it is a valid
/// terminal provider in a chain.
///
/// There is no bundled Fluent, ICU, or other localisation runtime, and this is
/// deliberate (ADR-0017): pulling a heavy i18n stack into every build to serve
/// the subset that translates is the wrong default. [`MessageProvider`] is the
/// seam instead — an
/// application that wants translated or reworded messages registers its own with
/// `.provide_dyn::<dyn MessageProvider>(…)`, optionally wrapping this one in a
/// [`ChainedMessages`] to override a handful of codes and fall through to the
/// English here for the rest.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultMessages;

impl MessageProvider for DefaultMessages {
    fn message(
        &self,
        code: &str,
        params: &BTreeMap<&'static str, Value>,
        _locale: &Locale,
    ) -> Option<String> {
        Some(default_message(code, params).into_owned())
    }
}

/// Try each provider in order, taking the first non-`None` message.
///
/// Lets an application override a handful of codes without reimplementing the
/// rest.
pub struct ChainedMessages(Vec<std::sync::Arc<dyn MessageProvider>>);

impl ChainedMessages {
    /// An empty chain, which always falls through to the default messages.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Append a provider to the end of the chain.
    #[must_use]
    pub fn with(mut self, provider: std::sync::Arc<dyn MessageProvider>) -> Self {
        self.0.push(provider);
        self
    }
}

impl Default for ChainedMessages {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ChainedMessages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainedMessages")
            .field("providers", &self.0.len())
            .finish()
    }
}

impl MessageProvider for ChainedMessages {
    fn message(
        &self,
        code: &str,
        params: &BTreeMap<&'static str, Value>,
        locale: &Locale,
    ) -> Option<String> {
        self.0.iter().find_map(|p| p.message(code, params, locale))
    }
}

/// The bundled English message for `code`, rendered with `params`.
///
/// This is the fallback for every provider and the direct source of messages
/// when an application registers none. It never fails: an unknown code renders
/// as a generic "this value is not valid" rather than panicking, because an
/// unknown code is a bug in a library, not a reason to 500 a request.
///
/// # Parameter keys
///
/// | code | keys |
/// | --- | --- |
/// | `len` | `min`, `max`, `unit` (defaults to `characters`) |
/// | `range` | `min`, `max`, `exclusive_min`, `exclusive_max` |
/// | `pattern` | `pattern`, or one of `starts_with` / `ends_with` / `contains` |
/// | `format` | `format` |
/// | `enum` | `allowed` (array) |
/// | `multiple_of` | `multiple_of` |
/// | `type` | `expected` |
#[must_use]
pub fn default_message(code: &str, params: &BTreeMap<&'static str, Value>) -> Cow<'static, str> {
    let get = |k: &str| params.get(k);
    match code {
        codes::REQUIRED => Cow::Borrowed("this field is required"),
        UNKNOWN_FIELD => Cow::Borrowed("is not a field of this type"),
        codes::TYPE => match get("expected") {
            Some(v) => Cow::Owned(format!("must be {}", render(v))),
            None => Cow::Borrowed("has the wrong type"),
        },
        codes::LEN => {
            let unit = get("unit").and_then(Value::as_str).unwrap_or(UNIT_DEFAULT);
            match (get("min"), get("max")) {
                (Some(min), Some(max)) if min == max => {
                    Cow::Owned(format!("must be exactly {}", quantity(min, unit)))
                }
                (Some(min), Some(max)) => Cow::Owned(format!(
                    "must be between {} and {}",
                    render(min),
                    quantity(max, unit)
                )),
                // `non_empty` is `min = 1`, and "must be at least 1 character"
                // is a worse way of saying it.
                (Some(min), None) if is_one(min) => Cow::Borrowed("must not be empty"),
                (Some(min), None) => {
                    Cow::Owned(format!("must be at least {}", quantity(min, unit)))
                }
                (None, Some(max)) => Cow::Owned(format!("must be at most {}", quantity(max, unit))),
                (None, None) => Cow::Borrowed("has an invalid length"),
            }
        }
        codes::RANGE => {
            let ex_min = get("exclusive_min")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let ex_max = get("exclusive_max")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match (get("min"), get("max")) {
                (Some(min), Some(max)) if !ex_min && !ex_max => Cow::Owned(format!(
                    "must be between {} and {}",
                    render(min),
                    render(max)
                )),
                (Some(min), Some(max)) => Cow::Owned(format!(
                    "must be {} {} and {} {}",
                    if ex_min { "greater than" } else { "at least" },
                    render(min),
                    if ex_max { "less than" } else { "at most" },
                    render(max)
                )),
                (Some(min), None) => Cow::Owned(format!(
                    "must be {} {}",
                    if ex_min { "greater than" } else { "at least" },
                    render(min)
                )),
                (None, Some(max)) => Cow::Owned(format!(
                    "must be {} {}",
                    if ex_max { "less than" } else { "at most" },
                    render(max)
                )),
                (None, None) => Cow::Borrowed("is out of range"),
            }
        }
        // `starts_with`/`ends_with`/`contains` report `pattern` so a client
        // only has to know one code, but "must match ^ORD\-" is a poor way to
        // say "must start with ORD-".
        codes::PATTERN => {
            if let Some(v) = get("starts_with") {
                Cow::Owned(format!("must start with `{}`", render(v)))
            } else if let Some(v) = get("ends_with") {
                Cow::Owned(format!("must end with `{}`", render(v)))
            } else if let Some(v) = get("contains") {
                Cow::Owned(format!("must contain `{}`", render(v)))
            } else if let Some(p) = get("pattern") {
                Cow::Owned(format!("must match {}", render(p)))
            } else {
                Cow::Borrowed("has an invalid format")
            }
        }
        codes::FORMAT => match get("format").and_then(Value::as_str) {
            Some(f) => Cow::Owned(format!("must be a valid {}", format_noun(f))),
            None => Cow::Borrowed("has an invalid format"),
        },
        codes::ENUM => match get("allowed").and_then(Value::as_array) {
            Some(values) => Cow::Owned(format!(
                "must be one of: {}",
                values.iter().map(render).collect::<Vec<_>>().join(", ")
            )),
            None => Cow::Borrowed("is not a permitted value"),
        },
        codes::UNIQUE => Cow::Borrowed("must not contain duplicate values"),
        codes::MULTIPLE_OF => match get("multiple_of") {
            Some(m) => Cow::Owned(format!("must be a multiple of {}", render(m))),
            None => Cow::Borrowed("is not a permitted multiple"),
        },
        _ => Cow::Borrowed("this value is not valid"),
    }
}

/// The `unit` a `len` error assumes when it does not say.
const UNIT_DEFAULT: &str = "characters";

/// The code `deny_unknown` reports; see
/// [`ErrorCode::Custom`](crate::ErrorCode::Custom).
const UNKNOWN_FIELD: &str = "custom:unknown_field";

/// Render a parameter value for embedding in a message: strings unquoted,
/// everything else as compact JSON.
fn render(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// True when `v` is numerically one, whichever JSON number type it is.
fn is_one(v: &Value) -> bool {
    v.as_u64() == Some(1) || v.as_i64() == Some(1) || v.as_f64() == Some(1.0)
}

/// `"32 characters"`, or `"1 character"` — an English message that says
/// "1 characters" reads as a bug in the framework.
fn quantity(n: &Value, unit: &str) -> String {
    match unit.strip_suffix('s') {
        Some(singular) if is_one(n) => format!("{} {singular}", render(n)),
        _ => format!("{} {unit}", render(n)),
    }
}

/// The noun a `format` failure names.
///
/// "must be a valid phone-e164" is a schema keyword read aloud; the point of
/// the message is to be the part a human understands.
fn format_noun(format: &str) -> &str {
    match format {
        "email" => "email address",
        "uri" => "URL",
        "uuid" => "UUID",
        "hostname" => "host name",
        "ipv4" => "IPv4 address",
        "ipv6" => "IPv6 address",
        "ip-cidr" => "IP network in CIDR notation",
        "date-time" => "date and time",
        "phone-e164" => "phone number in E.164 format",
        "json-pointer" => "JSON Pointer",
        "regex" => "regular expression",
        "byte" => "base64-encoded value",
        other => other,
    }
}

/// Message keys the bundled renderer understands, for tests and for the CLI's
/// error-code reference.
#[must_use]
pub fn default_message_codes() -> &'static [&'static str] {
    codes::ALL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_locale_is_english() {
        assert_eq!(Locale::default(), Locale::EN);
        assert_eq!(Locale::EN.as_str(), "en");
    }

    #[test]
    fn renders_the_documented_english_messages() {
        let mut p = BTreeMap::new();
        p.insert("min", Value::from(3));
        p.insert("max", Value::from(32));
        assert_eq!(
            default_message(codes::LEN, &p),
            "must be between 3 and 32 characters"
        );
        p.insert("unit", Value::from("items"));
        assert_eq!(
            default_message(codes::LEN, &p),
            "must be between 3 and 32 items"
        );

        let mut p = BTreeMap::new();
        p.insert("min", Value::from(1));
        assert_eq!(default_message(codes::RANGE, &p), "must be at least 1");
        p.insert("exclusive_min", Value::from(true));
        assert_eq!(default_message(codes::RANGE, &p), "must be greater than 1");

        let mut p = BTreeMap::new();
        p.insert("pattern", Value::from("^[a-z]+$"));
        assert_eq!(default_message(codes::PATTERN, &p), "must match ^[a-z]+$");

        assert_eq!(
            default_message(codes::REQUIRED, &BTreeMap::new()),
            "this field is required"
        );
        assert_eq!(
            default_message("custom:match", &BTreeMap::new()),
            "this value is not valid"
        );
    }

    #[test]
    fn chained_provider_is_empty_by_default() {
        let c = ChainedMessages::new();
        assert_eq!(
            c.message("len", &BTreeMap::new(), &Locale::EN),
            None,
            "an empty chain must fall through"
        );
    }

    fn params(pairs: &[(&'static str, Value)]) -> BTreeMap<&'static str, Value> {
        pairs.iter().cloned().collect()
    }

    // ── locale ─────────────────────────────────────────────────────────

    #[test]
    fn parse_normalises_casing() {
        for (input, expected) in [
            ("en", "en"),
            ("EN", "en"),
            ("EN-gb", "en-GB"),
            (" en-gb ", "en-GB"),
            ("zh-hant-tw", "zh-Hant-TW"),
            ("ES-419", "es-419"),
            ("en-GB-oxendict", "en-GB-oxendict"),
            ("en-u-ca-gregory", "en-u-ca-gregory"),
        ] {
            let locale = Locale::parse(input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
            assert_eq!(locale.as_str(), expected);
        }
    }

    #[test]
    fn parse_rejects_malformed_tags() {
        for input in [
            "",
            "-",
            "e",
            "en-",
            "-en",
            "en--GB",
            "en_GB",
            "en-toolongsubtag",
            "*",
            "en;q=1",
        ] {
            assert!(
                Locale::parse(input).is_err(),
                "{input:?} should not be a locale"
            );
        }
    }

    #[test]
    fn invalid_locale_truncates_its_input() {
        let long = "!".repeat(200);
        let err = Locale::parse(&long).expect_err("not a tag");
        assert_eq!(err.input.chars().count(), InvalidLocale::MAX_INPUT);

        // Truncation happens on a character boundary, not a byte one.
        let multibyte = "é".repeat(200);
        let err = Locale::parse(&multibyte).expect_err("not a tag");
        assert_eq!(err.input.chars().count(), InvalidLocale::MAX_INPUT);
    }

    #[test]
    fn subtags_are_addressable() {
        let locale = Locale::parse("en-GB").expect("valid");
        assert_eq!(locale.language(), "en");
        assert_eq!(locale.region(), Some("GB"));

        let locale = Locale::parse("zh-Hant-TW").expect("valid");
        assert_eq!(locale.language(), "zh");
        assert_eq!(locale.region(), Some("TW"), "the script must be skipped");

        let locale = Locale::parse("zh-Hant").expect("valid");
        assert_eq!(locale.region(), None);

        let locale = Locale::parse("es-419").expect("valid");
        assert_eq!(locale.region(), Some("419"));

        assert_eq!(Locale::EN.language(), "en");
        assert_eq!(Locale::EN.region(), None);
    }

    #[test]
    fn accept_language_picks_the_highest_quality_tag() {
        assert_eq!(
            Locale::from_accept_language("fr;q=0.8, en-GB;q=0.9, de;q=0.7"),
            Some(Locale::parse("en-GB").expect("valid"))
        );
        // No `q` means 1.0, which beats everything weighted.
        assert_eq!(
            Locale::from_accept_language("fr;q=0.9, en"),
            Some(Locale::parse("en").expect("valid"))
        );
        // Ties keep the client's own ordering.
        assert_eq!(
            Locale::from_accept_language("fr, en"),
            Some(Locale::parse("fr").expect("valid"))
        );
        // Whitespace and casing are the header's business, not ours.
        assert_eq!(
            Locale::from_accept_language("  DE-de ; Q=0.5 , zz-zz;q=0.4"),
            Some(Locale::parse("de-DE").expect("valid"))
        );
    }

    #[test]
    fn accept_language_skips_what_it_cannot_use() {
        // `q=0` means "not acceptable".
        assert_eq!(
            Locale::from_accept_language("fr;q=0, en;q=0.1"),
            Some(Locale::parse("en").expect("valid"))
        );
        // A malformed weight discards its entry rather than promoting it.
        assert_eq!(
            Locale::from_accept_language("fr;q=high, en;q=0.1"),
            Some(Locale::parse("en").expect("valid"))
        );
        assert_eq!(Locale::from_accept_language("*"), None);
        assert_eq!(Locale::from_accept_language(""), None);
        assert_eq!(Locale::from_accept_language(",,;;"), None);
        assert_eq!(Locale::from_accept_language("en_US"), None);
    }

    #[test]
    fn locale_round_trips_through_from_str() {
        let locale: Locale = "PT-br".parse().expect("valid");
        assert_eq!(locale.to_string(), "pt-BR");
        assert_eq!(locale.as_ref(), "pt-BR");
    }

    // ── messages ───────────────────────────────────────────────────────

    #[test]
    fn every_code_has_a_message() {
        for code in default_message_codes() {
            let message = default_message(code, &BTreeMap::new());
            assert!(!message.is_empty(), "{code} has no message");
            assert!(
                !message.contains("{"),
                "{code} left an uninterpolated placeholder: {message}"
            );
        }
    }

    #[test]
    fn length_messages_are_grammatical() {
        assert_eq!(
            default_message(codes::LEN, &params(&[("min", 1.into())])),
            "must not be empty"
        );
        assert_eq!(
            default_message(
                codes::LEN,
                &params(&[("min", 1.into()), ("unit", "items".into())])
            ),
            "must not be empty"
        );
        assert_eq!(
            default_message(codes::LEN, &params(&[("max", 1.into())])),
            "must be at most 1 character"
        );
        assert_eq!(
            default_message(
                codes::LEN,
                &params(&[("max", 1.into()), ("unit", "items".into())])
            ),
            "must be at most 1 item"
        );
        assert_eq!(
            default_message(codes::LEN, &params(&[("min", 8.into()), ("max", 8.into())])),
            "must be exactly 8 characters"
        );
        assert_eq!(
            default_message(codes::LEN, &params(&[("min", 2.into())])),
            "must be at least 2 characters"
        );
    }

    #[test]
    fn substring_messages_beat_the_raw_regex() {
        assert_eq!(
            default_message(
                codes::PATTERN,
                &params(&[
                    ("starts_with", "ORD-".into()),
                    ("pattern", r"^ORD\-".into())
                ])
            ),
            "must start with `ORD-`"
        );
        assert_eq!(
            default_message(codes::PATTERN, &params(&[("ends_with", ".png".into())])),
            "must end with `.png`"
        );
        assert_eq!(
            default_message(codes::PATTERN, &params(&[("contains", "@".into())])),
            "must contain `@`"
        );
    }

    #[test]
    fn format_messages_name_the_thing_not_the_keyword() {
        for (format, expected) in [
            ("email", "must be a valid email address"),
            ("uri", "must be a valid URL"),
            ("phone-e164", "must be a valid phone number in E.164 format"),
            ("date", "must be a valid date"),
            ("order-number", "must be a valid order-number"),
        ] {
            assert_eq!(
                default_message(codes::FORMAT, &params(&[("format", format.into())])),
                expected
            );
        }
    }

    #[test]
    fn range_and_enum_messages() {
        assert_eq!(
            default_message(
                codes::RANGE,
                &params(&[
                    ("min", 0.into()),
                    ("max", 1.into()),
                    ("exclusive_max", true.into())
                ])
            ),
            "must be at least 0 and less than 1"
        );
        assert_eq!(
            default_message(
                codes::ENUM,
                &params(&[("allowed", Value::Array(vec!["a".into(), "b".into()]))])
            ),
            "must be one of: a, b"
        );
        assert_eq!(
            default_message(codes::MULTIPLE_OF, &params(&[("multiple_of", 5.into())])),
            "must be a multiple of 5"
        );
        assert_eq!(
            default_message(codes::UNIQUE, &BTreeMap::new()),
            "must not contain duplicate values"
        );
        assert_eq!(
            default_message(codes::TYPE, &params(&[("expected", "an integer".into())])),
            "must be an integer"
        );
        assert_eq!(
            default_message("custom:unknown_field", &BTreeMap::new()),
            "is not a field of this type"
        );
    }

    #[test]
    fn chained_provider_takes_the_first_answer() {
        struct Only(&'static str, &'static str);
        impl MessageProvider for Only {
            fn message(
                &self,
                code: &str,
                _params: &BTreeMap<&'static str, Value>,
                _locale: &Locale,
            ) -> Option<String> {
                (code == self.0).then(|| self.1.to_owned())
            }
        }

        let chain = ChainedMessages::new()
            .with(std::sync::Arc::new(Only(codes::LEN, "first")))
            .with(std::sync::Arc::new(Only(codes::LEN, "second")))
            .with(std::sync::Arc::new(DefaultMessages));

        assert_eq!(
            chain.message(codes::LEN, &BTreeMap::new(), &Locale::EN),
            Some("first".to_owned())
        );
        assert_eq!(
            chain.message(codes::REQUIRED, &BTreeMap::new(), &Locale::EN),
            Some("this field is required".to_owned()),
            "the terminal DefaultMessages must answer everything"
        );
        assert!(format!("{chain:?}").contains('3'));
    }

    #[test]
    fn default_messages_answers_every_code() {
        for code in default_message_codes() {
            assert!(
                DefaultMessages
                    .message(code, &BTreeMap::new(), &Locale::EN)
                    .is_some(),
                "{code} unanswered"
            );
        }
    }
}
