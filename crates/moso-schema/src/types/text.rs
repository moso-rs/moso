//! Text types: [`Trimmed`], [`Sanitised`], [`PhoneE164`].

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::json_schema::{SchemaGenerator, SchemaNode, SchemaRef, StringBuilder};
use crate::schema::Schema;
use crate::types::ConstraintError;
use crate::validate::{Validate, ValidationCtx, ValidationErrors};

/// A string with no leading or trailing whitespace, guaranteed.
///
/// Trimming on deserialise rather than rejecting: a trailing space in a form
/// field is a user-interface artefact, not a user error. What it *does*
/// guarantee is that `"  "` and `""` are the same value, so a `NonEmpty`
/// wrapper around it means what it says.
///
/// Whitespace is Unicode whitespace, not just ASCII.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Trimmed(String);

impl Trimmed {
    /// Trim and wrap. Never fails.
    #[must_use]
    pub fn new_trimmed(value: impl AsRef<str>) -> Self {
        Self(value.as_ref().trim().to_owned())
    }

    /// Trim and wrap, for the shared newtype impls.
    ///
    /// # Errors
    /// Never; the `Result` exists so [`Trimmed`] can share the constrained
    /// string plumbing with types that do fail.
    pub fn new(value: impl Into<String>) -> Result<Self, ConstraintError> {
        let mut s = value.into();
        let trimmed = s.trim();
        if trimmed.len() != s.len() {
            s = trimmed.to_owned();
        }
        Ok(Self(s))
    }

    /// Wrap a string without trimming it.
    ///
    /// **Escape hatch.** The caller promises the value has no leading or
    /// trailing whitespace; nothing checks. For values that were trimmed
    /// upstream and would otherwise be scanned twice.
    #[must_use]
    pub fn new_unchecked(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The trimmed text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the underlying `String`.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

string_newtype!(Trimmed);

impl Schema for Trimmed {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Trimmed")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemaNode {
        StringBuilder::new()
            .description("Leading and trailing whitespace is removed.")
            .build()
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }
}

/// A telephone number in E.164 form: `+` followed by 2 to 15 digits.
///
/// Only the shape is checked. Whether the number is allocated, reachable, or
/// yours is a question for a verification SMS, not a deserialiser.
///
/// ```text
/// JSON Schema: { "type": "string", "format": "phone-e164",
///                "pattern": "^\\+[1-9]\\d{1,14}$" }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PhoneE164(String);

impl PhoneE164 {
    /// The JSON Schema `format` this type emits.
    pub const FORMAT: &'static str = "phone-e164";

    /// The pattern emitted into the schema and enforced on construction.
    pub const PATTERN: &'static str = r"^\+[1-9]\d{1,14}$";

    /// Parse an E.164 number.
    ///
    /// Spaces, hyphens, dots, parentheses and non-breaking spaces are stripped
    /// before checking, because every phone keypad in the world encourages
    /// them. A leading `00` is rewritten to `+`, which is what a European
    /// dialling out actually types.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `format`.
    pub fn new(value: impl Into<String>) -> Result<Self, ConstraintError> {
        let raw = value.into();
        let mut digits = String::with_capacity(raw.len());
        for c in raw.chars() {
            match c {
                ' ' | '-' | '.' | '(' | ')' | '/' | '\t' | '\u{00a0}' | '\u{202f}' => {}
                other => digits.push(other),
            }
        }

        // `0044…` is the same number as `+44…` to everyone but a parser.
        if let Some(rest) = digits.strip_prefix("00") {
            digits = format!("+{rest}");
        }

        if !is_e164(&digits) {
            return Err(ConstraintError::format(
                Self::FORMAT,
                "must be a telephone number in international form, e.g. `+441632960961`",
            )
            .with_param("pattern", Self::PATTERN));
        }

        Ok(Self(digits))
    }

