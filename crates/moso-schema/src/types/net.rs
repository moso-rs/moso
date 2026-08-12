//! Network types: [`Hostname`] and [`IpCidr`].

use std::borrow::Cow;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::json_schema::{SchemaGenerator, SchemaNode, SchemaRef, StringBuilder};
use crate::schema::Schema;
use crate::types::ConstraintError;
use crate::validate::{ErrorCode, Validate, ValidationCtx, ValidationErrors};

/// A DNS hostname, lowercased.
///
/// Enforces RFC 1123: labels of 1–63 characters from `[a-z0-9-]`, not starting
/// or ending with `-`, total length at most 253. Trailing dots are stripped.
/// Unicode domains must be punycoded by the caller — accepting them raw would
/// mean two spellings of the same host, which is a homograph problem waiting to
/// happen.
///
/// ```text
/// JSON Schema: { "type": "string", "format": "hostname", "maxLength": 253 }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hostname(String);

impl Hostname {
    /// The JSON Schema `format` this type emits.
    pub const FORMAT: &'static str = "hostname";

    /// RFC 1035 total length limit.
    pub const MAX_LENGTH: u64 = 253;

    /// RFC 1035 per-label length limit.
    pub const MAX_LABEL_LENGTH: usize = 63;

    /// Parse and lowercase a hostname.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `format` when a label is malformed, or
    /// `len` when the name is longer than [`Hostname::MAX_LENGTH`].
    pub fn new(value: impl Into<String>) -> Result<Self, ConstraintError> {
        let raw = value.into();
        // A trailing dot is the fully-qualified spelling of the same name.
        let trimmed = raw.trim().trim_end_matches('.');

        if trimmed.is_empty() {
            return Err(malformed("must not be empty"));
        }
        // Counted in characters, like the `maxLength` this type emits. A valid
        // hostname is ASCII, so the two agree for anything that gets past the
        // label checks below.
        let length = trimmed.chars().count() as u64;
        if length > Self::MAX_LENGTH {
            return Err(ConstraintError::new(
                ErrorCode::Len,
                format!(
                    "must be at most {} characters (got {length})",
                    Self::MAX_LENGTH
                ),
            )
            .with_param("min", 1)
            .with_param("max", Self::MAX_LENGTH)
            .with_param("unit", "characters"));
        }

        for label in trimmed.split('.') {
            check_label(label)?;
        }

        if trimmed.len() == raw.len() && !raw.bytes().any(|b| b.is_ascii_uppercase()) {
            return Ok(Self(raw));
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    /// Wrap a string without checking or lowercasing it.
    ///
    /// **Escape hatch.** Nothing verifies the RFC 1123 shape. A `Hostname` is
    /// frequently interpolated into a connection string or a `Host` header, so
    /// an unchecked one is a header-injection primitive; use it only where the
    /// value provably came from a validated source.
    #[must_use]
    pub fn new_unchecked(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The lowercased hostname.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the underlying `String`.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    /// The dot-separated labels, left to right.
    pub fn labels(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }
}

string_newtype!(Hostname);

/// A `format` failure naming what a hostname is allowed to contain.
fn malformed(detail: &str) -> ConstraintError {
    ConstraintError::format(
        Hostname::FORMAT,
        format!("must be a hostname such as `api.example.com` ({detail})"),
    )
}

/// RFC 1123 §2.1: a label is 1–63 characters of `[a-zA-Z0-9-]` that neither
/// starts nor ends with a hyphen.
fn check_label(label: &str) -> Result<(), ConstraintError> {
    if label.is_empty() {
        return Err(malformed("it has an empty part between dots"));
    }
    // Checked before the length, so a Unicode domain gets the message that
    // tells the caller what to do rather than one about counting: the caller
    // almost always has an IDN and does not know it needs punycode.
    if !label.is_ascii() {
        return Err(malformed(
            "non-ASCII names must be punycoded first, e.g. `xn--mnchen-3ya.de`",
        ));
    }
    // ASCII from here, so bytes and characters are the same count.
    if label.len() > Hostname::MAX_LABEL_LENGTH {
        return Err(malformed("one part is longer than 63 characters"));
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err(malformed("a part must not start or end with a hyphen"));
    }
    if !label
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(malformed("only letters, digits and hyphens are allowed"));
    }
    Ok(())
}

impl Schema for Hostname {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Hostname")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemaNode {
        StringBuilder::new()
            .format(Self::FORMAT)
            .min_length(1)
            .max_length(Self::MAX_LENGTH)
            .description("A DNS hostname.")
            .build()
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }

    const HAS_CONSTRAINTS: bool = true;
}

/// An IP network in CIDR notation: `10.0.0.0/8`, `2001:db8::/32`.
///
/// Built on [`std::net::IpAddr`] plus a prefix length rather than a dedicated
/// IP-network crate, which keeps this crate's dependency footprint to what a
/// model layer actually needs.
///
/// The stored address is the **network address**: host bits are masked off on
/// construction, so `10.0.0.5/8` and `10.0.0.0/8` are the same value and
/// comparing two `IpCidr`s means what you expect.
///
/// ```text
/// JSON Schema: { "type": "string", "format": "ip-cidr" }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IpCidr {
    address: IpAddr,
    prefix_len: u8,
}

