//! [`Cursor`] — the opaque token in a paginated response.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::json_schema::{SchemaGenerator, SchemaNode, SchemaRef, StringBuilder};
use crate::schema::Schema;
use crate::types::ConstraintError;
use crate::validate::{ErrorCode, Validate, ValidationCtx, ValidationErrors};

/// An opaque pagination token.
///
/// A cursor carries whatever the query needs to resume — usually a sort-key
/// tuple — encoded as base64url with no padding. Clients must treat it as
/// opaque; encoding it rather than exposing `?after_id=…` is what lets the
/// pagination key change without breaking every client.
///
/// # Scope
///
/// This type is the *carrier*. Signing — which is what actually makes a cursor
/// tamper-proof — needs the application secret and therefore lives in
/// `moso-core`, which wraps [`Cursor::from_bytes`] with a MAC. A `Cursor`
/// built here is not authenticated and must not be trusted as one.
///
/// ```text
/// JSON Schema: { "type": "string", "format": "cursor",
///                "contentEncoding": "base64url" }
/// ```
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cursor(Vec<u8>);

impl Cursor {
    /// The JSON Schema `format` this type emits.
    pub const FORMAT: &'static str = "cursor";

    /// Refuse to decode anything larger than this. A cursor is a sort key, not
    /// a payload; an unbounded one is a memory-amplification vector.
    pub const MAX_ENCODED_LENGTH: usize = 2048;

    /// Wrap raw bytes.
    #[must_use]
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// Borrow the raw bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consume into the raw bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Number of raw bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when the cursor carries nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Encode as unpadded base64url (RFC 4648 §5).
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(self.0.len().div_ceil(3) * 4);
        for chunk in self.0.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let bits = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            // 3 bytes → 4 characters; a short final chunk emits 2 or 3.
            let characters = chunk.len() + 1;
            for i in 0..characters {
                let index = (bits >> (18 - 6 * i)) & 0x3f;
                out.push(char::from(ALPHABET[index as usize]));
            }
        }
        out
    }

    /// Decode unpadded base64url.
    ///
    /// Padding is tolerated on input and never produced on output, because
    /// some clients round-trip through a library that adds it. What is *not*
    /// tolerated is a non-canonical encoding — a final character whose unused
    /// low bits are non-zero. Two spellings of the same bytes would let a
    /// signed cursor be mutated without invalidating its signature, so the
    /// stricter reading is the safe one.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `format` for invalid base64url, or `len`
    /// when the input exceeds [`Cursor::MAX_ENCODED_LENGTH`].
    pub fn decode(encoded: &str) -> Result<Self, ConstraintError> {
        // Length first, before allocating anything sized by the input.
        if encoded.len() > Self::MAX_ENCODED_LENGTH {
            return Err(ConstraintError::new(
                ErrorCode::Len,
                format!(
                    "must be at most {} characters (got {})",
                    Self::MAX_ENCODED_LENGTH,
                    encoded.len()
                ),
            )
            .with_param("max", Self::MAX_ENCODED_LENGTH as u64)
            .with_param("unit", "characters"));
        }

        let body = encoded.trim_end_matches('=');
        // 4 characters carry 3 bytes; 2 carry 1 and 3 carry 2. A remainder of
        // one character carries nothing and cannot be produced by `encode`.
        if body.len() % 4 == 1 {
            return Err(malformed());
        }

        let mut out = Vec::with_capacity(body.len() / 4 * 3);
        // Holds only `bits_held` bits — masked after every byte, so it cannot
        // overflow however long the input is.
        let mut accumulator: u32 = 0;
        let mut bits_held: u32 = 0;
        for byte in body.bytes() {
            let value = decode_byte(byte).ok_or_else(malformed)?;
            accumulator = (accumulator << 6) | u32::from(value);
            bits_held += 6;
            if bits_held >= 8 {
                bits_held -= 8;
                out.push(((accumulator >> bits_held) & 0xff) as u8);
                accumulator &= (1u32 << bits_held) - 1;
            }
        }

        // 0, 2 or 4 leftover bits, all of which must be zero for this to be
        // the encoding `encode` would have produced.
        if accumulator != 0 {
            return Err(malformed());
        }

        Ok(Self(out))
    }
}

/// The base64url alphabet (RFC 4648 §5): `-` and `_` in place of `+` and `/`,
/// so a cursor survives a URL without percent-encoding.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// One base64url character → its six-bit value.
const fn decode_byte(byte: u8) -> Option<u8> {
    Some(match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'-' => 62,
        b'_' => 63,
        _ => return None,
    })
}

/// The `format` failure shared by every unusable cursor.
fn malformed() -> ConstraintError {
    ConstraintError::format(
        Cursor::FORMAT,
        "must be a pagination cursor from a previous response, passed back unmodified",
    )
}

impl fmt::Debug for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The encoded form is what appears in URLs and logs, so print that
        // rather than a byte array nobody can correlate.
        write!(f, "Cursor({})", self.encode())
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl FromStr for Cursor {
    type Err = ConstraintError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::decode(s)
    }
}

impl TryFrom<String> for Cursor {
    type Error = ConstraintError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::decode(&s)
    }
}

impl<'a> TryFrom<&'a str> for Cursor {
    type Error = ConstraintError;

    fn try_from(s: &'a str) -> Result<Self, Self::Error> {
        Self::decode(s)
    }
}

impl Serialize for Cursor {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.encode())
    }
}

impl<'de> Deserialize<'de> for Cursor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::decode(&raw).map_err(ConstraintError::into_serde_error)
    }
}

impl Validate for Cursor {
    fn validate(&self, _ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        Ok(())
    }
}