    /// Wrap a string without checking or normalising it.
    ///
    /// **Escape hatch.** Nothing verifies the E.164 shape; a value that is not
    /// one will still claim to be through `Display` and the schema.
    #[must_use]
    pub fn new_unchecked(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The digits, without the leading `+`.
    ///
    /// Deliberately not a `country_code()`: E.164 country codes are one to
    /// three digits and are not self-delimiting, so splitting one out correctly
    /// needs the ITU-T assignment table — which changes, and which does not
    /// belong in a schema crate. Applications that need it should use a
    /// dedicated phone-number library on top of this type.
    #[must_use]
    pub fn digits(&self) -> &str {
        self.0.strip_prefix('+').unwrap_or(&self.0)
    }

    /// The normalised number, `+` and digits only.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the underlying `String`.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

string_newtype!(PhoneE164);

/// `^\+[1-9]\d{1,14}$`, hand-written.
///
/// A test asserts this agrees with [`PhoneE164::PATTERN`] compiled by `regex`,
/// so the enforced rule and the published one cannot drift.
fn is_e164(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('+') else {
        return false;
    };
    // `[1-9]` then 1..=14 more digits: 2..=15 in total, no leading zero.
    if !matches!(digits.len(), 2..=15) {
        return false;
    }
    let mut bytes = digits.bytes();
    match bytes.next() {
        Some(b'1'..=b'9') => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_digit())
}

impl Schema for PhoneE164 {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("PhoneE164")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemaNode {
        StringBuilder::new()
            .format(Self::FORMAT)
            .pattern(Self::PATTERN)
            .description("A telephone number in E.164 form.")
            .build()
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }

    const HAS_CONSTRAINTS: bool = true;
}

/// How a [`Sanitised`] string is cleaned.
///
/// Sanitisation runs on **deserialise**, so the value in memory is already
/// safe; there is no "remember to escape it" step left to forget.
///
/// [`StripTags`] and [`EscapeHtml`] are bundled. Write one when a field needs a
/// cleaning rule neither covers.
///
/// ```
/// use moso_schema::types::{Sanitised, SanitisePolicy, StripTags};
/// use std::borrow::Cow;
///
/// /// Collapses runs of whitespace to one space.
/// pub struct Squash;
///
/// impl SanitisePolicy for Squash {
///     const NAME: &'static str = "squash";
///     const FORMAT: Option<&'static str> = None;
///
///     fn sanitise(input: &str) -> Cow<'_, str> {
///         if input.split_whitespace().count() == input.split(' ').count() {
///             return Cow::Borrowed(input);
///         }
///         Cow::Owned(input.split_whitespace().collect::<Vec<_>>().join(" "))
///     }
/// }
///
/// // The value is clean the moment it is deserialised.
/// let squashed: Sanitised<Squash> = serde_json::from_str(r#""a   b""#).unwrap();
/// assert_eq!(squashed.as_str(), "a b");
///
/// let stripped: Sanitised<StripTags> = serde_json::from_str(r#""<b>hi</b>""#).unwrap();
/// assert_eq!(stripped.as_str(), "hi");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a sanitisation policy",
    label = "does not implement `SanitisePolicy`",
    note = "help: use a bundled policy — `moso::StripTags` or `moso::EscapeHtml` — or write one:\n    \
            impl moso::SanitisePolicy for {Self} {{\n        \
            const NAME: &'static str = \"my-policy\";\n        \
            const FORMAT: Option<&'static str> = None;\n        \
            fn sanitise(input: &str) -> std::borrow::Cow<'_, str> {{ input.into() }}\n    }}"
)]
pub trait SanitisePolicy: Send + Sync + 'static {
    /// Stable name, used in the schema description and in the generic schema
    /// name.
    const NAME: &'static str;

    /// JSON Schema `format` to emit, if the policy implies one.
    const FORMAT: Option<&'static str>;

    /// Clean `input`. Returning `Cow::Borrowed` when nothing changed avoids an
    /// allocation on the common path.
    fn sanitise(input: &str) -> Cow<'_, str>;
}

