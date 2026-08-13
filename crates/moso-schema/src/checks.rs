//! The `check_*` helpers that generated validation bodies call.
//!
//! Every function here takes `&mut ValidationErrors` — a *concrete* type — and
//! primitive arguments. That is deliberate: a generic `check<T: Constraint>`
//! would monomorphise once per call site, and a 20-field struct has 30 call
//! sites. These signatures make a generated `validate` body compile to
//! straight-line calls into already-compiled code.
//!
//! # Messages and localisation
//!
//! Each helper attaches the bundled English message and the constraint's
//! parameters. Translation happens afterwards, once, via
//! [`ValidationErrors::localise`] — so the hot path never touches a
//! [`MessageProvider`](crate::MessageProvider) and a request with no failures
//! pays nothing.
//!
//! # Length is counted in characters
//!
//! [`check_len_str`] counts Unicode code points (`str::chars`), not bytes. This
//! matches JSON Schema's definition of `minLength`/`maxLength`, so the check
//! and the emitted keyword agree exactly — which is the entire premise of the
//! crate. `"café"` is four characters and five bytes; a `len = ..=4` field
//! accepts it.
//!
//! # Pointers
//!
//! A helper never pushes onto the [`ValidationCtx`](crate::ValidationCtx)
//! pointer stack — it is handed the finished pointer for the value it is
//! checking. Generated code builds that with
//! [`ValidationCtx::field_pointer`](crate::ValidationCtx::field_pointer). The
//! two helpers that address a *sub*-value — [`check_unique`], which reports the
//! duplicate element, and [`check_each_nested`] — append the index themselves.

use std::collections::{BTreeMap, HashSet};
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use regex::Regex;
use serde_json::Value;

use crate::message::default_message;
use crate::validate::{FieldError, ValidationErrors, codes};

/// Whether a numeric bound is inclusive or exclusive.
///
/// Rust range syntax cannot express an exclusive *lower* bound, so
/// `exclusive_min` is only ever set by `#[schema(positive)]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bounds {
    /// `exclusiveMinimum` rather than `minimum`.
    pub exclusive_min: bool,
    /// `exclusiveMaximum` rather than `maximum` — what `1..10` means.
    pub exclusive_max: bool,
}

impl Bounds {
    /// `min ..= max`.
    pub const INCLUSIVE: Bounds = Bounds {
        exclusive_min: false,
        exclusive_max: false,
    };
    /// `min .. max`.
    pub const EXCLUSIVE_MAX: Bounds = Bounds {
        exclusive_min: false,
        exclusive_max: true,
    };
    /// `#[schema(positive)]`: `exclusiveMinimum: 0`.
    pub const EXCLUSIVE_MIN: Bounds = Bounds {
        exclusive_min: true,
        exclusive_max: false,
    };
}

/// The `unit` parameter used for string length errors — "must be between 3 and
/// 32 **characters**".
pub const UNIT_CHARACTERS: &str = "characters";

/// The `unit` parameter used for collection length errors — "must be between 1
/// and 10 **items**".
pub const UNIT_ITEMS: &str = "items";

/// Build a [`FieldError`] with the bundled English message for `code`.
///
/// Exposed because `#[schema(check = …)]` functions and custom constrained
/// types want the same message rendering the built-in checks get.
#[must_use]
pub fn field_error(
    pointer: &str,
    code: &'static str,
    params: BTreeMap<&'static str, Value>,
) -> FieldError {
    let message = default_message(code, &params);
    FieldError {
        pointer: pointer.to_owned(),
        code: std::borrow::Cow::Borrowed(code),
        message,
        params,
    }
}

/// An empty parameter map — every helper that reports a parameterless code
/// goes through this so the intent reads at the call site.
fn no_params() -> BTreeMap<&'static str, Value> {
    BTreeMap::new()
}

/// `{pointer}/{index}`, for the helpers that address an element of the value
/// they were given.
///
/// Shares [`crate::validate::itoa`] with the pointer stack so an element
/// pointer costs one `String` rather than two.
fn element_pointer(pointer: &str, index: usize) -> String {
    let mut buf = String::with_capacity(pointer.len() + 4);
    buf.push_str(pointer);
    buf.push('/');
    buf.push_str(crate::validate::itoa(index).as_str());
    buf
}

/// The parameter map for a `len` failure.
fn len_params(
    min: Option<usize>,
    max: Option<usize>,
    unit: &'static str,
) -> BTreeMap<&'static str, Value> {
    let mut params = BTreeMap::new();
    if let Some(min) = min {
        params.insert("min", Value::from(min));
    }
    if let Some(max) = max {
        params.insert("max", Value::from(max));
    }
    // `characters` is the documented default, so only the other case is
    // worth the bytes on the wire.
    if unit != UNIT_CHARACTERS {
        params.insert("unit", Value::from(unit));
    }
    params
}

/// The parameter map for a `range` failure. The exclusivity flags are only
/// emitted when true and when the bound they qualify exists.
fn range_params(
    min: Option<Value>,
    max: Option<Value>,
    bounds: Bounds,
) -> BTreeMap<&'static str, Value> {
    let mut params = BTreeMap::new();
    if let Some(min) = min {
        params.insert("min", min);
        if bounds.exclusive_min {
            params.insert("exclusive_min", Value::Bool(true));
        }
    }
    if let Some(max) = max {
        params.insert("max", max);
        if bounds.exclusive_max {
            params.insert("exclusive_max", Value::Bool(true));
        }
    }
    params
}

/// `serde_json` cannot represent NaN or an infinity, so a non-finite bound is
/// dropped from the parameters rather than serialised as `null`. The bound is
/// still enforced; only its description is omitted.
fn finite(value: f64) -> Option<Value> {
    serde_json::Number::from_f64(value).map(Value::Number)
}

// ── strings ─────────────────────────────────────────────────────────────

/// `minLength`/`maxLength`, counted in characters.
///
/// Emits one error with `code: "len"` and whichever of `min`/`max` were set.
/// Both bounds are inclusive, matching JSON Schema and the `3..=32` the user
/// wrote.
pub fn check_len_str(
    value: &str,
    min: Option<usize>,
    max: Option<usize>,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    let len = value.chars().count();
    if min.is_some_and(|m| len < m) || max.is_some_and(|m| len > m) {
        errors.push(field_error(
            pointer,
            codes::LEN,
            len_params(min, max, UNIT_CHARACTERS),
        ));
    }
}

