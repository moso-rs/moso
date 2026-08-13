//! [`Password`] — a secret that resists being printed.
//!
//! The threat this type addresses is not cryptographic, it is *operational*: a
//! plaintext password reaching a log aggregator because someone added
//! `#[derive(Debug)]` to a struct three refactors later. The defence is to make
//! the ordinary ways of turning a value into text fail to compile.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::json_schema::{SchemaGenerator, SchemaNode, SchemaRef, StringBuilder};
use crate::schema::Schema;
use crate::types::ConstraintError;
use crate::validate::{Validate, ValidationCtx, ValidationErrors};

/// A plaintext password, in transit from a request to a hasher.
///
/// # What it deliberately does *not* implement
///
/// `Display`, `AsRef<str>` and `Deref<Target = str>` are all absent. Reading
/// the secret requires the explicit, greppable [`Password::expose`]. `Serialize`
/// exists — [`Schema`] requires it — but **always fails**, so a `Password` that
/// reaches a response body produces a loud serialisation error rather than a
/// quiet breach. `Debug` prints `Password(***)`.
///
/// The buffer is overwritten on drop. Without `unsafe` this is best-effort: the
/// bytes are zeroed through a `Vec<u8>` view of the same allocation and passed
/// through `black_box` to hinder elimination, but Rust cannot promise the value
/// was never copied elsewhere by the optimiser. Documented rather than
/// overclaimed.
///
/// ```text
/// JSON Schema: { "type": "string", "format": "password",
///                "writeOnly": true, "minLength": 12 }
/// ```
#[derive(Clone)]
pub struct Password(String);

impl Password {
    /// The JSON Schema `format` this type emits.
    pub const FORMAT: &'static str = "password";

    /// Default minimum length, in characters.
    ///
    /// Twelve, not eight: NIST SP 800-63B drops composition rules in favour of
    /// length, and eight has been inadequate for a decade.
    pub const MIN_LENGTH: usize = 12;

    /// Maximum accepted length, in characters.
    ///
    /// Bounded because bcrypt-family hashers are `O(n)` in the input and an
    /// unbounded password field is a denial-of-service vector.
    pub const MAX_LENGTH: usize = 256;

    /// Accept a password meeting the default length policy.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `len` when outside
    /// [`Password::MIN_LENGTH`]..=[`Password::MAX_LENGTH`].
    pub fn new(value: impl Into<String>) -> Result<Self, ConstraintError> {
        Self::with_min_length(value, Self::MIN_LENGTH)
    }

    /// Accept a password meeting an application-specific minimum length.
    ///
    /// The value is **not** trimmed, lowercased or otherwise normalised. A
    /// space is a character like any other, and silently changing a secret
    /// before hashing it means the same input sometimes fails to authenticate.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `len`, carrying `min` and `max` so the
    /// message names the actual policy.
    pub fn with_min_length(
        value: impl Into<String>,
        min_length: usize,
    ) -> Result<Self, ConstraintError> {
        let value = value.into();
        // Characters, not bytes: a 12-character passphrase in Japanese is 36
        // bytes and must not be rejected for it.
        let length = value.chars().count();

        // The error deliberately does not quote the value back.
        let out_of_range = length < min_length || length > Self::MAX_LENGTH;
        if out_of_range {
            let message = if length < min_length {
                format!("must be at least {min_length} characters")
            } else {
                format!("must be at most {} characters", Self::MAX_LENGTH)
            };
            return Err(
                ConstraintError::new(crate::validate::ErrorCode::Len, message)
                    .with_param("min", min_length as u64)
                    .with_param("max", Self::MAX_LENGTH as u64)
                    .with_param("unit", "characters"),
            );
        }

        Ok(Self(value))
    }

    /// Wrap a value without applying any policy.
    ///
    /// For values that are already known-good — a password read from a fixture,
    /// or one whose policy was applied elsewhere. Named to be conspicuous in
    /// review.
    #[must_use]
    pub fn from_trusted(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Read the secret. The only way to do so, and easy to grep for.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper and take the secret, skipping the zeroising `Drop`.
    #[must_use]
    pub fn expose_into_string(self) -> String {
        // `Drop` cannot be skipped for a type that implements it, so move the
        // buffer out and leave an empty string behind for `drop` to zero.
        let mut this = self;
        std::mem::take(&mut this.0)
    }

    /// Length in characters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.chars().count()
    }

    /// True when the secret is empty — only reachable via
    /// [`Password::from_trusted`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(***)")
    }
}

impl PartialEq for Password {
    /// Compares in time independent of *where* the first difference is.
    ///
    /// Lengths still leak, as they do in every practical implementation.
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.0.as_bytes(), other.0.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for (x, y) in a.iter().zip(b) {
            diff |= x ^ y;
        }
        std::hint::black_box(diff) == 0
    }
}

impl Eq for Password {}

impl Drop for Password {
    fn drop(&mut self) {
        // `String::into_bytes` reuses the same allocation, so zeroing the
        // `Vec<u8>` zeroes the buffer the secret lived in.
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
        std::hint::black_box(&bytes);
    }
}

impl FromStr for Password {
    type Err = ConstraintError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for Password {
    type Error = ConstraintError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl Serialize for Password {
    /// Always fails.
    ///
    /// A `Password` in a response body is a bug; failing the serialisation
    /// surfaces it in tests and in the error log rather than in an attacker's
    /// hands.
    fn serialize<S: Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom(
            "a `Password` must never be serialised; it is `writeOnly`",
        ))
    }
}