impl Schema for Cursor {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Cursor")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemaNode {
        StringBuilder::new()
            .format(Self::FORMAT)
            .content_encoding("base64url")
            .max_length(Self::MAX_ENCODED_LENGTH as u64)
            .description("An opaque pagination token. Pass it back unmodified.")
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

    /// The RFC 4648 §10 test vectors, in the URL-safe alphabet.
    const VECTORS: &[(&[u8], &str)] = &[
        (b"", ""),
        (b"f", "Zg"),
        (b"fo", "Zm8"),
        (b"foo", "Zm9v"),
        (b"foob", "Zm9vYg"),
        (b"fooba", "Zm9vYmE"),
        (b"foobar", "Zm9vYmFy"),
    ];

    #[test]
    fn matches_the_rfc_4648_vectors_without_padding() {
        for (bytes, encoded) in VECTORS {
            let c = Cursor::from_bytes(*bytes);
            assert_eq!(c.encode(), *encoded, "encoding {bytes:?}");
            assert_eq!(
                Cursor::decode(encoded).unwrap().as_bytes(),
                *bytes,
                "decoding {encoded:?}"
            );
        }
    }

    #[test]
    fn uses_the_url_safe_alphabet() {
        // These bytes encode to `+` and `/` in the standard alphabet.
        let c = Cursor::from_bytes([0xfb, 0xff, 0xbf]);
        let encoded = c.encode();
        assert_eq!(encoded, "-_-_");
        assert!(!encoded.contains(['+', '/', '=']));
        assert_eq!(Cursor::decode(&encoded).unwrap(), c);
    }

    #[test]
    fn round_trips_arbitrary_bytes() {
        for len in 0..64usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(37)).collect();
            let c = Cursor::from_bytes(bytes.clone());
            let encoded = c.encode();
            assert!(!encoded.contains('='), "encode must not pad: {encoded}");
            assert_eq!(Cursor::decode(&encoded).unwrap().into_bytes(), bytes);
        }
    }

    #[test]
    fn tolerates_padding_on_input() {
        assert_eq!(Cursor::decode("Zg==").unwrap().as_bytes(), b"f");
        assert_eq!(Cursor::decode("Zm8=").unwrap().as_bytes(), b"fo");
        assert_eq!(Cursor::decode("Zm9v").unwrap().as_bytes(), b"foo");
    }

    #[test]
    fn rejects_malformed_input_with_a_format_code() {
        for input in [
            "Z",     // a lone character carries no byte
            "AAAAA", // five characters: one left over
            "Zm9v!", // not in the alphabet
            "Zm+v",  // standard-alphabet character
            "Zm/v", "Zm9 v", "Zh",     // non-canonical: the unused low bits are not zero
            "Zm9vYh", // ditto, at the two-byte boundary
        ] {
            let e = Cursor::decode(input).expect_err(input);
            assert_eq!(e.code().as_str(), codes::FORMAT, "for {input:?}");
            assert_eq!(e.params().get("format"), Some(&json!("cursor")));
        }
    }

    /// Non-canonical encodings are rejected so that a signed cursor cannot be
    /// mutated into a different spelling of the same bytes.
    #[test]
    fn rejects_non_canonical_encodings() {
        // `Zg` and `Zh` would both decode to `f` in a lenient decoder.
        assert_eq!(Cursor::decode("Zg").unwrap().as_bytes(), b"f");
        assert!(Cursor::decode("Zh").is_err());
        assert_eq!(Cursor::decode("Zm8").unwrap().as_bytes(), b"fo");
        assert!(Cursor::decode("Zm9").is_err());
    }

    #[test]
    fn rejects_over_long_input_with_a_len_code() {
        let long = "A".repeat(Cursor::MAX_ENCODED_LENGTH + 1);
        let e = Cursor::decode(&long).expect_err("too long");
        assert_eq!(e.code().as_str(), codes::LEN);
        assert_eq!(
            e.params().get("max"),
            Some(&json!(Cursor::MAX_ENCODED_LENGTH as u64))
        );
        assert!(Cursor::decode(&"A".repeat(Cursor::MAX_ENCODED_LENGTH)).is_ok());
    }

    #[test]
    fn debug_and_display_show_the_encoded_form() {
        let c = Cursor::from_bytes(b"foobar");
        assert_eq!(c.to_string(), "Zm9vYmFy");
        assert_eq!(format!("{c:?}"), "Cursor(Zm9vYmFy)");
        assert_eq!(c.len(), 6);
        assert!(!c.is_empty());
        assert!(Cursor::from_bytes(b"").is_empty());
    }

    #[test]
    fn serialises_as_the_encoded_string() {
        let c = Cursor::from_bytes(b"foobar");
        assert_eq!(serde_json::to_value(&c).unwrap(), json!("Zm9vYmFy"));
        assert_eq!(serde_json::from_str::<Cursor>("\"Zm9vYmFy\"").unwrap(), c);
        let err = serde_json::from_str::<Cursor>("\"not base64!\"").unwrap_err();
        assert_eq!(
            crate::types::parse_serde_message(&err.to_string()).map(|(c, _)| c),
            Some(codes::FORMAT)
        );
        assert!("Zm9vYmFy".parse::<Cursor>().is_ok());
        assert!(Cursor::try_from(String::from("!")).is_err());
    }

    #[test]
    fn json_schema_documents_what_is_enforced() {
        let node = Cursor::json_schema(&mut SchemaGenerator::default());
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            json!({
                "type": "string",
                "format": "cursor",
                "contentEncoding": "base64url",
                "maxLength": 2048,
                "description": "An opaque pagination token. Pass it back unmodified.",
            })
        );
    }
}