/// `minItems`/`maxItems` for any collection. Callers pass `value.len()`.
pub fn check_len_seq(
    len: usize,
    min: Option<usize>,
    max: Option<usize>,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    if min.is_some_and(|m| len < m) || max.is_some_and(|m| len > m) {
        errors.push(field_error(
            pointer,
            codes::LEN,
            len_params(min, max, UNIT_ITEMS),
        ));
    }
}

/// [`check_len_seq`] for a slice, so the caller does not have to spell
/// `.len()`.
///
/// Generic only in the sense that the *wrapper* is; it forwards to the
/// non-generic helper immediately, so nothing beyond a `len()` call is
/// duplicated per element type.
#[inline]
pub fn check_len_slice<T>(
    items: &[T],
    min: Option<usize>,
    max: Option<usize>,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    check_len_seq(items.len(), min, max, pointer, errors);
}

/// `#[schema(non_empty)]` on a string: at least one character.
pub fn check_non_empty_str(value: &str, pointer: &str, errors: &mut ValidationErrors) {
    if value.is_empty() {
        errors.push(field_error(
            pointer,
            codes::LEN,
            len_params(Some(1), None, UNIT_CHARACTERS),
        ));
    }
}

/// `#[schema(non_empty)]` on a collection: at least one element.
pub fn check_non_empty_seq(len: usize, pointer: &str, errors: &mut ValidationErrors) {
    if len == 0 {
        errors.push(field_error(
            pointer,
            codes::LEN,
            len_params(Some(1), None, UNIT_ITEMS),
        ));
    }
}

/// `pattern`.
///
/// `regex` is compiled once into a `OnceLock` by the generated code;
/// `pattern` is the source text, reported in `params` so a client can render
/// its own message.
pub fn check_pattern(
    value: &str,
    regex: &Regex,
    pattern: &'static str,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    if !regex.is_match(value) {
        let mut params = BTreeMap::new();
        params.insert("pattern", Value::from(pattern));
        errors.push(field_error(pointer, codes::PATTERN, params));
    }
}

/// `#[schema(contains = "…")]`.
///
/// Reports `code: "pattern"` with both the literal (`contains`) and the
/// equivalent regular expression (`pattern`), which is the keyword the JSON
/// Schema carries.
pub fn check_contains(
    value: &str,
    needle: &'static str,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    if !value.contains(needle) {
        errors.push(field_error(
            pointer,
            codes::PATTERN,
            substring_params("contains", needle, regex::escape(needle)),
        ));
    }
}

/// `#[schema(starts_with = "…")]`.
pub fn check_starts_with(
    value: &str,
    prefix: &'static str,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    if !value.starts_with(prefix) {
        errors.push(field_error(
            pointer,
            codes::PATTERN,
            substring_params("starts_with", prefix, format!("^{}", regex::escape(prefix))),
        ));
    }
}

/// `#[schema(ends_with = "…")]`.
pub fn check_ends_with(
    value: &str,
    suffix: &'static str,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    if !value.ends_with(suffix) {
        errors.push(field_error(
            pointer,
            codes::PATTERN,
            substring_params("ends_with", suffix, format!("{}$", regex::escape(suffix))),
        ));
    }
}

/// Parameters shared by the three substring checks.
fn substring_params(
    key: &'static str,
    literal: &'static str,
    pattern: String,
) -> BTreeMap<&'static str, Value> {
    let mut params = BTreeMap::new();
    params.insert(key, Value::from(literal));
    params.insert("pattern", Value::String(pattern));
    params
}

/// A named string `format`.
///
/// Unknown formats are annotations only and never fail, which is what JSON
/// Schema specifies; see [`is_valid_format`].
pub fn check_format(
    value: &str,
    format: &'static str,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    if is_valid_format(format, value) == Some(false) {
        let mut params = BTreeMap::new();
        params.insert("format", Value::from(format));
        errors.push(field_error(pointer, codes::FORMAT, params));
    }
}

/// Every `format` [`is_valid_format`] enforces, in documentation order.
///
/// A `format` outside this list is legal — JSON Schema treats unknown formats
/// as annotations — but Moso will not check it, which is why the derive warns
/// about names that are close to one of these.
pub const KNOWN_FORMATS: &[&str] = &[
    "email",
    "uri",
    "uuid",
    "hostname",
    "ipv4",
    "ipv6",
    "ip-cidr",
    "date-time",
    "date",
    "time",
    "duration",
    "slug",
    "phone-e164",
    "json-pointer",
    "regex",
    "password",
    "byte",
    "binary",
    "cursor",
];