impl IpCidr {
    /// The JSON Schema `format` this type emits.
    pub const FORMAT: &'static str = "ip-cidr";

    /// The widest prefix an IPv4 network can have.
    pub const MAX_PREFIX_V4: u8 = 32;

    /// The widest prefix an IPv6 network can have.
    pub const MAX_PREFIX_V6: u8 = 128;

    /// Build from an address and prefix length, masking host bits.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `range` when `prefix_len` exceeds 32 for
    /// IPv4 or 128 for IPv6.
    pub fn new(address: IpAddr, prefix_len: u8) -> Result<Self, ConstraintError> {
        let max = Self::max_prefix_for(&address);
        if prefix_len > max {
            let family = if address.is_ipv4() { "IPv4" } else { "IPv6" };
            return Err(ConstraintError::new(
                ErrorCode::Range,
                format!("an {family} prefix must be between 0 and {max} (got {prefix_len})"),
            )
            .with_param("min", 0)
            .with_param("max", u64::from(max)));
        }

        // Storing the masked address is what makes `10.0.0.5/8 == 10.0.0.0/8`.
        let address = match address {
            IpAddr::V4(v4) => IpAddr::V4(std::net::Ipv4Addr::from(mask_u32(
                u32::from(v4),
                prefix_len,
            ))),
            IpAddr::V6(v6) => IpAddr::V6(std::net::Ipv6Addr::from(mask_u128(
                u128::from(v6),
                prefix_len,
            ))),
        };

        Ok(Self {
            address,
            prefix_len,
        })
    }

    /// Build without checking or masking.
    ///
    /// **Escape hatch.** A prefix longer than the family allows, or an address
    /// with host bits still set, makes [`IpCidr::contains`] answer questions
    /// nobody asked. It exists for reconstructing a value that was already
    /// validated — from a database row written by this same type.
    #[must_use]
    pub const fn new_unchecked(address: IpAddr, prefix_len: u8) -> Self {
        Self {
            address,
            prefix_len,
        }
    }

    /// Parse `address/prefix`.
    ///
    /// A bare address is accepted and treated as a host route (`/32` or
    /// `/128`).
    ///
    /// # Errors
    /// [`ConstraintError`] with code `format` when the input does not parse,
    /// or `range` when the prefix is too long for the address family.
    pub fn parse(value: &str) -> Result<Self, ConstraintError> {
        let value = value.trim();
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (value, None),
        };

        let address: IpAddr = address.parse().map_err(|_| unparsable())?;
        let prefix_len = match prefix {
            // A bare address is a host route: exactly this one address.
            None => Self::max_prefix_for(&address),
            Some(p) => p.parse::<u8>().map_err(|_| unparsable())?,
        };

