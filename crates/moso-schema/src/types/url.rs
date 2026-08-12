//! [`Url`] — an absolute URL, parsed by the `url` crate.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::json_schema::{SchemaGenerator, SchemaNode, SchemaRef, StringBuilder};
use crate::schema::Schema;
use crate::types::ConstraintError;
use crate::validate::{Validate, ValidationCtx, ValidationErrors};

/// An absolute, parsed URL.
///
/// Wraps [`url::Url`] rather than a `String` so the parse happens once, at the
/// boundary, and every later `scheme()`/`host()` access is free. Relative
/// references are rejected: an API that accepts "maybe absolute, maybe not" is
/// an API with an SSRF bug in its future.
///
/// ```text
/// JSON Schema: { "type": "string", "format": "uri" }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Url(url::Url);

impl Url {
    /// The JSON Schema `format` this type emits.
    pub const FORMAT: &'static str = "uri";

    /// Parse an absolute URL.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `format` when the input is not an
    /// absolute URL.
    pub fn parse(value: &str) -> Result<Self, ConstraintError> {
        url::Url::parse(value.trim())
            .map(Self)
            .map_err(|e| unparsable(&e))
    }

    /// Parse an absolute URL restricted to one of `schemes`.
    ///
    /// Use this for anything that will later be fetched: allowing `file:` or
    /// `gopher:` because "it parsed" is how SSRF happens.
    ///
    /// Comparison is exact and case-insensitive — `url` lowercases the scheme
    /// on parse — so pass `&["http", "https"]`, not `&["HTTPS://"]`.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `format` when the URL does not parse or
    /// its scheme is not in `schemes`.
    pub fn parse_with_schemes(value: &str, schemes: &[&str]) -> Result<Self, ConstraintError> {
        let parsed = Self::parse(value)?;
        if !schemes
            .iter()
            .any(|s| s.eq_ignore_ascii_case(parsed.scheme()))
        {
            let allowed = schemes
                .iter()
                .map(|s| format!("`{s}:`"))
                .collect::<Vec<_>>()
                .join(" or ");
            return Err(ConstraintError::format(
                Self::FORMAT,
                format!("must be a {allowed} URL (got `{}:`)", parsed.scheme()),
            ));
        }
        Ok(parsed)
    }

    /// Parse an `http:` or `https:` URL.
    ///
    /// The common case of [`Url::parse_with_schemes`], spelled out because
    /// "which schemes did I allow?" should not be a question a reviewer has to
    /// answer from memory.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `format`.
    pub fn parse_http(value: &str) -> Result<Self, ConstraintError> {
        Self::parse_with_schemes(value, &["http", "https"])
    }

    /// The normalised, serialised form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Borrow the parsed URL.
    #[must_use]
    pub fn as_url(&self) -> &url::Url {
        &self.0
    }

    /// Consume into the parsed URL.
    #[must_use]
    pub fn into_url(self) -> url::Url {
        self.0
    }

    /// Consume into the serialised form.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0.into()
    }

    /// The scheme, e.g. `"https"`.
    #[must_use]
    pub fn scheme(&self) -> &str {
        self.0.scheme()
    }

    /// The host, if the scheme has one.
    #[must_use]
    pub fn host_str(&self) -> Option<&str> {
        self.0.host_str()
    }
}

/// Turn a `url` parse error into a message that names the problem.
///
/// The bundled messages are already good; the wrapper adds the shape the value
/// should have had, which is the part a caller staring at a 422 needs.
fn unparsable(error: &url::ParseError) -> ConstraintError {
    let detail = match error {
        url::ParseError::RelativeUrlWithoutBase => {
            "it must be absolute, starting with a scheme such as `https://`"
        }
        url::ParseError::EmptyHost => "the host is missing",
        url::ParseError::InvalidPort => "the port is not a number",
        url::ParseError::InvalidIpv4Address | url::ParseError::InvalidIpv6Address => {
            "the host is not a valid IP address"
        }
        url::ParseError::InvalidDomainCharacter => {
            "the host contains a character that is not \
                                                    allowed"
        }
        _ => "it is not a valid URL",
    };
    ConstraintError::format(
        Url::FORMAT,
        format!("must be an absolute URL such as `https://example.com/path` ({detail})"),
    )
}

impl From<url::Url> for Url {
    fn from(u: url::Url) -> Self {
        Self(u)
    }
}