/// Validate `value` against a built-in `format`.
///
/// Returns `None` for formats Moso does not know, in which case the format is
/// a documentation annotation and imposes no constraint.
///
/// Known formats: `email`, `uri`, `uuid`, `hostname`, `ipv4`, `ipv6`,
/// `ip-cidr`, `date-time`, `date`, `time`, `duration`, `slug`, `phone-e164`,
/// `json-pointer`, `regex`, `password`, `byte`, `binary`, `cursor`.
///
/// # Strictness
///
/// The checks follow the specifications the format names refer to rather than
/// what is merely common, because the `format` keyword ends up in the OpenAPI
/// document and a generated client will hold Moso to it:
///
/// * `uuid` is the hyphenated 36-character form only, not the 32-character
///   "simple" or URN forms `uuid::Uuid` also parses.
/// * `time` is RFC 3339 `full-time`, so the offset is **required**:
///   `09:00:00Z` passes, `09:00:00` does not. Use a plain string with a
///   `pattern` for a local wall-clock time.
/// * `date` is RFC 3339 `full-date` — `2024-01-05`, never `2024-1-5`.
/// * `duration` is the RFC 3339 appendix A grammar, which has no fractional
///   components: `PT1H30M` passes, `PT1.5H` does not.
/// * `hostname` is RFC 1123, so no trailing dot.
/// * `password` and `binary` are pure annotations and always pass — returning
///   `Some(true)` rather than `None` records that Moso knows the name.
#[must_use]
pub fn is_valid_format(format: &str, value: &str) -> Option<bool> {
    let valid = match format {
        "email" => email_address::EmailAddress::is_valid(value),
        // `url::Url::parse` requires a scheme, which is exactly the
        // absolute-URI requirement of the `uri` format.
        "uri" => url::Url::parse(value).is_ok(),
        "uuid" => value.len() == 36 && uuid::Uuid::parse_str(value).is_ok(),
        "hostname" => is_hostname(value),
        "ipv4" => value.parse::<Ipv4Addr>().is_ok(),
        "ipv6" => value.parse::<Ipv6Addr>().is_ok(),
        "ip-cidr" => is_ip_cidr(value),
        "date-time" => is_rfc3339_date_time(value),
        "date" => is_rfc3339_date(value),
        "time" => is_rfc3339_time(value),
        "duration" => is_iso8601_duration(value),
        "slug" => is_slug(value),
        "phone-e164" => is_phone_e164(value),
        "json-pointer" => is_json_pointer(value),
        "regex" => Regex::new(value).is_ok(),
        "byte" => is_base64_standard(value),
        "cursor" => is_base64url_unpadded(value),
        "password" | "binary" => true,
        _ => return None,
    };
    Some(valid)
}

/// RFC 1123 host name: dot-separated labels of ASCII alphanumerics and
/// hyphens, no label empty or hyphen-terminated, no trailing dot.
fn is_hostname(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 {
        return false;
    }
    value.split('.').all(is_hostname_label)
}

/// One label of a host name.
fn is_hostname_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

/// `address/prefix`, with the prefix within the address family's range.
///
/// Host bits are *not* required to be zero: `192.0.2.5/24` describes a host
/// inside a network and is the form firewall rules are written in.
fn is_ip_cidr(value: &str) -> bool {
    let Some((address, prefix)) = value.split_once('/') else {
        return false;
    };
    if prefix.is_empty()
        || !prefix.bytes().all(|b| b.is_ascii_digit())
        || (prefix.len() > 1 && prefix.starts_with('0'))
    {
        return false;
    }
    let Ok(prefix_len) = prefix.parse::<u16>() else {
        return false;
    };
    match address.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => prefix_len <= 32,
        Ok(IpAddr::V6(_)) => prefix_len <= 128,
        Err(_) => false,
    }
}

/// RFC 3339 `date-time`: a `full-date`, a `T` separator and a `full-time` whose
/// offset is mandatory. `time` owns the grammar, fractional seconds included.
fn is_rfc3339_date_time(value: &str) -> bool {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).is_ok()
}

/// RFC 3339 `full-date`: `YYYY-MM-DD`, zero-padded, a real calendar day.
fn is_rfc3339_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if !bytes
        .iter()
        .enumerate()
        .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        return false;
    }
    // `time` owns the leap-year and month-length rules.
    time::Date::parse(
        value,
        time::macros::format_description!("[year]-[month]-[day]"),
    )
    .is_ok()
}

/// RFC 3339 `full-time`: `partial-time` followed by a mandatory offset.
fn is_rfc3339_time(value: &str) -> bool {
    let (time, offset) = if let Some(rest) = value.strip_suffix(['Z', 'z']) {
        (rest, None)
    } else if let Some(position) = value.rfind(['+', '-']) {
        (&value[..position], Some(&value[position + 1..]))
    } else {
        return false;
    };
    if let Some(offset) = offset
        && !is_offset(offset)
    {
        return false;
    }
    is_partial_time(time)
}

/// `HH:MM` after the sign of a numeric UTC offset.
fn is_offset(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 5
        && bytes[2] == b':'
        && two_digits(&bytes[0..2]).is_some_and(|h| h < 24)
        && two_digits(&bytes[3..5]).is_some_and(|m| m < 60)
}

/// `HH:MM:SS` with an optional fractional part. Second 60 is accepted, because
/// RFC 3339 permits it for leap seconds.
fn is_partial_time(value: &str) -> bool {
    let (clock, fraction) = match value.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (value, None),
    };
    if let Some(fraction) = fraction
        && (fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    let bytes = clock.as_bytes();
    if bytes.len() != 8 || bytes[2] != b':' || bytes[5] != b':' {
        return false;
    }
    let (Some(hours), Some(minutes), Some(seconds)) = (
        two_digits(&bytes[0..2]),
        two_digits(&bytes[3..5]),
        two_digits(&bytes[6..8]),
    ) else {
        return false;
    };
    hours < 24 && minutes < 60 && seconds <= 60
}

/// Two ASCII digits as a number, or `None`.
fn two_digits(bytes: &[u8]) -> Option<u8> {
    match bytes {
        [tens, units] if tens.is_ascii_digit() && units.is_ascii_digit() => {
            Some((tens - b'0') * 10 + (units - b'0'))
        }
        _ => None,
    }
}

/// RFC 3339 appendix A `duration`: `P[nY][nM][nD][T[nH][nM][nS]]` or `PnW`.
fn is_iso8601_duration(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('P') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    let (date, time) = match rest.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (rest, None),
    };
    // `dur-week` is exclusive of every other component.
    if let Some(weeks) = date.strip_suffix('W') {
        return time.is_none() && !weeks.is_empty() && weeks.bytes().all(|b| b.is_ascii_digit());
    }
    let Some(date_groups) = ordered_units(date, b"YMD") else {
        return false;
    };
    match time {
        None => date_groups > 0,
        Some(time) => matches!(ordered_units(time, b"HMS"), Some(groups) if groups > 0),
    }
}

/// Consume `value` as `<digits><unit>` groups whose units appear in `units` in
/// order, each at most once. Returns how many groups were read, or `None` when
/// anything is malformed or left over.
fn ordered_units(value: &str, units: &[u8]) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut cursor = 0;
    let mut next_unit = 0;
    let mut groups = 0;
    while cursor < bytes.len() {
        let start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor == start || cursor == bytes.len() {
            return None;
        }
        let position = units[next_unit..]
            .iter()
            .position(|u| *u == bytes[cursor])?;
        next_unit += position + 1;
        cursor += 1;
        groups += 1;
    }
    Some(groups)
}