        Self::new(address, prefix_len)
    }

    /// The widest prefix `address`'s family allows.
    #[must_use]
    pub const fn max_prefix_for(address: &IpAddr) -> u8 {
        match address {
            IpAddr::V4(_) => Self::MAX_PREFIX_V4,
            IpAddr::V6(_) => Self::MAX_PREFIX_V6,
        }
    }

    /// The masked network address.
    #[must_use]
    pub fn network(&self) -> IpAddr {
        self.address
    }

    /// The prefix length in bits.
    #[must_use]
    pub fn prefix_len(&self) -> u8 {
        self.prefix_len
    }

    /// True when this is an IPv4 network.
    #[must_use]
    pub fn is_ipv4(&self) -> bool {
        self.address.is_ipv4()
    }

    /// True when this is an IPv6 network.
    #[must_use]
    pub fn is_ipv6(&self) -> bool {
        self.address.is_ipv6()
    }

    /// True when `address` falls inside this network.
    ///
    /// Always false across address families: an IPv4 address is not in an IPv6
    /// network, not even a v4-mapped one, because treating it as such is how
    /// allow-lists get bypassed.
    #[must_use]
    pub fn contains(&self, address: IpAddr) -> bool {
        match (self.address, address) {
            (IpAddr::V4(network), IpAddr::V4(candidate)) => {
                mask_u32(u32::from(candidate), self.prefix_len) == u32::from(network)
            }
            (IpAddr::V6(network), IpAddr::V6(candidate)) => {
                mask_u128(u128::from(candidate), self.prefix_len) == u128::from(network)
            }
            _ => false,
        }
    }
}

/// Keep the top `prefix_len` bits, zero the rest.
///
/// Written with a shift guard because `u32 << 32` is undefined in C and a
/// panic in debug Rust; `checked_shr` returns `None` for the whole-width case,
/// which is exactly the `/0` network.
fn mask_u32(value: u32, prefix_len: u8) -> u32 {
    match u32::MAX.checked_shl(u32::from(32 - prefix_len.min(32))) {
        Some(mask) => value & mask,
        None => 0,
    }
}

/// Keep the top `prefix_len` bits, zero the rest. See [`mask_u32`].
fn mask_u128(value: u128, prefix_len: u8) -> u128 {
    match u128::MAX.checked_shl(u32::from(128 - prefix_len.min(128))) {
        Some(mask) => value & mask,
        None => 0,
    }
}

/// The `format` failure shared by every unparsable CIDR string.
fn unparsable() -> ConstraintError {
    ConstraintError::format(
        IpCidr::FORMAT,
        "must be an IP network in CIDR notation, e.g. `10.0.0.0/8` or `2001:db8::/32`",
    )
}

impl fmt::Display for IpCidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix_len)
    }
}

impl FromStr for IpCidr {
    type Err = ConstraintError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for IpCidr {
    type Error = ConstraintError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl<'a> TryFrom<&'a str> for IpCidr {
    type Error = ConstraintError;

    fn try_from(s: &'a str) -> Result<Self, Self::Error> {
        Self::parse(s)
    }
}

impl Serialize for IpCidr {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for IpCidr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).map_err(ConstraintError::into_serde_error)
    }
}

impl Validate for IpCidr {
    fn validate(&self, _ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        Ok(())
    }
}

impl Schema for IpCidr {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("IpCidr")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemaNode {
        StringBuilder::new()
            .format(Self::FORMAT)
            .description("An IP network in CIDR notation, e.g. `10.0.0.0/8`.")
            .build()
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }

    const HAS_CONSTRAINTS: bool = true;
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use serde_json::json;

    use super::*;
    use crate::validate::codes;

    // ── Hostname ─────────────────────────────────────────────────────────

