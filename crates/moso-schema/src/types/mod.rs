//! Constrained types — parse, don't validate.
//!
//! Attributes are convenient; types are correct. A `#[schema(len = 3..=32)]`
//! attribute protects one field of one struct. An [`Email`] cannot be
//! constructed from a non-email *anywhere in the program*, which is a stronger
//! guarantee and one the type system enforces for free at every later call
//! site.
//!
//! Every type in this module:
//!
//! * enforces its invariant in `Deserialize`, `FromStr` and `TryFrom<String>`,
//!   producing a [`ConstraintError`] with a code from
//!   [`codes`](crate::codes);
//! * emits the matching JSON Schema keyword from its `Schema` impl, so the
//!   documented constraint is the enforced one;
//! * implements `Display`, `AsRef` and `Deref` where that is safe. [`Password`]
//!   deliberately implements none of them.
//!
//! # Anonymity
//!
//! These types are *inline* in the OpenAPI document — a `format`-annotated
//! string, not a `$ref` to a one-line component. That keeps
//! `components/schemas` full of the application's models rather than Moso's
//! primitives, and generated clients get `string` where they would otherwise
//! get an alias chain.

/// Generates the trait impls every constrained *string* newtype shares.
///
/// The type must provide three inherent items:
/// `new(impl Into<String>) -> Result<Self, ConstraintError>`,
/// `as_str(&self) -> &str` and `into_string(self) -> String`.
///
/// Deliberately not used by [`Password`], which must not be `Display`,
/// `Deref<Target = str>` or serialisable.
macro_rules! string_newtype {
    ($t:ident) => {
        impl ::core::fmt::Display for $t {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::core::convert::AsRef<str> for $t {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl ::core::ops::Deref for $t {
            type Target = str;

            fn deref(&self) -> &str {
                self.as_str()
            }
        }

        impl ::core::borrow::Borrow<str> for $t {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }

        impl ::core::str::FromStr for $t {
            type Err = $crate::types::ConstraintError;

            fn from_str(s: &str) -> ::core::result::Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl ::core::convert::TryFrom<String> for $t {
            type Error = $crate::types::ConstraintError;

            fn try_from(s: String) -> ::core::result::Result<Self, Self::Error> {
                Self::new(s)
            }
        }

        impl<'a> ::core::convert::TryFrom<&'a str> for $t {
            type Error = $crate::types::ConstraintError;

            fn try_from(s: &'a str) -> ::core::result::Result<Self, Self::Error> {
                Self::new(s)
            }
        }

        impl ::core::convert::From<$t> for String {
            fn from(v: $t) -> String {
                v.into_string()
            }
        }

        impl ::serde::Serialize for $t {
            fn serialize<S: ::serde::Serializer>(
                &self,
                s: S,
            ) -> ::core::result::Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $t {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                d: D,
            ) -> ::core::result::Result<Self, D::Error> {
                let raw = <String as ::serde::Deserialize>::deserialize(d)?;
                Self::new(raw).map_err(|e| e.into_serde_error::<D::Error>())
            }
        }

        impl $crate::validate::Validate for $t {
            fn validate(
                &self,
                _ctx: &mut $crate::validate::ValidationCtx,
            ) -> ::core::result::Result<(), $crate::validate::ValidationErrors> {
                // The invariant is established on construction; there is
                // nothing left to check.
                Ok(())
            }
        }
    };
}

mod bounded;
mod cursor;
mod email;
mod id;
mod net;
mod password;
mod slug;
mod text;
mod url;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

use crate::validate::{ErrorCode, FieldError, ValidationErrors};

pub use bounded::{Bounded, IntegerValue, Length, Measured, NonEmpty};
pub use cursor::Cursor;
pub use email::Email;
pub use id::{Id, IdMarker};
pub use net::{Hostname, IpCidr};
pub use password::Password;
pub use slug::Slug;
pub use text::{EscapeHtml, PhoneE164, SanitisePolicy, Sanitised, StripTags, Trimmed};
pub use url::Url;

/// Prefix used to smuggle a validation code through a `serde` error message.
///
/// `serde::de::Error` has no room for structured data, so a constrained type
/// rejecting a value inside `Deserialize` encodes its code into the message as
/// `moso.constraint:<code>:<message>`. `moso-core`'s JSON extractor calls
/// [`parse_serde_message`] to recover the code and produce a 422 with the right
/// `code` rather than a generic parse failure.
///
/// This is the one place Moso uses a string protocol internally; the
/// alternative is a custom `Deserializer` wrapper on every request, which costs
/// far more than it saves.
pub const SERDE_ERROR_PREFIX: &str = "moso.constraint:";

/// A value that failed a constrained type's invariant.
///
/// Carries the same triple as a [`FieldError`] minus the pointer, because a
/// constructor does not know where the value will live.
#[derive(Clone, Debug, PartialEq)]
pub struct ConstraintError {
    code: ErrorCode,
    message: Cow<'static, str>,
    params: BTreeMap<&'static str, Value>,
}

impl ConstraintError {
    /// A failure with the given code and message.
    pub fn new(code: ErrorCode, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            code,
            message: message.into(),
            params: BTreeMap::new(),
        }
    }

    /// A `format` failure — the common case for a constrained string.
    pub fn format(format: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(ErrorCode::Format, message).with_param("format", format)
    }

    /// Attach a constraint parameter.
    #[must_use]
    pub fn with_param(mut self, key: &'static str, value: impl Into<Value>) -> Self {
        self.params.insert(key, value.into());
        self
    }

    /// The machine-readable code.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// The human message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The constraint parameters.
    #[must_use]
    pub fn params(&self) -> &BTreeMap<&'static str, Value> {
        &self.params
    }

    /// Place this failure at `pointer`.
    #[must_use]
    pub fn into_field_error(self, pointer: impl Into<String>) -> FieldError {
        FieldError {
            pointer: pointer.into(),
            code: self.code.into(),
            message: self.message,
            params: self.params,
        }
    }

    /// Place this failure at `pointer` as a one-element error set.
    #[must_use]
    pub fn into_validation_errors(self, pointer: impl Into<String>) -> ValidationErrors {
        ValidationErrors::from(self.into_field_error(pointer))
    }

    /// Encode as a `serde` error message; see [`SERDE_ERROR_PREFIX`].
    #[must_use]
    pub fn to_serde_message(&self) -> String {
        format!(
            "{SERDE_ERROR_PREFIX}{}:{}",
            self.code.as_str(),
            self.message
        )
    }

    /// Raise this failure as a `serde` deserialisation error.
    pub fn into_serde_error<E: serde::de::Error>(self) -> E {
        E::custom(self.to_serde_message())
    }
}

impl fmt::Display for ConstraintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConstraintError {}

/// Recover `(code, message)` from a `serde` error message produced by
/// [`ConstraintError::to_serde_message`].
///
/// Returns `None` for any message that is not one of ours, which is how
/// `moso-core` distinguishes "this field violated a documented constraint"
/// from "this JSON was malformed".
#[must_use]
pub fn parse_serde_message(message: &str) -> Option<(&str, &str)> {
    let rest = message.strip_prefix(SERDE_ERROR_PREFIX)?;
    let (code, detail) = rest.split_once(':')?;
    Some((code, detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_messages_round_trip() {
        let e = ConstraintError::format("email", "must be a valid email address");
        let msg = e.to_serde_message();
        assert_eq!(
            parse_serde_message(&msg),
            Some(("format", "must be a valid email address"))
        );
    }

    #[test]
    fn foreign_serde_messages_are_ignored() {
        assert_eq!(parse_serde_message("invalid type: string"), None);
        assert_eq!(parse_serde_message("moso.constraint:no-colon-after"), None);
    }

    #[test]
    fn constraint_errors_become_field_errors() {
        let e = ConstraintError::format("email", "must be a valid email address")
            .into_field_error("/email");
        assert_eq!(e.pointer, "/email");
        assert_eq!(e.code, "format");
        assert_eq!(e.params.get("format"), Some(&Value::from("email")));
    }
}