/// `^[a-z0-9]+(-[a-z0-9]+)*$`, without paying for a regex.
fn is_slug(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// `^\+[1-9]\d{1,14}$` — E.164 allows at most 15 digits including the country
/// code, and the first may not be zero.
fn is_phone_e164(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('+') else {
        return false;
    };
    let bytes = digits.as_bytes();
    (2..=15).contains(&bytes.len()) && bytes.iter().all(u8::is_ascii_digit) && bytes[0] != b'0'
}

/// RFC 6901: the empty string, or `/`-separated tokens in which `~` is always
/// followed by `0` or `1`.
fn is_json_pointer(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    if !value.starts_with('/') {
        return false;
    }
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
            return false;
        }
    }
    true
}

/// Padded standard base64 (RFC 4648 §4), the `byte` format's encoding.
fn is_base64_standard(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    if !value.len().is_multiple_of(4) {
        return false;
    }
    let body = value.trim_end_matches('=');
    if value.len() - body.len() > 2 {
        return false;
    }
    body.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// Unpadded base64url (RFC 4648 §5), the encoding
/// [`Cursor`](crate::types::Cursor) uses. A length of `4n + 1` cannot be the
/// encoding of any byte string.
fn is_base64url_unpadded(value: &str) -> bool {
    !value.is_empty()
        && value.len() % 4 != 1
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ── numbers ─────────────────────────────────────────────────────────────

/// `minimum`/`maximum` for signed integers.
pub fn check_range_i64(
    value: i64,
    min: Option<i64>,
    max: Option<i64>,
    bounds: Bounds,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    let below = min.is_some_and(|m| {
        if bounds.exclusive_min {
            value <= m
        } else {
            value < m
        }
    });
    let above = max.is_some_and(|m| {
        if bounds.exclusive_max {
            value >= m
        } else {
            value > m
        }
    });
    if below || above {
        errors.push(field_error(
            pointer,
            codes::RANGE,
            range_params(min.map(Value::from), max.map(Value::from), bounds),
        ));
    }
}

/// `minimum`/`maximum` for unsigned integers.
pub fn check_range_u64(
    value: u64,
    min: Option<u64>,
    max: Option<u64>,
    bounds: Bounds,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    let below = min.is_some_and(|m| {
        if bounds.exclusive_min {
            value <= m
        } else {
            value < m
        }
    });
    let above = max.is_some_and(|m| {
        if bounds.exclusive_max {
            value >= m
        } else {
            value > m
        }
    });
    if below || above {
        errors.push(field_error(
            pointer,
            codes::RANGE,
            range_params(min.map(Value::from), max.map(Value::from), bounds),
        ));
    }
}

/// `minimum`/`maximum` for floats.
///
/// A NaN value fails with `code: "range"` regardless of the bounds, because it
/// compares false against everything and silently passing it is worse.
pub fn check_range_f64(
    value: f64,
    min: Option<f64>,
    max: Option<f64>,
    bounds: Bounds,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    let out_of_range = if value.is_nan() {
        true
    } else {
        min.is_some_and(|m| {
            if bounds.exclusive_min {
                value <= m
            } else {
                value < m
            }
        }) || max.is_some_and(|m| {
            if bounds.exclusive_max {
                value >= m
            } else {
                value > m
            }
        })
    };
    if out_of_range {
        errors.push(field_error(
            pointer,
            codes::RANGE,
            range_params(min.and_then(finite), max.and_then(finite), bounds),
        ));
    }
}

/// `multipleOf` for integers.
///
/// A zero divisor is not a constraint anyone can satisfy, so it is treated as
/// no constraint at all rather than a panic.
pub fn check_multiple_of_i64(
    value: i64,
    divisor: i64,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    if divisor == 0 {
        return;
    }
    // `i64::MIN % -1` overflows even though the remainder is mathematically
    // zero, so the checked form's `None` means "exactly divisible".
    if value.checked_rem(divisor).unwrap_or(0) != 0 {
        errors.push(field_error(
            pointer,
            codes::MULTIPLE_OF,
            multiple_of_params(Value::from(divisor)),
        ));
    }
}

/// How many `f64::EPSILON`s of slack [`check_multiple_of_f64`] allows,
/// relative to the larger of the value and the divisor.
///
/// `0.3 / 0.1` is `2.9999999999999996`; a tolerance is not optional, and one
/// scaled to the operands is the only kind that behaves the same at `0.3` and
/// at `3.0e9`.
const MULTIPLE_OF_TOLERANCE_STEPS: f64 = 8.0;

/// `multipleOf` for floats. Uses a relative epsilon, because exact float
/// modulo is meaningless.
pub fn check_multiple_of_f64(
    value: f64,
    divisor: f64,
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    if !divisor.is_finite() || divisor == 0.0 {
        return;
    }
    let ok = if value.is_finite() {
        let nearest = (value / divisor).round();
        let remainder = (value - nearest * divisor).abs();
        let tolerance =
            f64::EPSILON * MULTIPLE_OF_TOLERANCE_STEPS * value.abs().max(divisor.abs()).max(1.0);
        remainder <= tolerance
    } else {
        // An infinity is not a multiple of anything.
        false
    };
    if !ok {
        errors.push(field_error(
            pointer,
            codes::MULTIPLE_OF,
            multiple_of_params(finite(divisor).unwrap_or(Value::Null)),
        ));
    }
}

/// The parameter map for a `multiple_of` failure.
fn multiple_of_params(divisor: Value) -> BTreeMap<&'static str, Value> {
    let mut params = BTreeMap::new();
    params.insert("multiple_of", divisor);
    params
}

// ── collections ─────────────────────────────────────────────────────────

/// `uniqueItems`.
///
/// The one generic helper: duplicate detection cannot be done without knowing
/// the element type. Monomorphisation is per *element type*, not per call site,
/// so the cost is bounded by the number of distinct collection types in the
/// application.
///
/// Reports the pointer of the *first duplicate element*
/// (`/tags/3`), not the collection, so a UI can highlight it.
pub fn check_unique<T: Eq + Hash>(items: &[T], pointer: &str, errors: &mut ValidationErrors) {
    if items.len() < 2 {
        return;
    }
    let mut seen = HashSet::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        if !seen.insert(item) {
            let at = element_pointer(pointer, index);
            errors.push(field_error(&at, codes::UNIQUE, no_params()));
            return;
        }
    }
}