/// Removes every `<…>` tag, leaving the text content.
///
/// The conservative policy: it cannot produce markup because it produces no
/// markup at all. Use it for anything rendered as text.
///
/// A `<` only opens a tag when what follows could start one — an ASCII letter,
/// `/`, `!` or `?`, which is the HTML tokeniser's rule. `a < b` therefore
/// survives intact, while `<script>` does not. An unterminated tag consumes the
/// rest of the input: a truncated `<script src=…` must not be allowed to
/// reappear as text and be re-parsed as markup by something downstream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StripTags;

impl SanitisePolicy for StripTags {
    const NAME: &'static str = "strip-tags";
    const FORMAT: Option<&'static str> = None;

    fn sanitise(input: &str) -> Cow<'_, str> {
        fn opens_a_tag(rest: &str) -> bool {
            matches!(
                rest.as_bytes().first(),
                Some(b'/' | b'!' | b'?') | Some(b'a'..=b'z') | Some(b'A'..=b'Z')
            )
        }

        /// The byte index of the next `<` that begins a tag. `<` is ASCII, so
        /// the index is always a character boundary.
        fn next_tag_start(text: &str) -> Option<usize> {
            let mut from = 0;
            while let Some(offset) = text[from..].find('<') {
                let at = from + offset;
                if opens_a_tag(&text[at + 1..]) {
                    return Some(at);
                }
                from = at + 1;
            }
            None
        }

        // The overwhelmingly common case is text with no markup in it at all.
        let Some(first) = next_tag_start(input) else {
            return Cow::Borrowed(input);
        };

        let mut out = String::with_capacity(input.len());
        out.push_str(&input[..first]);
        // `rest` always starts at a `<` that opens a tag; when the matching `>`
        // is missing the tag is unterminated and the remainder is dropped.
        let mut rest = &input[first..];
        while let Some(end) = rest.find('>') {
            rest = &rest[end + 1..];
            match next_tag_start(rest) {
                Some(next) => {
                    out.push_str(&rest[..next]);
                    rest = &rest[next..];
                }
                None => {
                    out.push_str(rest);
                    break;
                }
            }
        }
        Cow::Owned(out)
    }
}

/// Escapes `& < > " '` into HTML entities.
///
/// Keeps the user's characters intact while making them inert in an HTML
/// context: `Tom & Jerry` stays readable, `<script>` becomes text.
///
/// This is escaping for *element content and quoted attribute values*, the two
/// contexts a template engine puts user data in. It is not sufficient for an
/// unquoted attribute, a `javascript:` URL, or inside a `<script>` block —
/// nothing character-level is, and a policy that claimed otherwise would be
/// worse than none.
///
/// Escaping the same string twice yields `&amp;lt;`, which is visible in the
/// output and is the intended failure mode: silently *not* escaping an
/// already-escaped `&` is how a filter turns into a bypass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EscapeHtml;

impl SanitisePolicy for EscapeHtml {
    const NAME: &'static str = "escape-html";
    const FORMAT: Option<&'static str> = None;

    fn sanitise(input: &str) -> Cow<'_, str> {
        fn entity(c: char) -> Option<&'static str> {
            match c {
                '&' => Some("&amp;"),
                '<' => Some("&lt;"),
                '>' => Some("&gt;"),
                '"' => Some("&quot;"),
                // `&apos;` is XML; the numeric form works in every HTML parser.
                '\'' => Some("&#x27;"),
                _ => None,
            }
        }

        // The common case is text with nothing to escape: return it borrowed.
        let Some(first) = input.find(['&', '<', '>', '"', '\'']) else {
            return Cow::Borrowed(input);
        };

        let mut out = String::with_capacity(input.len() + 16);
        out.push_str(&input[..first]);
        for c in input[first..].chars() {
            match entity(c) {
                Some(e) => out.push_str(e),
                None => out.push(c),
            }
        }
        Cow::Owned(out)
    }
}

/// A string cleaned by policy `P` at the boundary.
///
/// # What Moso does *not* ship
///
/// There is no allow-list HTML policy here. A correct one is a large, adversarial
/// piece of software — it is why `ammonia` exists — and shipping a half-built
/// one under a reassuring name would be worse than shipping none. Implement
/// [`SanitisePolicy`] over the sanitiser you have audited.
pub struct Sanitised<P: SanitisePolicy>(String, PhantomData<fn() -> P>);