impl From<Url> for url::Url {
    fn from(u: Url) -> Self {
        u.0
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for Url {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::ops::Deref for Url {
    type Target = url::Url;

    fn deref(&self) -> &url::Url {
        &self.0
    }
}

impl FromStr for Url {
    type Err = ConstraintError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Url {
    type Error = ConstraintError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl<'a> TryFrom<&'a str> for Url {
    type Error = ConstraintError;

    fn try_from(s: &'a str) -> Result<Self, Self::Error> {
        Self::parse(s)
    }
}

impl Serialize for Url {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Url {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).map_err(ConstraintError::into_serde_error)
    }
}

impl Validate for Url {
    fn validate(&self, _ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        Ok(())
    }
}

impl Schema for Url {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Url")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemaNode {
        StringBuilder::new()
            .format(Self::FORMAT)
            .description("An absolute URL.")
            .build()
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }

    const HAS_CONSTRAINTS: bool = true;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::validate::codes;

    #[test]
    fn accepts_absolute_urls_and_normalises_them() {
        for (input, expected) in [
            ("https://example.com", "https://example.com/"),
            (
                "https://example.com/a/b?q=1#f",
                "https://example.com/a/b?q=1#f",
            ),
            ("HTTPS://EXAMPLE.COM/Path", "https://example.com/Path"),
            ("  https://example.com/  ", "https://example.com/"),
            ("mailto:ada@example.com", "mailto:ada@example.com"),
            ("https://example.com:8443/", "https://example.com:8443/"),
        ] {
            let u = Url::parse(input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
            assert_eq!(u.as_str(), expected);
        }
    }

    #[test]
    fn rejects_relative_and_malformed_urls_with_a_format_code() {
        for input in [
            "",
            "/path",
            "example.com",
            "not a url",
            "https://",
            "http://[::1",
        ] {
            let e = Url::parse(input).expect_err(input);
            assert_eq!(e.code().as_str(), codes::FORMAT, "for {input:?}");
            assert_eq!(e.params().get("format"), Some(&json!("uri")));
            assert!(
                e.message().contains("https://example.com/path"),
                "the message should show the expected shape: {}",
                e.message()
            );
        }
        assert!(
            Url::parse("/path")
                .unwrap_err()
                .message()
                .contains("must be absolute"),
            "a relative URL deserves the specific message"
        );
    }

    #[test]
    fn scheme_restriction_is_the_ssrf_guard() {
        assert!(Url::parse_http("https://example.com").is_ok());
        assert!(Url::parse_http("http://example.com").is_ok());
        assert!(
            Url::parse("file:///etc/passwd").is_ok(),
            "parse alone allows it"
        );

        let e = Url::parse_http("file:///etc/passwd").expect_err("file: must be refused");
        assert_eq!(e.code().as_str(), codes::FORMAT);
        assert!(e.message().contains("`http:`"), "{}", e.message());
        assert!(e.message().contains("`file:`"), "{}", e.message());

        assert!(Url::parse_with_schemes("ftp://example.com", &["ftp"]).is_ok());
        assert!(Url::parse_with_schemes("HTTPS://example.com", &["https"]).is_ok());
        assert!(Url::parse_with_schemes("not a url", &["https"]).is_err());
    }

    #[test]
    fn exposes_the_parsed_components() {
        let u = Url::parse("https://api.example.com:8443/v1?x=1").unwrap();
        assert_eq!(u.scheme(), "https");
        assert_eq!(u.host_str(), Some("api.example.com"));
        assert_eq!(u.port(), Some(8443), "Deref reaches `url::Url`");
        assert_eq!(u.path(), "/v1");
        assert_eq!(u.to_string(), "https://api.example.com:8443/v1?x=1");
        assert_eq!(u.as_ref() as &str, "https://api.example.com:8443/v1?x=1");
        assert_eq!(
            u.clone().into_string(),
            "https://api.example.com:8443/v1?x=1"
        );
        assert_eq!(u.as_url().scheme(), "https");
        assert_eq!(url::Url::from(u).scheme(), "https");
    }

    #[test]
    fn round_trips_through_json() {
        let u = Url::parse("https://example.com/a").unwrap();
        assert_eq!(
            serde_json::to_value(&u).unwrap(),
            json!("https://example.com/a")
        );
        assert_eq!(
            serde_json::from_str::<Url>("\"https://example.com/a\"").unwrap(),
            u
        );
        let err = serde_json::from_str::<Url>("\"/relative\"")
            .unwrap_err()
            .to_string();
        let (code, message) = crate::types::parse_serde_message(&err)
            .expect("the serde message must carry the constraint code");
        assert_eq!(code, codes::FORMAT);
        assert!(message.contains("absolute"));
        assert!("https://example.com".parse::<Url>().is_ok());
        assert!(Url::try_from(String::from("nope")).is_err());
    }

    #[test]
    fn json_schema_documents_what_is_enforced() {
        let node = Url::json_schema(&mut SchemaGenerator::default());
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            json!({
                "type": "string",
                "format": "uri",
                "description": "An absolute URL.",
            })
        );
    }
}