/// `#[schema(enum_values = [...])]` on a plain string field.
pub fn check_one_of_str(
    value: &str,
    allowed: &'static [&'static str],
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    if allowed.contains(&value) {
        return;
    }
    errors.push(field_error(
        pointer,
        codes::ENUM,
        allowed_params(allowed.iter().map(|v| Value::from(*v))),
    ));
}

/// `#[schema(enum_values = [...])]` on an integer field.
pub fn check_one_of_i64(
    value: i64,
    allowed: &'static [i64],
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    if allowed.contains(&value) {
        return;
    }
    errors.push(field_error(
        pointer,
        codes::ENUM,
        allowed_params(allowed.iter().map(|v| Value::from(*v))),
    ));
}

/// The general form of [`check_one_of_str`], for a field whose permitted
/// values are neither strings nor integers.
///
/// Prefer the concrete helpers where they apply: this one monomorphises per
/// element type and serialises the permitted values to build its message.
pub fn check_one_of<T: PartialEq + serde::Serialize>(
    value: &T,
    allowed: &[T],
    pointer: &str,
    errors: &mut ValidationErrors,
) {
    if allowed.contains(value) {
        return;
    }
    errors.push(field_error(
        pointer,
        codes::ENUM,
        allowed_params(
            allowed
                .iter()
                .map(|v| serde_json::to_value(v).unwrap_or(Value::Null)),
        ),
    ));
}

/// The parameter map for an `enum` failure.
fn allowed_params(allowed: impl Iterator<Item = Value>) -> BTreeMap<&'static str, Value> {
    let mut params = BTreeMap::new();
    params.insert("allowed", Value::Array(allowed.collect()));
    params
}

/// A value that must be present.
///
/// Serde already rejects a missing required field, so this exists for the cases
/// serde cannot see: a field that is structurally `Option<T>` but required by a
/// cross-field rule.
pub fn check_required<T>(value: Option<&T>, pointer: &str, errors: &mut ValidationErrors) {
    if value.is_none() {
        errors.push(field_error(pointer, codes::REQUIRED, BTreeMap::new()));
    }
}

// ── composition ─────────────────────────────────────────────────────────

/// `#[schema(nested)]`: validate an inner value and lift its errors.
///
/// The inner value is validated as if it were the document root, then every
/// pointer is prefixed with `pointer`, which is what makes
/// `/address/postcode` fall out of an `Address` that only knows about
/// `/postcode`.
pub fn check_nested<T: crate::Validate + ?Sized>(
    value: &T,
    pointer: &str,
    ctx: &mut crate::ValidationCtx,
    errors: &mut ValidationErrors,
) {
    if let Err(inner) = value.validate(ctx) {
        errors.merge_prefixed(pointer, inner);
    }
}