impl<P: SanitisePolicy> Sanitised<P> {
    /// Sanitise and wrap. Never fails: the policy cleans rather than rejects.
    #[must_use]
    pub fn new(value: impl AsRef<str>) -> Self {
        Self(P::sanitise(value.as_ref()).into_owned(), PhantomData)
    }

    /// The cleaned text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the underlying `String`.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl<P: SanitisePolicy> Clone for Sanitised<P> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), PhantomData)
    }
}

impl<P: SanitisePolicy> fmt::Debug for Sanitised<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sanitised<{}>({:?})", P::NAME, self.0)
    }
}

impl<P: SanitisePolicy> fmt::Display for Sanitised<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<P: SanitisePolicy> PartialEq for Sanitised<P> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<P: SanitisePolicy> Eq for Sanitised<P> {}

impl<P: SanitisePolicy> AsRef<str> for Sanitised<P> {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<P: SanitisePolicy> std::ops::Deref for Sanitised<P> {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl<P: SanitisePolicy> FromStr for Sanitised<P> {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s))
    }
}

impl<P: SanitisePolicy> From<String> for Sanitised<P> {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl<P: SanitisePolicy> Serialize for Sanitised<P> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de, P: SanitisePolicy> Deserialize<'de> for Sanitised<P> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(Self::new(raw))
    }
}

impl<P: SanitisePolicy> Validate for Sanitised<P> {
    fn validate(&self, _ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        Ok(())
    }
}

