//! [`Email`] — an address that has passed RFC 5322-lite syntax plus domain
//! sanity checks.

use std::borrow::Cow;

use email_address::{EmailAddress, Error as EmailError, Options};

use crate::json_schema::{SchemaGenerator, SchemaNode, SchemaRef, StringBuilder};
use crate::schema::Schema;
use crate::types::ConstraintError;
use crate::validate::ErrorCode;

/// A syntactically valid email address, normalised.
///
/// Normalisation lowercases the domain (which is case-insensitive) and leaves
/// the local part alone (which is not, whatever most providers do in practice).
/// Surrounding whitespace is trimmed.
///
/// This validates *syntax*. It does not prove the mailbox exists — nothing
/// short of sending mail does, so Moso does not pretend otherwise with DNS
/// lookups that would make deserialisation do network I/O.
///
/// ```text
/// JSON Schema: { "type": "string", "format": "email",
///                "minLength": 3, "maxLength": 254 }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Email(String);

impl Email {
    /// The JSON Schema `format` this type emits.
    pub const FORMAT: &'static str = "email";

    /// A loose lower bound, emitted as `minLength`.
    ///
    /// Three characters is the shortest string that can even have the shape
    /// `local@domain`. The *real* gate is the syntax check in [`Email::new`],
    /// which additionally requires a dot in the domain; `minLength` is a
    /// necessary condition a client can check cheaply, not a sufficient one.
    pub const MIN_LENGTH: u64 = 3;

    /// The SMTP path limit from RFC 5321 §4.5.3.1.3.
    pub const MAX_LENGTH: u64 = 254;

    /// The RFC 5321 limit on the part before the `@`.
    pub const MAX_LOCAL_PART_LENGTH: usize = 64;

    /// Parse and normalise an address.
    ///
    /// Accepted: a `local@domain` pair where the local part is a dot-atom or a
    /// quoted string, and the domain is a dotted name whose labels are
    /// alphanumeric-delimited. Unicode is tolerated on both sides (RFC 6531),
    /// so `józef@kraków.pl` parses.
    ///
    /// Rejected on purpose, even though RFC 5322 permits them:
    ///
    /// * display names — `Ada <ada@example.com>` is a *header*, not an address;
    /// * domain literals — `ada@[192.0.2.1]` is never what an API meant;
    /// * dotless domains — `ada@localhost` has no TLD.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `format` when the address is not
    /// syntactically valid, or code `len` when it is longer than
    /// [`Email::MAX_LENGTH`] characters.
    pub fn new(value: impl Into<String>) -> Result<Self, ConstraintError> {
        let raw = value.into();
        let trimmed = raw.trim();

        let length = trimmed.chars().count() as u64;
        if length > Self::MAX_LENGTH {
            return Err(too_long(length));
        }
        if length < Self::MIN_LENGTH {
            return Err(ConstraintError::format(
                Self::FORMAT,
                "must be a valid email address (too short to be one)",
            ));
        }

        EmailAddress::parse_with_options(trimmed, parse_options()).map_err(describe)?;

        // The parser guarantees an `@` from here on.
        let at = trimmed
            .rfind('@')
            .expect("a parsed address contains an `@`");
        let (local, domain) = (&trimmed[..at], &trimmed[at + 1..]);

        if local.chars().count() > Self::MAX_LOCAL_PART_LENGTH {
            return Err(ConstraintError::format(
                Self::FORMAT,
                "must be a valid email address (the part before the `@` is too long)",
            ));
        }

        // The domain is case-insensitive (RFC 5321 §2.4); the local part is not,
        // whatever most providers do in practice, so it is left alone.
        if domain.chars().any(char::is_uppercase) {
            let mut normalised = String::with_capacity(trimmed.len());
            normalised.push_str(local);
            normalised.push('@');
            normalised.extend(domain.chars().flat_map(char::to_lowercase));
            return Ok(Self(normalised));
        }

        // Already normalised: reuse the caller's allocation when we can.
        if trimmed.len() == raw.len() {
            Ok(Self(raw))
        } else {
            Ok(Self(trimmed.to_owned()))
        }
    }