    #[test]
    fn hostname_accepts_rfc_1123_names() {
        for (input, expected) in [
            ("example.com", "example.com"),
            ("API.Example.COM", "api.example.com"),
            ("example.com.", "example.com"),
            ("  example.com  ", "example.com"),
            ("a", "a"),
            ("xn--mnchen-3ya.de", "xn--mnchen-3ya.de"),
            ("my-host.internal", "my-host.internal"),
            ("3com.example", "3com.example"),
            ("localhost", "localhost"),
        ] {
            let h = Hostname::new(input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
            assert_eq!(h.as_str(), expected);
        }
    }

    #[test]
    fn hostname_rejects_malformed_names_with_a_format_code() {
        for input in [
            "",
            ".",
            "..",
            "example..com",
            "-example.com",
            "example-.com",
            "exa mple.com",
            "example.com/path",
            "example_host.com",
            "münchen.de",
            "http://example.com",
        ] {
            let e = Hostname::new(input).expect_err(input);
            assert_eq!(e.code().as_str(), codes::FORMAT, "for {input:?}");
            assert_eq!(e.params().get("format"), Some(&json!("hostname")));
        }
        assert!(
            Hostname::new("münchen.de")
                .unwrap_err()
                .message()
                .contains("punycode"),
            "the message should say what to do about a Unicode domain"
        );
    }

    #[test]
    fn hostname_enforces_both_length_limits() {
        let long_label = "a".repeat(64);
        let e = Hostname::new(format!("{long_label}.com")).expect_err("label too long");
        assert_eq!(e.code().as_str(), codes::FORMAT);
        assert!(Hostname::new(format!("{}.com", "a".repeat(63))).is_ok());

        let long_name = std::iter::repeat_n("abcdefghij", 26)
            .collect::<Vec<_>>()
            .join(".");
        assert!(long_name.len() as u64 > Hostname::MAX_LENGTH);
        let e = Hostname::new(long_name).expect_err("name too long");
        assert_eq!(e.code().as_str(), codes::LEN);
        assert_eq!(e.params().get("max"), Some(&json!(Hostname::MAX_LENGTH)));
    }

    #[test]
    fn hostname_exposes_its_labels() {
        let h = Hostname::new("api.example.com").unwrap();
        assert_eq!(h.labels().collect::<Vec<_>>(), ["api", "example", "com"]);
        assert_eq!(h.to_string(), "api.example.com");
        assert_eq!(&*h, "api.example.com");
    }

    #[test]
    fn hostname_deserialisation_enforces_the_invariant() {
        assert_eq!(
            serde_json::from_str::<Hostname>("\"API.example.com\"")
                .unwrap()
                .as_str(),
            "api.example.com"
        );
        let err = serde_json::from_str::<Hostname>("\"exa mple.com\"").unwrap_err();
        assert_eq!(
            crate::types::parse_serde_message(&err.to_string()).map(|(c, _)| c),
            Some(codes::FORMAT)
        );
    }

    #[test]
    fn hostname_json_schema_documents_what_is_enforced() {
        let node = Hostname::json_schema(&mut SchemaGenerator::default());
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            json!({
                "type": "string",
                "format": "hostname",
                "minLength": 1,
                "maxLength": 253,
                "description": "A DNS hostname.",
            })
        );
    }

    // ── IpCidr ───────────────────────────────────────────────────────────

    #[test]
    fn cidr_parses_both_families() {
        for (input, expected) in [
            ("10.0.0.0/8", "10.0.0.0/8"),
            ("192.168.1.1", "192.168.1.1/32"),
            ("0.0.0.0/0", "0.0.0.0/0"),
            ("255.255.255.255/32", "255.255.255.255/32"),
            ("2001:db8::/32", "2001:db8::/32"),
            ("::1", "::1/128"),
            ("::/0", "::/0"),
            (" 10.0.0.0/8 ", "10.0.0.0/8"),
        ] {
            let c = IpCidr::parse(input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
            assert_eq!(c.to_string(), expected);
        }
    }

    #[test]
    fn cidr_masks_host_bits_on_construction() {
        let a = IpCidr::parse("10.0.0.5/8").unwrap();
        let b = IpCidr::parse("10.255.255.255/8").unwrap();
        assert_eq!(a, b, "host bits must not affect identity");
        assert_eq!(a.network(), IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)));
        assert_eq!(a.prefix_len(), 8);
        assert!(a.is_ipv4() && !a.is_ipv6());

        let v6 = IpCidr::parse("2001:db8:1234::5/32").unwrap();
        assert_eq!(v6.to_string(), "2001:db8::/32");
        assert!(v6.is_ipv6());