impl<P: SanitisePolicy> Schema for Sanitised<P> {
    fn schema_name() -> Cow<'static, str> {
        crate::schema::generic_schema_name("Sanitised", &[Cow::Borrowed(P::NAME)])
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemaNode {
        let mut node = StringBuilder::new()
            .description(Cow::Owned(format!(
                "Sanitised on receipt with the `{}` policy.",
                P::NAME
            )))
            .build();
        node.format = P::FORMAT.map(Cow::Borrowed);
        node
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use regex::Regex;
    use serde_json::json;

    use super::*;
    use crate::validate::codes;

    #[test]
    fn trimmed_removes_surrounding_whitespace() {
        let t = Trimmed::new_trimmed("  hello \u{00a0}");
        assert_eq!(t.as_str(), "hello");
        assert_eq!(Trimmed::new("  x  ").unwrap().as_str(), "x");
        assert_eq!(Trimmed::new("x").unwrap().as_str(), "x");
        assert_eq!(
            serde_json::from_str::<Trimmed>("\"  x \"")
                .unwrap()
                .as_str(),
            "x",
            "deserialisation trims too"
        );
        assert_eq!(Trimmed::new_unchecked("  x  ").as_str(), "  x  ");
    }

    #[test]
    fn phone_accepts_e164_numbers() {
        for (input, expected) in [
            ("+441632960961", "+441632960961"),
            ("+44 1632 960961", "+441632960961"),
            ("+1 (555) 010-9999", "+15550109999"),
            ("+33.1.23.45.67.89", "+33123456789"),
            ("00441632960961", "+441632960961"),
            ("+12", "+12"),
        ] {
            let p = PhoneE164::new(input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
            assert_eq!(p.as_str(), expected);
        }
        assert_eq!(
            PhoneE164::new("+441632960961").unwrap().digits(),
            "441632960961"
        );
    }

    #[test]
    fn phone_rejects_everything_else_with_a_format_code() {
        for input in [
            "",
            "1632960961",        // no `+`
            "+0441632960961",    // leading zero after the `+`
            "+1",                // too short
            "+1234567890123456", // 16 digits
            "+44163296096a",
            "441632960961",
            "+",
            "+44-abc",
        ] {
            let e = PhoneE164::new(input).expect_err(input);
            assert_eq!(e.code().as_str(), codes::FORMAT, "for {input:?}");
            assert_eq!(e.params().get("format"), Some(&json!("phone-e164")));
            assert!(
                e.message().contains("+441632960961"),
                "no example in the message"
            );
        }
    }

    #[test]
    fn the_hand_written_phone_check_agrees_with_the_published_pattern() {
        let re = Regex::new(PhoneE164::PATTERN).expect("PATTERN must be a valid regex");
        let mut cases = vec![
            String::new(),
            "+".into(),
            "+1".into(),
            "+12".into(),
            "+01".into(),
            "12".into(),
            "+1a".into(),
            "+1 2".into(),
            "+١٢٣".into(), // Arabic-Indic digits are not `\d` in this pattern
            "+12\n".into(),
        ];
        for n in 1..=17 {
            cases.push(format!("+{}", "1".repeat(n)));
        }
        for case in &cases {
            assert_eq!(
                is_e164(case),
                re.is_match(case),
                "disagreement on {case:?}: hand-written says {}",
                is_e164(case)
            );
        }
    }

    #[test]
    fn phone_json_schema_documents_what_is_enforced() {
        let node = PhoneE164::json_schema(&mut SchemaGenerator::default());
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            json!({
                "type": "string",
                "format": "phone-e164",
                "pattern": r"^\+[1-9]\d{1,14}$",
                "description": "A telephone number in E.164 form.",
            })
        );
    }

    #[test]
    fn strip_tags_removes_markup_and_keeps_text() {
        for (input, expected) in [
            ("<script>alert(1)</script>", "alert(1)"),
            ("hello <b>world</b>", "hello world"),
            ("<p class=\"x\">a</p><p>b</p>", "ab"),
            ("<img src=x onerror=alert(1)>", ""),
            ("<!-- comment -->kept", "kept"),
            ("unterminated <script src=", "unterminated "),
        ] {
            assert_eq!(StripTags::sanitise(input), expected, "for {input:?}");
        }
    }

    #[test]
    fn strip_tags_leaves_ordinary_text_borrowed() {
        for input in ["plain text", "a < b and c > d", "5 < 6", "", "a<3"] {
            let out = StripTags::sanitise(input);
            assert!(
                matches!(out, Cow::Borrowed(_)),
                "{input:?} should not allocate, got {out:?}"
            );
            assert_eq!(out, input);
        }
    }

    #[test]
    fn escape_html_neutralises_the_five_characters() {
        assert_eq!(
            EscapeHtml::sanitise("<script>alert(\"x\")</script>"),
            "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;"
        );
        assert_eq!(EscapeHtml::sanitise("Tom & Jerry"), "Tom &amp; Jerry");
        assert_eq!(EscapeHtml::sanitise("it's"), "it&#x27;s");
        // Multi-byte characters either side of an escape must survive.
        assert_eq!(EscapeHtml::sanitise("é<é>é"), "é&lt;é&gt;é");
    }

    #[test]
    fn escape_html_leaves_ordinary_text_borrowed() {
        for input in ["plain text", "", "héllo wörld"] {
            assert!(matches!(EscapeHtml::sanitise(input), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn sanitised_cleans_on_deserialise() {
        let s: Sanitised<StripTags> = serde_json::from_str("\"<b>hi</b>\"").unwrap();
        assert_eq!(s.as_str(), "hi");
        assert_eq!(serde_json::to_value(&s).unwrap(), json!("hi"));

        let s: Sanitised<EscapeHtml> = serde_json::from_str("\"<b>hi</b>\"").unwrap();
        assert_eq!(s.as_str(), "&lt;b&gt;hi&lt;/b&gt;");
    }

    #[test]
    fn sanitised_names_its_policy() {
        assert_eq!(
            <Sanitised<StripTags> as Schema>::schema_name(),
            "Sanitised_strip-tags"
        );
        let node = <Sanitised<StripTags> as Schema>::json_schema(&mut SchemaGenerator::default());
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            json!({
                "type": "string",
                "description": "Sanitised on receipt with the `strip-tags` policy.",
            })
        );
    }
}