/// `#[schema(each(nested))]`: validate every element, lifting pointers with the
/// element index.
///
/// Stops early once `errors` has reached the context's cap, so a 10 000-element
/// array of invalid values costs 50 validations rather than 10 000.
pub fn check_each_nested<'a, T: crate::Validate + 'a>(
    items: impl IntoIterator<Item = &'a T>,
    pointer: &str,
    ctx: &mut crate::ValidationCtx,
    errors: &mut ValidationErrors,
) {
    for (index, item) in items.into_iter().enumerate() {
        if ctx.is_full(errors) {
            break;
        }
        if let Err(inner) = item.validate(ctx) {
            errors.merge_prefixed(&element_pointer(pointer, index), inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::{ValidationCtx, codes};

    fn errors() -> ValidationErrors {
        ValidationErrors::new()
    }

    fn only(errors: &ValidationErrors) -> &FieldError {
        assert_eq!(errors.len(), 1, "expected exactly one error: {errors:?}");
        &errors.as_slice()[0]
    }

    fn param(errors: &ValidationErrors, key: &str) -> Value {
        only(errors).params.get(key).cloned().unwrap_or(Value::Null)
    }

    #[test]
    fn required_reports_missing_values() {
        let mut errs = ValidationErrors::new();
        check_required::<u8>(None, "/age", &mut errs);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs.as_slice()[0].code, codes::REQUIRED);
        assert_eq!(errs.as_slice()[0].pointer, "/age");

        let mut errs = ValidationErrors::new();
        check_required(Some(&1u8), "/age", &mut errs);
        assert!(errs.is_empty());
    }

    #[test]
    fn bounds_constants_are_distinct() {
        assert_ne!(Bounds::INCLUSIVE, Bounds::EXCLUSIVE_MAX);
        assert_ne!(Bounds::INCLUSIVE, Bounds::EXCLUSIVE_MIN);
        assert_eq!(Bounds::default(), Bounds::INCLUSIVE);
    }

    // ── strings ────────────────────────────────────────────────────────

    #[test]
    fn len_str_counts_characters_not_bytes() {
        // "café" is 4 characters and 5 bytes; a max of 4 must accept it.
        let mut errs = errors();
        check_len_str("café", Some(1), Some(4), "/name", &mut errs);
        assert!(errs.is_empty(), "{errs:?}");

        // "日本語" is 3 characters and 9 bytes.
        let mut errs = errors();
        check_len_str("日本語", Some(4), None, "/name", &mut errs);
        assert_eq!(only(&errs).code, codes::LEN);
        assert_eq!(param(&errs, "min"), Value::from(4));
    }

    #[test]
    fn len_str_bounds_are_inclusive_and_reported() {
        let mut errs = errors();
        check_len_str("ab", Some(3), Some(32), "/username", &mut errs);
        let e = only(&errs);
        assert_eq!(e.pointer, "/username");
        assert_eq!(e.code, codes::LEN);
        assert_eq!(e.message, "must be between 3 and 32 characters");
        assert_eq!(e.params.get("min"), Some(&Value::from(3)));
        assert_eq!(e.params.get("max"), Some(&Value::from(32)));
        assert_eq!(e.params.get("unit"), None, "characters is the default");

        for value in ["abc", "abcd"] {
            let mut errs = errors();
            check_len_str(value, Some(3), Some(4), "/u", &mut errs);
            assert!(errs.is_empty(), "{value} should be accepted");
        }
    }

    #[test]
    fn len_seq_reports_items_as_the_unit() {
        let mut errs = errors();
        check_len_seq(11, None, Some(10), "/tags", &mut errs);
        let e = only(&errs);
        assert_eq!(e.params.get("unit"), Some(&Value::from("items")));
        assert_eq!(e.message, "must be at most 10 items");
    }

    #[test]
    fn len_slice_forwards_to_len_seq() {
        let mut errs = errors();
        check_len_slice(&[1, 2, 3], Some(4), None, "/xs", &mut errs);
        assert_eq!(only(&errs).code, codes::LEN);

        let mut errs = errors();
        check_len_slice::<u8>(&[], None, None, "/xs", &mut errs);
        assert!(errs.is_empty(), "no bound means no constraint");
    }

    #[test]
    fn non_empty_helpers_report_a_minimum_of_one() {
        let mut errs = errors();
        check_non_empty_str("", "/name", &mut errs);
        assert_eq!(only(&errs).message, "must not be empty");

        let mut errs = errors();
        check_non_empty_str("a", "/name", &mut errs);
        assert!(errs.is_empty());

        let mut errs = errors();
        check_non_empty_seq(0, "/tags", &mut errs);
        assert_eq!(only(&errs).message, "must not be empty");
        assert_eq!(param(&errs, "min"), Value::from(1));
    }

    #[test]
    fn pattern_reports_the_source_text() {
        let regex = Regex::new("^[a-z0-9_]+$").expect("valid");
        let mut errs = errors();
        check_pattern("Bad Name", &regex, "^[a-z0-9_]+$", "/username", &mut errs);
        let e = only(&errs);
        assert_eq!(e.code, codes::PATTERN);
        assert_eq!(e.message, "must match ^[a-z0-9_]+$");
        assert_eq!(e.params.get("pattern"), Some(&Value::from("^[a-z0-9_]+$")));

        let mut errs = errors();
        check_pattern("ok_name", &regex, "^[a-z0-9_]+$", "/username", &mut errs);
        assert!(errs.is_empty());
    }

    #[test]
    fn substring_checks_carry_both_the_literal_and_a_regex() {
        let mut errs = errors();
        check_starts_with("XYZ-1", "ORD-", "/number", &mut errs);
        let e = only(&errs);
        assert_eq!(e.code, codes::PATTERN);
        assert_eq!(e.message, "must start with `ORD-`");
        assert_eq!(e.params.get("starts_with"), Some(&Value::from("ORD-")));
        assert_eq!(e.params.get("pattern"), Some(&Value::from(r"^ORD\-")));

        let mut errs = errors();
        check_ends_with("a.txt", ".png", "/file", &mut errs);
        assert_eq!(only(&errs).message, "must end with `.png`");
        assert_eq!(param(&errs, "pattern"), Value::from(r"\.png$"));

        let mut errs = errors();
        check_contains("hello", "@", "/handle", &mut errs);
        assert_eq!(only(&errs).message, "must contain `@`");

        let mut errs = errors();
        check_starts_with("ORD-1", "ORD-", "/number", &mut errs);
        check_ends_with("a.png", ".png", "/file", &mut errs);
        check_contains("a@b", "@", "/handle", &mut errs);
        assert!(errs.is_empty(), "{errs:?}");
    }

    // ── formats ────────────────────────────────────────────────────────

    #[test]
    fn every_known_format_is_dispatched() {
        for format in KNOWN_FORMATS {
            assert!(
                is_valid_format(format, "irrelevant").is_some(),
                "{format} is listed but not implemented"
            );
        }
        assert_eq!(is_valid_format("colour", "red"), None);
        assert_eq!(is_valid_format("", ""), None);
    }

    #[test]
    fn unknown_formats_never_fail() {
        let mut errs = errors();
        check_format("anything at all", "colour", "/c", &mut errs);
        assert!(errs.is_empty(), "an unknown format is an annotation");
    }

    #[test]
    fn check_format_reports_the_format_name() {
        let mut errs = errors();
        check_format("not-an-email", "email", "/email", &mut errs);
        let e = only(&errs);
        assert_eq!(e.code, codes::FORMAT);
        assert_eq!(e.message, "must be a valid email address");
        assert_eq!(e.params.get("format"), Some(&Value::from("email")));
    }

    #[track_caller]
    fn accepts(format: &str, values: &[&str]) {
        for value in values {
            assert_eq!(
                is_valid_format(format, value),
                Some(true),
                "{format} should accept {value:?}"
            );
        }
    }

    #[track_caller]
    fn rejects(format: &str, values: &[&str]) {
        for value in values {
            assert_eq!(
                is_valid_format(format, value),
                Some(false),
                "{format} should reject {value:?}"
            );
        }
    }

    #[test]
    fn format_email_and_uri() {
        accepts("email", &["a@b.com", "first.last+tag@sub.example.org"]);
        rejects("email", &["", "a@", "@b.com", "no-at-sign", "a b@c.com"]);

        accepts(
            "uri",
            &[
                "https://example.com/a?b=c#d",
                "mailto:a@b.com",
                "urn:isbn:1",
            ],
        );
        rejects("uri", &["", "/relative/path", "example.com"]);
    }

    #[test]
    fn format_uuid_is_the_hyphenated_form_only() {
        accepts("uuid", &["067e6162-3b6f-4ae2-a171-2470b63dff00"]);
        rejects(
            "uuid",
            &[
                "067e61623b6f4ae2a1712470b63dff00",
                "urn:uuid:067e6162-3b6f-4ae2-a171-2470b63dff00",
                "not-a-uuid",
                "",
            ],
        );
    }

    #[test]
    fn format_hostname_and_ips() {
        accepts("hostname", &["example.com", "a", "xn--bcher-kva.example"]);
        rejects(
            "hostname",
            &[
                "",
                "example.com.",
                "-bad.com",
                "bad-.com",
                "a..b",
                "a_b.com",
            ],
        );

        accepts("ipv4", &["192.0.2.1", "0.0.0.0"]);
        rejects("ipv4", &["192.0.2.256", "192.0.2", "::1", "010.0.0.1"]);

        accepts("ipv6", &["::1", "2001:db8::1"]);
        rejects("ipv6", &["192.0.2.1", "2001:db8:::1", ""]);

        accepts(
            "ip-cidr",
            &["192.0.2.0/24", "192.0.2.5/32", "2001:db8::/32"],
        );
        rejects(
            "ip-cidr",
            &[
                "192.0.2.0",
                "192.0.2.0/33",
                "2001:db8::/129",
                "192.0.2.0/",
                "192.0.2.0/024",
            ],
        );
    }

    #[test]
    fn format_dates_and_times() {
        accepts(
            "date-time",
            &["2024-01-05T09:30:00Z", "2024-01-05T09:30:00.123+01:00"],
        );
        rejects("date-time", &["2024-01-05", "not a date", ""]);

        accepts("date", &["2024-01-05", "2024-02-29"]);
        rejects(
            "date",
            &["2024-1-5", "2023-02-29", "2024-13-01", "20240105"],
        );

        accepts(
            "time",
            &["09:30:00Z", "09:30:00.500Z", "09:30:00+01:00", "23:59:60Z"],
        );
        rejects(
            "time",
            &[
                "09:30:00",
                "9:30:00Z",
                "24:00:00Z",
                "09:60:00Z",
                "09:30:00+1:00",
            ],
        );

        accepts("duration", &["P1Y", "P3Y6M4D", "PT1H30M", "P1DT2H", "P2W"]);
        rejects(
            "duration",
            &["", "P", "1Y", "PT", "P1S", "PT1.5H", "P1W1D", "P1M1Y"],
        );
    }

    #[test]
    fn format_slug_phone_and_pointer() {
        accepts("slug", &["a", "hello-world", "post-2024-01"]);
        rejects("slug", &["", "-a", "a-", "a--b", "Hello", "a_b"]);

        accepts("phone-e164", &["+14155552671", "+441632960961"]);
        rejects(
            "phone-e164",
            &["", "+", "+0123", "14155552671", "+1", "+1234567890123456"],
        );

        accepts("json-pointer", &["", "/a", "/a/0", "/a~0b", "/a~1b"]);
        rejects("json-pointer", &["a", "/a~", "/a~2b"]);
    }

    #[test]
    fn format_regex_and_encodings() {
        accepts("regex", &["^[a-z]+$", ".*"]);
        rejects("regex", &["[unclosed", "(?P<"]);

        accepts("byte", &["", "aGVsbG8=", "aGVsbG9=", "AAAA"]);
        rejects("byte", &["aGVsbG8", "!!!!", "a===", "aGVsbG8=="]);

        accepts("cursor", &["abc", "a-b_cd"]);
        rejects("cursor", &["", "a", "a+b", "ab=="]);

        // Annotation-only formats accept anything, but are still "known".
        accepts("password", &["", "hunter2"]);
        accepts("binary", &["\u{0}\u{1}"]);
    }

    // ── numbers ────────────────────────────────────────────────────────

    #[test]
    fn range_i64_honours_exclusivity() {
        let mut errs = errors();
        check_range_i64(0, Some(1), Some(100), Bounds::INCLUSIVE, "/n", &mut errs);
        let e = only(&errs);
        assert_eq!(e.code, codes::RANGE);
        assert_eq!(e.message, "must be between 1 and 100");
        assert_eq!(e.params.get("exclusive_min"), None);

        let mut errs = errors();
        check_range_i64(
            100,
            Some(1),
            Some(100),
            Bounds::EXCLUSIVE_MAX,
            "/n",
            &mut errs,
        );
        let e = only(&errs);
        assert_eq!(e.params.get("exclusive_max"), Some(&Value::Bool(true)));
        assert_eq!(e.message, "must be at least 1 and less than 100");

        let mut errs = errors();
        check_range_i64(0, Some(0), None, Bounds::EXCLUSIVE_MIN, "/n", &mut errs);
        assert_eq!(only(&errs).message, "must be greater than 0");

        // Inside the bounds, nothing is reported.
        let mut errs = errors();
        check_range_i64(1, Some(1), Some(100), Bounds::INCLUSIVE, "/n", &mut errs);
        check_range_i64(
            99,
            Some(1),
            Some(100),
            Bounds::EXCLUSIVE_MAX,
            "/n",
            &mut errs,
        );
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn range_u64_does_not_wrap() {
        let mut errs = errors();
        check_range_u64(0, Some(1), None, Bounds::INCLUSIVE, "/n", &mut errs);
        assert_eq!(only(&errs).code, codes::RANGE);

        let mut errs = errors();
        check_range_u64(
            u64::MAX,
            None,
            Some(u64::MAX),
            Bounds::INCLUSIVE,
            "/n",
            &mut errs,
        );
        assert!(errs.is_empty());
    }

    #[test]
    fn range_f64_rejects_nan_and_drops_infinite_bounds() {
        let mut errs = errors();
        check_range_f64(f64::NAN, None, None, Bounds::INCLUSIVE, "/x", &mut errs);
        assert_eq!(only(&errs).code, codes::RANGE, "NaN must never pass");

        let mut errs = errors();
        check_range_f64(
            2.0,
            Some(0.0),
            Some(1.0),
            Bounds::EXCLUSIVE_MAX,
            "/x",
            &mut errs,
        );
        assert_eq!(only(&errs).params.get("max"), Some(&Value::from(1.0)));

        let mut errs = errors();
        check_range_f64(
            -1.0,
            Some(f64::INFINITY),
            None,
            Bounds::INCLUSIVE,
            "/x",
            &mut errs,
        );
        assert_eq!(only(&errs).code, codes::RANGE);
        assert_eq!(
            only(&errs).params.get("min"),
            None,
            "an infinite bound cannot be serialised, so it is omitted"
        );
    }

    #[test]
    fn multiple_of_integers() {
        let mut errs = errors();
        check_multiple_of_i64(7, 5, "/n", &mut errs);
        let e = only(&errs);
        assert_eq!(e.code, codes::MULTIPLE_OF);
        assert_eq!(e.message, "must be a multiple of 5");

        let mut errs = errors();
        check_multiple_of_i64(10, 5, "/n", &mut errs);
        check_multiple_of_i64(-10, 5, "/n", &mut errs);
        check_multiple_of_i64(0, 5, "/n", &mut errs);
        check_multiple_of_i64(3, 0, "/n", &mut errs);
        check_multiple_of_i64(i64::MIN, -1, "/n", &mut errs);
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn multiple_of_floats_tolerate_representation_error() {
        let mut errs = errors();
        // 0.3 / 0.1 is 2.9999999999999996 in binary floating point.
        check_multiple_of_f64(0.3, 0.1, "/x", &mut errs);
        check_multiple_of_f64(1.5, 0.5, "/x", &mut errs);
        check_multiple_of_f64(3.0e9, 1.0e9, "/x", &mut errs);
        check_multiple_of_f64(1.0, 0.0, "/x", &mut errs);
        assert!(errs.is_empty(), "{errs:?}");

        let mut errs = errors();
        check_multiple_of_f64(0.35, 0.1, "/x", &mut errs);
        assert_eq!(only(&errs).code, codes::MULTIPLE_OF);

        let mut errs = errors();
        check_multiple_of_f64(f64::INFINITY, 0.1, "/x", &mut errs);
        assert_eq!(only(&errs).code, codes::MULTIPLE_OF);
    }

    // ── collections ────────────────────────────────────────────────────

    #[test]
    fn unique_points_at_the_first_duplicate() {
        let mut errs = errors();
        check_unique(&["a", "b", "c", "b"], "/tags", &mut errs);
        let e = only(&errs);
        assert_eq!(e.pointer, "/tags/3");
        assert_eq!(e.code, codes::UNIQUE);
        assert_eq!(e.message, "must not contain duplicate values");

        let mut errs = errors();
        check_unique(&["a", "b"], "/tags", &mut errs);
        check_unique::<u8>(&[], "/tags", &mut errs);
        check_unique(&[1], "/tags", &mut errs);
        assert!(errs.is_empty());
    }

    #[test]
    fn unique_uses_a_root_pointer_correctly() {
        let mut errs = errors();
        check_unique(&[1, 1], "", &mut errs);
        assert_eq!(only(&errs).pointer, "/1");
    }

    #[test]
    fn one_of_lists_the_permitted_values() {
        const ALLOWED: &[&str] = &["draft", "published"];
        let mut errs = errors();
        check_one_of_str("archived", ALLOWED, "/status", &mut errs);
        let e = only(&errs);
        assert_eq!(e.code, codes::ENUM);
        assert_eq!(e.message, "must be one of: draft, published");
        assert_eq!(
            e.params.get("allowed"),
            Some(&Value::Array(vec![
                Value::from("draft"),
                Value::from("published")
            ]))
        );

        let mut errs = errors();
        check_one_of_str("draft", ALLOWED, "/status", &mut errs);
        assert!(errs.is_empty());
    }

    #[test]
    fn one_of_covers_integers_and_arbitrary_values() {
        const SIZES: &[i64] = &[1, 2, 3];
        let mut errs = errors();
        check_one_of_i64(4, SIZES, "/size", &mut errs);
        assert_eq!(only(&errs).message, "must be one of: 1, 2, 3");

        let mut errs = errors();
        check_one_of(&4.5f64, &[1.5, 2.5], "/x", &mut errs);
        assert_eq!(only(&errs).code, codes::ENUM);

        let mut errs = errors();
        check_one_of(&1.5f64, &[1.5, 2.5], "/x", &mut errs);
        check_one_of_i64(2, SIZES, "/size", &mut errs);
        assert!(errs.is_empty());
    }

    // ── composition ────────────────────────────────────────────────────

    /// A value that fails at `/inner` whenever its flag is set.
    struct Fails(bool);

    impl crate::Validate for Fails {
        fn validate(&self, _ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
            if self.0 {
                return Err(ValidationErrors::one("/inner", codes::LEN, "too short"));
            }
            Ok(())
        }
    }

    #[test]
    fn nested_composes_pointers() {
        let mut ctx = ValidationCtx::new();
        let mut errs = errors();
        check_nested(&Fails(true), "/address", &mut ctx, &mut errs);
        assert_eq!(only(&errs).pointer, "/address/inner");

        let mut errs = errors();
        check_nested(&Fails(false), "/address", &mut ctx, &mut errs);
        assert!(errs.is_empty());
    }

    #[test]
    fn each_nested_indexes_pointers() {
        let mut ctx = ValidationCtx::new();
        let mut errs = errors();
        let items = [Fails(false), Fails(true), Fails(true)];
        check_each_nested(items.iter(), "/lines", &mut ctx, &mut errs);
        assert_eq!(errs.len(), 2);
        assert_eq!(errs.as_slice()[0].pointer, "/lines/1/inner");
        assert_eq!(errs.as_slice()[1].pointer, "/lines/2/inner");
    }

    #[test]
    fn each_nested_stops_at_the_cap() {
        let mut ctx = ValidationCtx::new().with_max_errors(3);
        let mut errs = ctx.errors();
        let items: Vec<Fails> = (0..500).map(|_| Fails(true)).collect();
        check_each_nested(items.iter(), "/lines", &mut ctx, &mut errs);
        assert_eq!(errs.len(), 3);
        // The cap is consulted *before* each element, so the walk stops at
        // index 2 rather than validating all 500 and discarding 497.
        assert_eq!(errs.as_slice()[2].pointer, "/lines/2/inner");
        assert_eq!(
            errs.dropped(),
            0,
            "nothing is validated only to be thrown away"
        );
    }

    #[test]
    fn element_pointers_render_large_indices() {
        assert_eq!(element_pointer("/tags", 0), "/tags/0");
        assert_eq!(element_pointer("/tags", 12_345), "/tags/12345");
        assert_eq!(
            element_pointer("/tags", usize::MAX),
            format!("/tags/{}", usize::MAX)
        );
    }
}