        // Odd prefixes must mask mid-byte.
        assert_eq!(
            IpCidr::parse("10.1.2.3/12").unwrap().to_string(),
            "10.0.0.0/12"
        );
        assert_eq!(
            IpCidr::parse("10.17.2.3/12").unwrap().to_string(),
            "10.16.0.0/12"
        );
    }

    #[test]
    fn cidr_rejects_unparsable_input_with_a_format_code() {
        for input in [
            "",
            "nope",
            "10.0.0.0/",
            "/8",
            "10.0.0.0/x",
            "10.0.0.256/8",
            "10.0.0.0/8/8",
        ] {
            let e = IpCidr::parse(input).expect_err(input);
            assert_eq!(e.code().as_str(), codes::FORMAT, "for {input:?}");
            assert_eq!(e.params().get("format"), Some(&json!("ip-cidr")));
        }
    }

    #[test]
    fn cidr_rejects_an_over_long_prefix_with_a_range_code() {
        let e = IpCidr::parse("10.0.0.0/33").expect_err("33 bits of IPv4");
        assert_eq!(e.code().as_str(), codes::RANGE);
        assert_eq!(e.params().get("max"), Some(&json!(32)));
        assert!(e.message().contains("32"), "{}", e.message());

        let e = IpCidr::parse("2001:db8::/129").expect_err("129 bits of IPv6");
        assert_eq!(e.code().as_str(), codes::RANGE);
        assert_eq!(e.params().get("max"), Some(&json!(128)));

        assert!(IpCidr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 128).is_ok());
        assert!(IpCidr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 33).is_err());
    }

    #[test]
    fn cidr_membership_is_family_strict() {
        let net = IpCidr::parse("10.0.0.0/8").unwrap();
        assert!(net.contains(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(net.contains(IpAddr::V4(Ipv4Addr::new(10, 255, 255, 255))));
        assert!(!net.contains(IpAddr::V4(Ipv4Addr::new(11, 0, 0, 1))));
        assert!(!net.contains(IpAddr::V4(Ipv4Addr::new(9, 255, 255, 255))));

        // A v4-mapped v6 address is *not* in a v4 network: allow-lists that
        // pretend otherwise are how filters get bypassed.
        assert!(!net.contains(IpAddr::V6("::ffff:10.0.0.1".parse().unwrap())));

        let all_v4 = IpCidr::parse("0.0.0.0/0").unwrap();
        assert!(all_v4.contains(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))));
        assert!(!all_v4.contains(IpAddr::V6(Ipv6Addr::LOCALHOST)));

        let host = IpCidr::parse("192.168.1.5").unwrap();
        assert!(host.contains(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5))));
        assert!(!host.contains(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 6))));

        let v6 = IpCidr::parse("2001:db8::/32").unwrap();
        assert!(v6.contains("2001:db8:1::1".parse().unwrap()));
        assert!(!v6.contains("2001:db9::1".parse().unwrap()));
    }

    #[test]
    fn cidr_round_trips_through_json() {
        let c = IpCidr::parse("10.0.0.0/8").unwrap();
        assert_eq!(serde_json::to_value(c).unwrap(), json!("10.0.0.0/8"));
        assert_eq!(
            serde_json::from_str::<IpCidr>("\"10.0.0.5/8\"").unwrap(),
            c,
            "deserialisation masks too"
        );
        let err = serde_json::from_str::<IpCidr>("\"nope\"").unwrap_err();
        assert_eq!(
            crate::types::parse_serde_message(&err.to_string()).map(|(c, _)| c),
            Some(codes::FORMAT)
        );
        assert!("10.0.0.0/8".parse::<IpCidr>().is_ok());
        assert!(IpCidr::try_from(String::from("nope")).is_err());
    }

    #[test]
    fn cidr_json_schema_documents_what_is_enforced() {
        let node = IpCidr::json_schema(&mut SchemaGenerator::default());
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            json!({
                "type": "string",
                "format": "ip-cidr",
                "description": "An IP network in CIDR notation, e.g. `10.0.0.0/8`.",
            })
        );
    }
}