    /// Wrap a string without checking or normalising it.
    ///
    /// **Escape hatch.** Every other constructor guarantees the invariant; this
    /// one hands that guarantee to you. It exists for values that were
    /// validated elsewhere — read back from a column with a `CHECK`
    /// constraint, or produced by a test fixture — where re-parsing is
    /// measurable waste. Passing a non-address makes every later `Email` in
    /// the program a lie.
    #[must_use]
    pub fn new_unchecked(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The normalised address.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the underlying `String`.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    /// Everything before the final `@`.
    #[must_use]
    pub fn local_part(&self) -> &str {
        match self.0.rfind('@') {
            Some(at) => &self.0[..at],
            // Only reachable through `new_unchecked`.
            None => &self.0,
        }
    }

    /// Everything after the final `@`, lowercased.
    #[must_use]
    pub fn domain(&self) -> &str {
        match self.0.rfind('@') {
            Some(at) => &self.0[at + 1..],
            // Only reachable through `new_unchecked`.
            None => "",
        }
    }
}

/// The parser configuration [`Email`] documents.
fn parse_options() -> Options {
    Options::default()
        .with_required_tld()
        .without_domain_literal()
        .without_display_text()
}

/// A `len` failure carrying the bounds, so the message names them.
fn too_long(actual: u64) -> ConstraintError {
    ConstraintError::new(
        ErrorCode::Len,
        format!(
            "must be at most {} characters (got {actual})",
            Email::MAX_LENGTH
        ),
    )
    .with_param("min", Email::MIN_LENGTH)
    .with_param("max", Email::MAX_LENGTH)
    .with_param("unit", "characters")
}

/// Turn a parser error into a message that says what to fix.
///
/// The bundled `email_address` messages are accurate and unhelpful
/// ("SubDomainEmpty"); these name the thing the user typed.
fn describe(error: EmailError) -> ConstraintError {
    let detail = match error {
        EmailError::MissingSeparator => "it needs an `@`, as in `you@example.com`",
        EmailError::LocalPartEmpty => "there is nothing before the `@`",
        EmailError::LocalPartTooLong => "the part before the `@` is too long",
        EmailError::DomainEmpty => "there is nothing after the `@`",
        EmailError::DomainTooLong => "the domain is too long",
        EmailError::DomainTooFew => "the domain needs a dot, as in `example.com`",
        EmailError::SubDomainEmpty | EmailError::DomainInvalidSeparator => {
            "the domain has a misplaced dot"
        }
        EmailError::SubDomainTooLong => "one part of the domain is too long",
        EmailError::UnbalancedQuotes => "the quotes around the part before the `@` are unbalanced",
        EmailError::InvalidComment => "comments in parentheses are not accepted",
        EmailError::InvalidIPAddress | EmailError::UnsupportedDomainLiteral => {
            "an IP address in brackets is not accepted; use a domain name"
        }
        EmailError::UnsupportedDisplayName
        | EmailError::MissingDisplayName
        | EmailError::MissingEndBracket => "give the address on its own, without a name or `<>`",
        EmailError::InvalidCharacter => "it contains a character that is not allowed",
    };
    ConstraintError::format(
        Email::FORMAT,
        format!("must be a valid email address ({detail})"),
    )
}

string_newtype!(Email);

impl Schema for Email {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Email")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemaNode {
        StringBuilder::new()
            .format(Self::FORMAT)
            .min_length(Self::MIN_LENGTH)
            .max_length(Self::MAX_LENGTH)
            .description("An email address.")
            .build()
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }

    const HAS_CONSTRAINTS: bool = true;
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use serde_json::json;

    use super::*;
    use crate::validate::codes;

    #[track_caller]
    fn accepted(input: &str) -> Email {
        Email::new(input).unwrap_or_else(|e| panic!("{input:?} should be accepted: {e}"))
    }

    #[track_caller]
    fn rejected(input: &str) -> ConstraintError {
        match Email::new(input) {
            Ok(e) => panic!("{input:?} should be rejected, got {e}"),
            Err(e) => e,
        }
    }

    #[test]
    fn accepts_ordinary_addresses() {
        for input in [
            "ada@example.com",
            "ada.lovelace@example.co.uk",
            "ada+tagged@example.com",
            "a_b-c@sub.example.museum",
            "\"quoted local\"@example.com",
            "1@2.co",
        ] {
            assert_eq!(accepted(input).as_str(), input);
        }
    }

    #[test]
    fn tolerates_internationalised_addresses() {
        // RFC 6531: UTF-8 on both sides of the `@`.
        let e = accepted("józef@kraków.pl");
        assert_eq!(e.local_part(), "józef");
        assert_eq!(e.domain(), "kraków.pl");
    }