impl<'de> Deserialize<'de> for Password {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::new(raw).map_err(ConstraintError::into_serde_error)
    }
}

impl Validate for Password {
    fn validate(&self, _ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        Ok(())
    }
}

impl Schema for Password {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Password")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemaNode {
        StringBuilder::new()
            .format(Self::FORMAT)
            .min_length(Self::MIN_LENGTH as u64)
            .max_length(Self::MAX_LENGTH as u64)
            .write_only(true)
            .description("A plaintext password. Never returned in a response.")
            .build()
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }

    const HAS_CONSTRAINTS: bool = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_leaks_the_secret() {
        let p = Password::from_trusted("hunter2-hunter2");
        let rendered = format!("{p:?}");
        assert_eq!(rendered, "Password(***)");
        assert!(!rendered.contains("hunter2"));
    }

    #[test]
    fn serialisation_is_refused() {
        let p = Password::from_trusted("hunter2-hunter2");
        let err = serde_json::to_string(&p).unwrap_err();
        assert!(err.to_string().contains("never be serialised"));
    }

    /// The acceptance criterion from `docs/01-http/13-schema-validation.md`:
    /// format a *struct* that holds a password and grep the output.
    #[test]
    fn a_struct_holding_a_password_never_prints_it() {
        #[derive(Debug)]
        #[allow(
            dead_code,
            reason = "the fields exist to be formatted by `Debug` and grepped for; \
                      reading them is not what the test is about"
        )]
        struct Credentials {
            email: &'static str,
            password: Password,
            remember: bool,
        }

        let secret = "correct-horse-battery-staple";
        let creds = Credentials {
            email: "ada@example.com",
            password: Password::new(secret).unwrap(),
            remember: true,
        };

        for rendered in [format!("{creds:?}"), format!("{creds:#?}")] {
            assert!(!rendered.contains(secret), "the secret leaked: {rendered}");
            assert!(
                !rendered.contains("correct"),
                "a fragment leaked: {rendered}"
            );
            assert!(rendered.contains("Password(***)"), "got {rendered}");
            assert!(
                rendered.contains("ada@example.com"),
                "other fields must survive"
            );
        }
    }

    #[test]
    fn enforces_the_length_policy_with_a_len_code() {
        let e = Password::new("short").expect_err("below the minimum");
        assert_eq!(e.code().as_str(), crate::validate::codes::LEN);
        assert_eq!(
            e.params().get("min"),
            Some(&serde_json::json!(Password::MIN_LENGTH as u64))
        );
        assert_eq!(
            e.params().get("max"),
            Some(&serde_json::json!(Password::MAX_LENGTH as u64))
        );
        assert_eq!(e.message(), "must be at least 12 characters");
        assert!(
            !e.message().contains("short"),
            "the message must not echo the value"
        );

        let too_long = "x".repeat(Password::MAX_LENGTH + 1);
        let e = Password::new(too_long.as_str()).expect_err("above the maximum");
        assert_eq!(e.message(), "must be at most 256 characters");

        assert!(Password::new("x".repeat(Password::MIN_LENGTH)).is_ok());
        assert!(Password::new("x".repeat(Password::MAX_LENGTH)).is_ok());
    }

    #[test]
    fn the_minimum_is_a_policy_the_application_can_raise() {
        assert!(Password::with_min_length("eight888", 8).is_ok());
        assert!(Password::with_min_length("eight888", 20).is_err());
    }

    #[test]
    fn length_is_counted_in_characters_not_bytes() {
        // Twelve characters, thirty-six bytes.
        let japanese = "あいうえおかきくけこさし";
        assert_eq!(japanese.chars().count(), 12);
        assert_eq!(japanese.len(), 36);
        let p = Password::new(japanese).expect("twelve characters is twelve characters");
        assert_eq!(p.len(), 12);
        assert!(!p.is_empty());
    }

    #[test]
    fn deserialisation_enforces_the_policy_and_carries_the_code() {
        assert!(serde_json::from_str::<Password>("\"correct-horse-battery\"").is_ok());
        let err = serde_json::from_str::<Password>("\"short\"")
            .unwrap_err()
            .to_string();
        let (code, message) = crate::types::parse_serde_message(&err)
            .expect("the serde message must carry the constraint code");
        assert_eq!(code, crate::validate::codes::LEN);
        assert_eq!(message, "must be at least 12 characters");
    }

    #[test]
    fn the_secret_is_reachable_only_through_expose() {
        let p = Password::new("correct-horse-battery").unwrap();
        assert_eq!(p.expose(), "correct-horse-battery");
        assert_eq!(
            Password::from_trusted("x").expose_into_string(),
            String::from("x")
        );
    }

    #[test]
    fn json_schema_is_write_only_and_documents_the_policy() {
        let node = Password::json_schema(&mut SchemaGenerator::default());
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            serde_json::json!({
                "type": "string",
                "format": "password",
                "minLength": 12,
                "maxLength": 256,
                "writeOnly": true,
                "description": "A plaintext password. Never returned in a response.",
            })
        );
    }

    #[test]
    fn equality_is_value_based() {
        assert_eq!(
            Password::from_trusted("correct horse"),
            Password::from_trusted("correct horse")
        );
        assert_ne!(
            Password::from_trusted("correct horse"),
            Password::from_trusted("battery staple")
        );
        assert_ne!(
            Password::from_trusted("short"),
            Password::from_trusted("much longer value")
        );
    }
}