    #[test]
    fn rejects_malformed_addresses_with_a_format_code() {
        for input in [
            "ada",                      // no separator
            "@example.com",             // no local part
            "ada@",                     // no domain
            "ada@localhost",            // no dot in the domain
            "ada@example..com",         // empty label
            "ada @example.com",         // space
            "ada@exa mple.com",         // space in the domain
            "Ada <ada@example.com>",    // display name
            "ada@[192.0.2.1]",          // domain literal
            "ada@-example.com",         // label starts with a hyphen
            "ada@example.com.",         // trailing dot
            "ada\u{0}@example.com",     // control character
            "\"unbalanced@example.com", // unbalanced quotes
        ] {
            let e = rejected(input);
            assert_eq!(e.code().as_str(), codes::FORMAT, "for {input:?}");
            assert!(
                e.message().starts_with("must be a valid email address"),
                "unhelpful message for {input:?}: {}",
                e.message()
            );
            assert_eq!(e.params().get("format"), Some(&json!("email")));
        }
    }

    #[test]
    fn rejects_over_long_addresses_with_a_len_code() {
        let long = format!("{}@example.com", "a".repeat(250));
        let e = rejected(&long);
        assert_eq!(e.code().as_str(), codes::LEN);
        assert_eq!(e.params().get("max"), Some(&json!(Email::MAX_LENGTH)));
        assert!(e.message().contains("254"), "{}", e.message());

        // 64 is the RFC 5321 local-part limit, checked separately from the total.
        let long_local = format!("{}@example.com", "a".repeat(70));
        assert_eq!(rejected(&long_local).code().as_str(), codes::FORMAT);
    }

    #[test]
    fn normalises_surrounding_space_and_domain_case() {
        assert_eq!(accepted("  ada@example.com  ").as_str(), "ada@example.com");
        assert_eq!(accepted("Ada@EXAMPLE.COM").as_str(), "Ada@example.com");
        assert_eq!(
            accepted("Ada@EXAMPLE.COM"),
            accepted("Ada@example.com"),
            "the domain is case-insensitive"
        );
        assert_ne!(
            accepted("ada@example.com"),
            accepted("Ada@example.com"),
            "the local part is not"
        );
    }

    #[test]
    fn splits_at_the_final_at_sign() {
        let e = accepted("\"weird@local\"@example.com");
        assert_eq!(e.local_part(), "\"weird@local\"");
        assert_eq!(e.domain(), "example.com");
    }

    #[test]
    fn every_constructor_enforces_the_invariant() {
        assert!("nope".parse::<Email>().is_err());
        assert!(Email::from_str("ada@example.com").is_ok());
        assert!(Email::try_from(String::from("nope")).is_err());
        assert!(Email::try_from("ada@example.com").is_ok());
        assert!(serde_json::from_str::<Email>("\"nope\"").is_err());
        assert_eq!(
            serde_json::from_str::<Email>("\"ada@example.com\"").unwrap(),
            accepted("ada@example.com")
        );
    }

    #[test]
    fn deserialisation_failures_carry_the_code() {
        let err = serde_json::from_str::<Email>("\"nope\"")
            .unwrap_err()
            .to_string();
        let (code, message) = crate::types::parse_serde_message(&err)
            .expect("the serde message must carry the constraint code");
        assert_eq!(code, codes::FORMAT);
        assert!(message.starts_with("must be a valid email address"));
    }

    #[test]
    fn round_trips_through_json() {
        let e = accepted("ada@example.com");
        assert_eq!(serde_json::to_value(&e).unwrap(), json!("ada@example.com"));
        assert_eq!(e.to_string(), "ada@example.com");
        assert_eq!(e.as_ref() as &str, "ada@example.com");
        assert_eq!(&*e, "ada@example.com");
    }

    #[test]
    fn json_schema_documents_what_is_enforced() {
        let node = Email::json_schema(&mut SchemaGenerator::default());
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            json!({
                "type": "string",
                "format": "email",
                "minLength": 3,
                "maxLength": 254,
                "description": "An email address.",
            })
        );
        assert!(Email::schema_ref().as_node().is_some(), "must be inline");
    }

    #[test]
    fn unchecked_construction_is_the_documented_escape_hatch() {
        let e = Email::new_unchecked("not-an-email");
        assert_eq!(e.as_str(), "not-an-email");
        assert_eq!(e.local_part(), "not-an-email");
        assert_eq!(e.domain(), "");
    }
}
