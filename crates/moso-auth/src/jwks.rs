//! JSON Web Keys: publishing this service's public keys, and consuming another
//! service's.
//!
//! A JWKS document is how a token issuer rotates a key without redeploying
//! every consumer. The issuer publishes `/.well-known/jwks.json`; the consumer
//! fetches it, caches it, and looks a key up by the `kid` in the token header.
//!
//! Two things in this module are load-bearing and easy to get wrong:
//!
//! 1. **A `kid` selects a key; it never selects an algorithm.** [`JwkSet::find`]
//!    returns a key, and the caller has already decided which algorithm it will
//!    verify with. That is the structural half of the defence against algorithm
//!    confusion — see [`crate::jwt`] for the other half.
//! 2. **An unknown `kid` must not be a request to the issuer.** A stream of
//!    tokens with made-up `kid`s is otherwise a denial-of-service amplifier
//!    pointed at somebody else's identity provider. The refetch throttle lives
//!    in [`RemoteJwks`](crate::RemoteJwks); this module only fetches when asked.
//!
//! ```
//! use moso_auth::jwks::JwkSet;
//!
//! let document = r#"{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"k1",
//!     "x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}]}"#;
//! let set: JwkSet = serde_json::from_str(document).unwrap();
//! assert!(set.find("k1").is_some());
//! assert!(set.find("k2").is_none());
//! ```
//!
//! This module is also where the small cryptographic primitives shared by
//! [`crate::jwt`], [`crate::apikey`] and [`crate::totp`] live — base64url, a
//! CSPRNG draw, a hex SHA-256 and a constant-time compare. They are here rather
//! than in a fifth module because a JWK is the one place all three meet, and
//! because four functions do not earn a file.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use crate::jwt::JwtAlgorithm;
use crate::{Error, Result};

// ---------------------------------------------------------------------------
// base64url
// ---------------------------------------------------------------------------

/// base64url without padding — the only encoding a JWT or a JWK uses.
pub(crate) fn b64u(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode base64url without padding.
///
/// Padding is *rejected* rather than tolerated: RFC 7515 § 2 defines the
/// encoding as unpadded, and a decoder that accepts both spellings of the same
/// bytes gives a token two signatures' worth of wiggle room.
pub(crate) fn unb64u(text: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(text.as_bytes())
        .map_err(|_| Error::InvalidCredentials)
}

// ---------------------------------------------------------------------------
// Shared primitives
// ---------------------------------------------------------------------------

/// `count` bytes from the operating system's CSPRNG.
///
/// A failure here is an [`Error::Unavailable`] and not a panic: on a container
/// that has exhausted its file descriptors, `getrandom` fails, and a 503 that
/// says "the system random generator is unavailable" is diagnosable where an
/// unwrap in a token mint is not.
pub(crate) fn random_bytes(count: usize) -> Result<Vec<u8>> {
    use ring::rand::SecureRandom as _;

    let mut bytes = vec![0u8; count];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| Error::Unavailable {
            component: "system random generator",
            detail: "the operating system refused to provide randomness".to_owned(),
            source: None,
        })?;
    Ok(bytes)
}

/// The lowercase hex SHA-256 of `bytes`.
///
/// The storage form for an API-key secret and a refresh token: both are
/// high-entropy values drawn from [`random_bytes`], so the argument for a
/// password hash — that the input is guessable and the attacker's cost must be
/// raised — does not apply, and a digest that is fast to compute keeps
/// authentication off the blocking pool.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut hex = String::with_capacity(digest.as_ref().len() * 2);
    for byte in digest.as_ref() {
        // Two hex digits, always, so the string length is fixed and comparing
        // two of them cannot leak through a length check.
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    hex
}

/// Compare two byte strings without an early exit.
///
/// `==` on a `str` stops at the first differing byte, which turns a stored hash
/// into an oracle that can be walked one byte at a time. Every comparison of a
/// presented secret against a stored one in this crate goes through here.
pub(crate) fn ct_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    // `black_box` stops the optimiser from turning the accumulator back into a
    // branch. It is a hint, not a guarantee — the guarantee is that the loop
    // above has no data-dependent control flow.
    std::hint::black_box(difference) == 0
}

// ---------------------------------------------------------------------------
// A minimal DER reader
// ---------------------------------------------------------------------------

/// Just enough DER to unwrap a `SubjectPublicKeyInfo` and to split a PKCS#1
/// `RSAPublicKey` into its two integers.
///
/// Deliberately tiny and deliberately total: every method returns an `Option`
/// and nothing indexes without checking. The alternative — a general-purpose
/// ASN.1 crate — is several thousand lines and a new dependency to parse two
/// shapes that have not changed since 1998.
mod der {
    /// A cursor over DER bytes.
    pub(super) struct Reader<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    /// `SEQUENCE`, constructed.
    pub(super) const SEQUENCE: u8 = 0x30;
    /// `INTEGER`.
    pub(super) const INTEGER: u8 = 0x02;
    /// `BIT STRING`.
    pub(super) const BIT_STRING: u8 = 0x03;

    impl<'a> Reader<'a> {
        /// A reader over `bytes`.
        pub(super) fn new(bytes: &'a [u8]) -> Self {
            Self { bytes, pos: 0 }
        }

        /// Whether every byte has been consumed.
        pub(super) fn is_empty(&self) -> bool {
            self.pos >= self.bytes.len()
        }

        /// Read one tag-length-value, checking the tag.
        pub(super) fn tlv(&mut self, tag: u8) -> Option<&'a [u8]> {
            let first = *self.bytes.get(self.pos)?;
            if first != tag {
                return None;
            }
            self.pos += 1;
            let length = self.length()?;
            let start = self.pos;
            let end = start.checked_add(length)?;
            if end > self.bytes.len() {
                return None;
            }
            self.pos = end;
            Some(&self.bytes[start..end])
        }

        /// Read a definite-form length. Indefinite form is not DER.
        fn length(&mut self) -> Option<usize> {
            let first = *self.bytes.get(self.pos)?;
            self.pos += 1;
            if first < 0x80 {
                return Some(usize::from(first));
            }
            let count = usize::from(first & 0x7f);
            // 0x80 is the indefinite form (not DER) and more than 4 length
            // bytes is longer than any key we will ever see.
            if count == 0 || count > 4 {
                return None;
            }
            let mut value: usize = 0;
            for _ in 0..count {
                let byte = *self.bytes.get(self.pos)?;
                self.pos += 1;
                value = value.checked_mul(256)?.checked_add(usize::from(byte))?;
            }
            Some(value)
        }
    }

    /// The key bits inside a `SubjectPublicKeyInfo`, or `None` when this is not
    /// one.
    ///
    /// `SEQUENCE { SEQUENCE { OID, params }, BIT STRING }`. The algorithm
    /// identifier is skipped rather than checked: the caller already knows which
    /// algorithm it is willing to use, and validating the OID here would only
    /// let a *matching* OID smuggle in a key the caller did not ask for.
    pub(super) fn spki_key_bits(bytes: &[u8]) -> Option<Vec<u8>> {
        let mut outer = Reader::new(bytes);
        let body = outer.tlv(SEQUENCE)?;
        if !outer.is_empty() {
            return None;
        }
        let mut inner = Reader::new(body);
        let _algorithm = inner.tlv(SEQUENCE)?;
        let bit_string = inner.tlv(BIT_STRING)?;
        if !inner.is_empty() {
            return None;
        }
        // The first octet of a BIT STRING is the number of unused trailing
        // bits. For a public key it is always zero.
        match bit_string.split_first() {
            Some((0, rest)) => Some(rest.to_vec()),
            _ => None,
        }
    }

    /// `(modulus, exponent)` from a PKCS#1 `RSAPublicKey`, as unsigned
    /// big-endian bytes with the DER sign padding removed.
    pub(super) fn rsa_public_key(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        let mut outer = Reader::new(bytes);
        let body = outer.tlv(SEQUENCE)?;
        if !outer.is_empty() {
            return None;
        }
        let mut inner = Reader::new(body);
        let modulus = inner.tlv(INTEGER)?;
        let exponent = inner.tlv(INTEGER)?;
        if !inner.is_empty() {
            return None;
        }
        Some((unsigned(modulus), unsigned(exponent)))
    }

    /// Strip the leading zero DER adds to keep an integer positive.
    fn unsigned(bytes: &[u8]) -> Vec<u8> {
        let trimmed = bytes
            .iter()
            .position(|byte| *byte != 0)
            .map_or(&bytes[..0], |first| &bytes[first..]);
        trimmed.to_vec()
    }
}

// ---------------------------------------------------------------------------
// Jwk
// ---------------------------------------------------------------------------

/// One public key, in the JSON Web Key encoding of RFC 7517.
///
/// Only the members Moso reads or writes are modelled. Anything else in the
/// document is ignored on the way in and absent on the way out, which is what
/// keeps a new registered member from being a breaking change here.
///
/// ```
/// use moso_auth::jwks::Jwk;
///
/// let json = r#"{"kty":"OKP","crv":"Ed25519","kid":"2026-07",
///     "x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}"#;
/// let key: Jwk = serde_json::from_str(json).unwrap();
/// assert_eq!(key.kid.as_deref(), Some("2026-07"));
/// assert_eq!(key.kty, "OKP");
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Jwk {
    /// The key type: `OKP` for Ed25519, `EC` for P-256, `RSA` for RSA.
    pub kty: String,
    /// The curve, for `OKP` and `EC`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crv: Option<String>,
    /// Which key. The `kid` in a token header names one of these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kid: Option<String>,
    /// The algorithm this key is for. Advisory: a verifier is constructed with
    /// a fixed algorithm and does not read this to choose one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alg: Option<String>,
    /// `sig` or `enc`. Moso only ever publishes `sig`.
    #[serde(rename = "use", default, skip_serializing_if = "Option::is_none")]
    pub key_use: Option<String>,
    /// The RSA modulus, base64url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<String>,
    /// The RSA public exponent, base64url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e: Option<String>,
    /// The `OKP` public key, or the EC x coordinate, base64url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<String>,
    /// The EC y coordinate, base64url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<String>,
}

impl Jwk {
    /// Whether this key can be used for signatures.
    ///
    /// `use` absent means "unrestricted" per RFC 7517 § 4.2, so only an explicit
    /// `enc` disqualifies a key.
    ///
    /// ```
    /// use moso_auth::jwks::Jwk;
    ///
    /// let json = r#"{"kty":"OKP","use":"enc","x":"AA"}"#;
    /// let key: Jwk = serde_json::from_str(json).unwrap();
    /// assert!(!key.is_signing_key());
    /// ```
    #[must_use]
    pub fn is_signing_key(&self) -> bool {
        !matches!(self.key_use.as_deref(), Some("enc"))
    }
}

/// A set of public keys, as served at
/// [`JWKS_PATH`](crate::jwt::JWKS_PATH).
///
/// ```
/// use moso_auth::jwks::JwkSet;
///
/// let empty = JwkSet::default();
/// assert!(empty.keys.is_empty());
/// assert_eq!(serde_json::to_string(&empty).unwrap(), r#"{"keys":[]}"#);
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JwkSet {
    /// The keys, in publication order.
    pub keys: Vec<Jwk>,
}

impl JwkSet {
    /// A set over `keys`.
    ///
    /// ```
    /// use moso_auth::jwks::{Jwk, JwkSet};
    ///
    /// let set = JwkSet::new(Vec::<Jwk>::new());
    /// assert!(set.keys.is_empty());
    /// ```
    #[must_use]
    pub fn new(keys: Vec<Jwk>) -> Self {
        Self { keys }
    }

    /// The key with this `kid`, if the set has one.
    ///
    /// ```
    /// use moso_auth::jwks::JwkSet;
    ///
    /// let set: JwkSet = serde_json::from_str(r#"{"keys":[]}"#).unwrap();
    /// assert!(set.find("anything").is_none());
    /// ```
    #[must_use]
    pub fn find(&self, kid: &str) -> Option<&Jwk> {
        self.keys
            .iter()
            .find(|key| key.kid.as_deref() == Some(kid) && key.is_signing_key())
    }
}

// ---------------------------------------------------------------------------
// Key material
// ---------------------------------------------------------------------------

/// A public key in the shape the verifying algorithm wants it.
///
/// This is the only representation the crate carries after construction: a
/// caller hands [`Jwt::verifier`](crate::Jwt::verifier) bytes in whatever
/// encoding it has, they are parsed once, and a parse failure is a configuration
/// error at boot rather than an authentication failure at 3 a.m.
#[derive(Clone, Debug)]
pub(crate) enum VerifyingKey {
    /// A raw 32-byte Ed25519 public key.
    Ed25519(Vec<u8>),
    /// An uncompressed SEC1 point, `0x04 || x || y`, 65 bytes.
    P256(Vec<u8>),
    /// Modulus and exponent, unsigned big-endian.
    Rsa {
        /// The modulus.
        n: Vec<u8>,
        /// The public exponent.
        e: Vec<u8>,
    },
    /// A shared secret. Symmetric, so this is also the *signing* key, which is
    /// the whole argument against HS256 between services.
    Hmac(Vec<u8>),
}

/// How many bytes an uncompressed P-256 point takes.
const P256_POINT_LEN: usize = 65;
/// How many bytes an Ed25519 public key takes.
const ED25519_KEY_LEN: usize = 32;
/// How many bytes one P-256 coordinate takes.
const P256_COORD_LEN: usize = 32;

impl VerifyingKey {
    /// Parse `bytes` as a public key for `algorithm`.
    ///
    /// Accepts, per algorithm:
    ///
    /// | Algorithm | Accepted encodings |
    /// | --- | --- |
    /// | `EdDSA` | 32 raw bytes, or a `SubjectPublicKeyInfo` wrapping them |
    /// | `ES256` | `0x04 ‖ x ‖ y` (65 bytes), `x ‖ y` (64), or an SPKI |
    /// | `RS256` | PKCS#1 `RSAPublicKey`, or an SPKI wrapping one |
    /// | `HS256` | the shared secret, as-is |
    pub(crate) fn parse(algorithm: JwtAlgorithm, bytes: &[u8]) -> Result<Self> {
        match algorithm {
            JwtAlgorithm::EdDSA => {
                if bytes.len() == ED25519_KEY_LEN {
                    return Ok(Self::Ed25519(bytes.to_vec()));
                }
                match der::spki_key_bits(bytes) {
                    Some(key) if key.len() == ED25519_KEY_LEN => Ok(Self::Ed25519(key)),
                    _ => Err(Error::Config(
                        "an EdDSA verifying key must be 32 raw bytes or a SubjectPublicKeyInfo \
                         wrapping them"
                            .into(),
                    )),
                }
            }
            JwtAlgorithm::ES256 => {
                if let Some(point) = normalise_p256(bytes) {
                    return Ok(Self::P256(point));
                }
                match der::spki_key_bits(bytes)
                    .as_deref()
                    .and_then(normalise_p256)
                {
                    Some(point) => Ok(Self::P256(point)),
                    None => Err(Error::Config(
                        "an ES256 verifying key must be an uncompressed P-256 point (65 bytes, \
                         leading 0x04), the bare coordinates (64 bytes), or a \
                         SubjectPublicKeyInfo"
                            .into(),
                    )),
                }
            }
            JwtAlgorithm::RS256 => {
                if let Some((n, e)) = der::rsa_public_key(bytes) {
                    return Ok(Self::Rsa { n, e });
                }
                match der::spki_key_bits(bytes)
                    .as_deref()
                    .and_then(der::rsa_public_key)
                {
                    Some((n, e)) => Ok(Self::Rsa { n, e }),
                    None => Err(Error::Config(
                        "an RS256 verifying key must be a DER RSAPublicKey (PKCS#1) or a \
                         SubjectPublicKeyInfo wrapping one"
                            .into(),
                    )),
                }
            }
            JwtAlgorithm::HS256 => {
                if bytes.is_empty() {
                    return Err(Error::Config("an HS256 key must not be empty".into()));
                }
                Ok(Self::Hmac(bytes.to_vec()))
            }
        }
    }

    /// Build a key from a JWK, refusing anything that is not the algorithm the
    /// caller has already committed to.
    pub(crate) fn from_jwk(jwk: &Jwk, algorithm: JwtAlgorithm) -> Result<Self> {
        let wrong = |what: &str| {
            Error::Config(format!("a JWK for {} must be {what}", algorithm.as_str()).into())
        };
        match algorithm {
            JwtAlgorithm::EdDSA => {
                if jwk.kty != "OKP" || jwk.crv.as_deref() != Some("Ed25519") {
                    return Err(wrong("kty=OKP with crv=Ed25519"));
                }
                let x = jwk.x.as_deref().ok_or_else(|| wrong("kty=OKP with an x"))?;
                let key = unb64u(x).map_err(|_| wrong("kty=OKP with a base64url x"))?;
                if key.len() != ED25519_KEY_LEN {
                    return Err(wrong("kty=OKP with a 32-byte x"));
                }
                Ok(Self::Ed25519(key))
            }
            JwtAlgorithm::ES256 => {
                if jwk.kty != "EC" || jwk.crv.as_deref() != Some("P-256") {
                    return Err(wrong("kty=EC with crv=P-256"));
                }
                let (Some(x), Some(y)) = (jwk.x.as_deref(), jwk.y.as_deref()) else {
                    return Err(wrong("kty=EC with both x and y"));
                };
                let x = unb64u(x).map_err(|_| wrong("kty=EC with a base64url x"))?;
                let y = unb64u(y).map_err(|_| wrong("kty=EC with a base64url y"))?;
                if x.len() != P256_COORD_LEN || y.len() != P256_COORD_LEN {
                    return Err(wrong("kty=EC with 32-byte coordinates"));
                }
                let mut point = Vec::with_capacity(P256_POINT_LEN);
                point.push(0x04);
                point.extend_from_slice(&x);
                point.extend_from_slice(&y);
                Ok(Self::P256(point))
            }
            JwtAlgorithm::RS256 => {
                if jwk.kty != "RSA" {
                    return Err(wrong("kty=RSA"));
                }
                let (Some(n), Some(e)) = (jwk.n.as_deref(), jwk.e.as_deref()) else {
                    return Err(wrong("kty=RSA with both n and e"));
                };
                let n = unb64u(n).map_err(|_| wrong("kty=RSA with a base64url n"))?;
                let e = unb64u(e).map_err(|_| wrong("kty=RSA with a base64url e"))?;
                if n.is_empty() || e.is_empty() {
                    return Err(wrong("kty=RSA with non-empty n and e"));
                }
                Ok(Self::Rsa { n, e })
            }
            JwtAlgorithm::HS256 => Err(Error::Config(
                "HS256 keys are secrets and are never published in a JWKS; configure the shared \
                 secret directly"
                    .into(),
            )),
        }
    }

    /// Render as a JWK, or `None` for a symmetric key — which is the point:
    /// there is nothing about an HS256 configuration that can be published, so
    /// every consumer needs the secret that also lets it forge.
    pub(crate) fn to_jwk(&self, kid: &str, algorithm: JwtAlgorithm) -> Option<Jwk> {
        let mut jwk = Jwk {
            kid: Some(kid.to_owned()),
            alg: Some(algorithm.as_str().to_owned()),
            key_use: Some("sig".to_owned()),
            ..Jwk::default()
        };
        match self {
            Self::Ed25519(key) => {
                jwk.kty = "OKP".to_owned();
                jwk.crv = Some("Ed25519".to_owned());
                jwk.x = Some(b64u(key));
            }
            Self::P256(point) => {
                let coordinates = point.get(1..)?;
                let (x, y) = coordinates.split_at_checked(P256_COORD_LEN)?;
                jwk.kty = "EC".to_owned();
                jwk.crv = Some("P-256".to_owned());
                jwk.x = Some(b64u(x));
                jwk.y = Some(b64u(y));
            }
            Self::Rsa { n, e } => {
                jwk.kty = "RSA".to_owned();
                jwk.n = Some(b64u(n));
                jwk.e = Some(b64u(e));
            }
            Self::Hmac(_) => return None,
        }
        Some(jwk)
    }

    /// Verify `signature` over `message`.
    ///
    /// Takes the algorithm from the caller and never from the token, and returns
    /// a plain `bool` because there is exactly one thing a caller may do with a
    /// failure.
    pub(crate) fn verify(&self, algorithm: JwtAlgorithm, message: &[u8], signature: &[u8]) -> bool {
        use ring::signature;

        match (self, algorithm) {
            (Self::Ed25519(key), JwtAlgorithm::EdDSA) => {
                signature::UnparsedPublicKey::new(&signature::ED25519, key)
                    .verify(message, signature)
                    .is_ok()
            }
            (Self::P256(point), JwtAlgorithm::ES256) => {
                // FIXED, not ASN1: JWS encodes an ECDSA signature as the raw
                // r ‖ s pair. Accepting the DER spelling as well would make one
                // signature representable two ways, which is a replay window.
                signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point)
                    .verify(message, signature)
                    .is_ok()
            }
            (Self::Rsa { n, e }, JwtAlgorithm::RS256) => {
                let components = signature::RsaPublicKeyComponents { n, e };
                components
                    .verify(&signature::RSA_PKCS1_2048_8192_SHA256, message, signature)
                    .is_ok()
            }
            (Self::Hmac(secret), JwtAlgorithm::HS256) => {
                let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
                ring::hmac::verify(&key, message, signature).is_ok()
            }
            // A key of one kind against an algorithm of another is the
            // algorithm-confusion attack arriving through the front door. It is
            // unreachable through the public API, because a `Jwt` holds one
            // algorithm and parses its keys with it, and it is `false` here so
            // that staying unreachable is not load-bearing.
            _ => false,
        }
    }
}

/// Normalise a P-256 public key to the uncompressed SEC1 form ring wants.
fn normalise_p256(bytes: &[u8]) -> Option<Vec<u8>> {
    match bytes.len() {
        P256_POINT_LEN if bytes.first() == Some(&0x04) => Some(bytes.to_vec()),
        64 => {
            let mut point = Vec::with_capacity(P256_POINT_LEN);
            point.push(0x04);
            point.extend_from_slice(bytes);
            Some(point)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

/// The largest JWKS document this crate will read.
///
/// A key set is a handful of keys; anything past this is either a
/// misconfiguration or somebody feeding an infinite body to a client that
/// buffers. 256 KiB is roughly a hundred RSA-4096 keys.
pub(crate) const MAX_JWKS_BYTES: usize = 256 * 1024;

/// Fetch and parse a JWKS document over HTTP or HTTPS.
///
/// This is the one outbound request Moso makes. See [`get`] for why it is
/// written on the socket rather than on a client library.
pub(crate) async fn fetch(url: &str, timeout: Duration) -> Result<JwkSet> {
    let body = tokio::time::timeout(timeout, get(url))
        .await
        .map_err(|_| Error::Unavailable {
            component: "jwks endpoint",
            detail: format!("no response from {url} within {timeout:?}"),
            source: None,
        })??;
    serde_json::from_slice(&body).map_err(|error| Error::Unavailable {
        component: "jwks endpoint",
        detail: format!("{url} did not return a JWKS document: {error}"),
        source: Some(Box::new(error)),
    })
}

/// The pieces of a URL this client needs.
#[derive(Debug)]
struct Target<'a> {
    /// Whether to wrap the connection in TLS.
    tls: bool,
    /// The host, for both the connection and SNI.
    host: &'a str,
    /// The port.
    port: u16,
    /// Path plus query.
    path: &'a str,
}

/// Split a URL into the four things a `GET` needs.
///
/// Deliberately narrow: no userinfo, no fragment, no IPv6 literal shortcuts
/// beyond the bracketed form. A JWKS URL comes from configuration, not from a
/// request, so refusing an exotic one at boot is the right trade.
fn split_url(url: &str) -> Result<Target<'_>> {
    let bad = |why: &'static str| Error::Config(format!("jwks url `{url}` {why}").into());
    let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(bad("must start with http:// or https://"));
    };
    if rest.contains('@') {
        return Err(bad("must not carry userinfo"));
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        // A colon inside brackets is part of an IPv6 literal, not a port.
        Some((host, port)) if !port.contains(']') => {
            let port = port.parse::<u16>().map_err(|_| bad("has a bad port"))?;
            (host, port)
        }
        _ => (authority, if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(bad("has no host"));
    }
    Ok(Target {
        tls,
        host,
        port,
        path,
    })
}
/// The largest set of response headers this client will buffer.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// One `GET`, returning the body.
///
/// Written directly on the socket rather than on a client library, and that is
/// a deliberate simplification rather than a shortcut. The requirement is one
/// unauthenticated `GET` of a small JSON document with a timeout and a size cap;
/// what a client library adds is a connection pool, redirects, a dispatcher task
/// and a second TLS stack — and reqwest 0.13's rustls feature in particular
/// pulls `aws-lc-rs`, which is a *second* crypto provider in a process where
/// sqlx has already installed ring's. That is a runtime panic, not a compile
/// error.
///
/// Redirects are not followed: a JWKS URL comes from an OIDC discovery document
/// or from configuration, and silently following a 302 to somewhere else is how
/// a key set gets fetched from a host nobody audited.
async fn get(url: &str) -> Result<Vec<u8>> {
    let target = split_url(url)?;
    let unreachable = |detail: String| Error::Unavailable {
        component: "jwks endpoint",
        detail,
        source: None,
    };

    let stream = tokio::net::TcpStream::connect((target.host, target.port))
        .await
        .map_err(|error| unreachable(format!("cannot reach {url}: {error}")))?;
    // A JWKS fetch is one small request and one small response; Nagle's
    // algorithm only adds latency to that shape.
    let _ = stream.set_nodelay(true);

    // `Connection: close` makes the response end at EOF, which is the one
    // framing every server agrees on. `Content-Length` and `Transfer-Encoding:
    // chunked` are both still honoured below, for the servers that ignore it.
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Accept: application/json\r\n\
         User-Agent: moso-auth/{version}\r\n\
         Connection: close\r\n\
         \r\n",
        path = target.path,
        host = target.host,
        version = env!("CARGO_PKG_VERSION"),
    );

    let raw = if target.tls {
        let connector = tokio_rustls::TlsConnector::from(tls_config());
        let server_name = rustls_pki_types::ServerName::try_from(target.host)
            .map_err(|_| {
                Error::Config(format!("jwks url `{url}` has a host TLS cannot verify").into())
            })?
            .to_owned();
        let stream = connector
            .connect(server_name, stream)
            .await
            .map_err(|error| unreachable(format!("TLS handshake with {url} failed: {error}")))?;
        exchange(stream, &request).await
    } else {
        exchange(stream, &request).await
    }
    .map_err(|error| match error {
        Error::Unavailable { detail, .. } => unreachable(format!("{url}: {detail}")),
        other => other,
    })?;

    Ok(raw)
}

/// Write the request, read the response, return the body.
async fn exchange<S>(mut stream: S, request: &str) -> Result<Vec<u8>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let failed = |detail: String| Error::Unavailable {
        component: "jwks endpoint",
        detail,
        source: None,
    };

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| failed(format!("could not send the request: {error}")))?;
    stream
        .flush()
        .await
        .map_err(|error| failed(format!("could not send the request: {error}")))?;

    // Read the head first, so a body is only ever accepted once its framing is
    // known and its size can be bounded.
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(index) = find_head_end(&buffer) {
            break index;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(failed(format!(
                "the response headers exceeded {MAX_HEADER_BYTES} bytes"
            )));
        }
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| failed(format!("the connection failed mid-response: {error}")))?;
        if read == 0 {
            return Err(failed("the connection closed before a response".to_owned()));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = core::str::from_utf8(&buffer[..head_end])
        .map_err(|_| failed("the response head was not text".to_owned()))?
        .to_owned();
    let head = Head::parse(&head).ok_or_else(|| failed("unreadable response head".to_owned()))?;
    if !(200..300).contains(&head.status) {
        return Err(failed(format!("answered {}", head.status)));
    }

    // Everything after the blank line is body; `head_end` points at the `\r\n`
    // that ends the last header, and the terminator is four bytes long.
    let mut body = buffer.split_off(head_end + 4);

    match head.framing {
        Framing::Length(length) => {
            if length > MAX_JWKS_BYTES {
                return Err(failed(format!(
                    "the document is {length} bytes, over the {MAX_JWKS_BYTES}-byte cap"
                )));
            }
            while body.len() < length {
                let read = stream
                    .read(&mut chunk)
                    .await
                    .map_err(|error| failed(format!("the connection failed mid-body: {error}")))?;
                if read == 0 {
                    return Err(failed("the body ended early".to_owned()));
                }
                body.extend_from_slice(&chunk[..read]);
            }
            body.truncate(length);
            Ok(body)
        }
        Framing::Chunked => {
            read_to_end(&mut stream, &mut body, &mut chunk).await?;
            dechunk(&body).ok_or_else(|| failed("the chunked body was malformed".to_owned()))
        }
        Framing::ToEnd => {
            read_to_end(&mut stream, &mut body, &mut chunk).await?;
            Ok(body)
        }
    }
}

/// Read until the peer hangs up, refusing to buffer more than the cap.
async fn read_to_end<S>(stream: &mut S, body: &mut Vec<u8>, chunk: &mut [u8]) -> Result<()>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;

    loop {
        if body.len() > MAX_JWKS_BYTES {
            return Err(Error::Unavailable {
                component: "jwks endpoint",
                detail: format!("the document exceeded the {MAX_JWKS_BYTES}-byte cap"),
                source: None,
            });
        }
        let read = match stream.read(chunk).await {
            Ok(read) => read,
            // A server that has finished writing and closes with our request
            // still unread makes the kernel send RST rather than FIN, so a
            // complete response can arrive followed by "connection reset". With
            // bytes already in hand, treat that as the end of the body: a
            // *truncated* body is still caught, by the chunk framing or by the
            // JSON parse.
            Err(error) if !body.is_empty() => {
                tracing::debug!(
                    target: "moso_auth::jwks",
                    %error,
                    "the jwks connection was reset after the body arrived"
                );
                return Ok(());
            }
            Err(error) => {
                return Err(Error::Unavailable {
                    component: "jwks endpoint",
                    detail: format!("the connection failed mid-body: {error}"),
                    source: None,
                });
            }
        };
        if read == 0 {
            return Ok(());
        }
        body.extend_from_slice(&chunk[..read]);
    }
}

/// Where the blank line separating the head from the body starts.
fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// How the body's end is determined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    /// `Content-Length`.
    Length(usize),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// Neither: the body ends when the connection does.
    ToEnd,
}

/// The two things this client reads out of a response head.
#[derive(Debug)]
struct Head {
    /// The status code.
    status: u16,
    /// How to find the end of the body.
    framing: Framing,
}

impl Head {
    /// Parse a status line and headers. `None` for anything that is not
    /// HTTP/1.x.
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.split("\r\n");
        let status_line = lines.next()?;
        let mut parts = status_line.split(' ');
        let version = parts.next()?;
        if !version.starts_with("HTTP/1.") {
            return None;
        }
        let status = parts.next()?.parse::<u16>().ok()?;

        let mut framing = Framing::ToEnd;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            if name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                // Chunked wins over Content-Length, per RFC 9112 § 6.1 — and a
                // response carrying both is a request-smuggling shape, which is
                // one more reason not to prefer the length.
                framing = Framing::Chunked;
            } else if name.eq_ignore_ascii_case("content-length")
                && framing != Framing::Chunked
                && let Ok(length) = value.parse::<usize>()
            {
                framing = Framing::Length(length);
            }
        }
        Some(Self { status, framing })
    }
}

/// Decode a chunked body. `None` when the framing does not add up.
fn dechunk(body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body;
    loop {
        let line_end = rest.windows(2).position(|window| window == b"\r\n")?;
        let header = core::str::from_utf8(&rest[..line_end]).ok()?;
        // A chunk extension follows a semicolon and is not something this
        // client has any use for.
        let size_text = header.split(';').next()?.trim();
        let size = usize::from_str_radix(size_text, 16).ok()?;
        rest = rest.get(line_end + 2..)?;
        if size == 0 {
            return Some(out);
        }
        if out.len().checked_add(size)? > MAX_JWKS_BYTES {
            return None;
        }
        out.extend_from_slice(rest.get(..size)?);
        // Each chunk is followed by its own CRLF.
        rest = rest.get(size + 2..)?;
    }
}

/// The shared client TLS configuration.
///
/// Built once. The provider is named rather than taken from the process default
/// because sqlx has already installed ring's, and two providers is a panic.
fn tls_config() -> Arc<rustls::ClientConfig> {
    use std::sync::OnceLock;

    /// One configuration for the process; building a root store is not free.
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                // Both arms are unreachable with the ring provider and the
                // default versions; `expect` here rather than a `Result` on
                // every caller, because a failure would be a bug in this
                // function and not something an application can act on.
                .expect("ring supports the default protocol versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 § 10, adapted: base64url of the test vectors, no padding.
    #[test]
    fn base64url_round_trips_without_padding() {
        for (bytes, encoded) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg"),
            (&b"fo"[..], "Zm8"),
            (&b"foo"[..], "Zm9v"),
            (&b"foob"[..], "Zm9vYg"),
            (&b"fooba"[..], "Zm9vYmE"),
            (&b"foobar"[..], "Zm9vYmFy"),
        ] {
            assert_eq!(b64u(bytes), encoded, "encoding {bytes:?}");
            assert_eq!(unb64u(encoded).unwrap(), bytes, "decoding {encoded:?}");
        }
    }

    /// The URL-safe alphabet, not the standard one: `+` and `/` must not appear
    /// or a token stops surviving a query string.
    #[test]
    fn base64url_uses_the_url_safe_alphabet() {
        let encoded = b64u(&[0xfb, 0xff, 0xbe]);
        assert!(!encoded.contains('+'), "{encoded}");
        assert!(!encoded.contains('/'), "{encoded}");
        assert_eq!(unb64u(&encoded).unwrap(), vec![0xfb, 0xff, 0xbe]);
    }

    /// Padding is a different spelling of the same bytes, and two spellings of
    /// one signature is a replay window.
    #[test]
    fn padded_base64_is_refused() {
        assert!(unb64u("Zg==").is_err());
        assert!(unb64u("Zg").is_ok());
    }

    /// A DER length in the long form is read, and a length that runs off the
    /// end is not.
    #[test]
    fn the_der_reader_refuses_a_truncated_value() {
        // SEQUENCE, length 0x05, but only three bytes follow.
        assert!(der::spki_key_bits(&[0x30, 0x05, 0x01, 0x02, 0x03]).is_none());
        // A bare integer is not an SPKI.
        assert!(der::spki_key_bits(&[0x02, 0x01, 0x00]).is_none());
        // Empty input.
        assert!(der::rsa_public_key(&[]).is_none());
    }

    /// The sign padding DER adds to a positive integer must not reach a JWK.
    #[test]
    fn rsa_integers_lose_their_der_sign_padding() {
        // SEQUENCE { INTEGER 0x00FF, INTEGER 0x010001 }
        let der_bytes = [
            0x30, 0x09, 0x02, 0x02, 0x00, 0xff, 0x02, 0x03, 0x01, 0x00, 0x01,
        ];
        let (n, e) = der::rsa_public_key(&der_bytes).expect("parses");
        assert_eq!(n, vec![0xff], "the leading zero is DER's, not the modulus'");
        assert_eq!(e, vec![0x01, 0x00, 0x01], "65537");
    }

    /// A P-256 key arrives three ways and must become one.
    #[test]
    fn p256_keys_normalise_to_the_uncompressed_point() {
        let coordinates = vec![7u8; 64];
        let mut uncompressed = vec![0x04];
        uncompressed.extend_from_slice(&coordinates);

        let from_bare = VerifyingKey::parse(JwtAlgorithm::ES256, &coordinates).unwrap();
        let from_point = VerifyingKey::parse(JwtAlgorithm::ES256, &uncompressed).unwrap();
        match (from_bare, from_point) {
            (VerifyingKey::P256(a), VerifyingKey::P256(b)) => {
                assert_eq!(a, b);
                assert_eq!(a.len(), P256_POINT_LEN);
                assert_eq!(a[0], 0x04);
            }
            other => panic!("expected two P-256 keys, got {other:?}"),
        }
    }

    /// A key of the wrong length is a boot-time configuration error with a
    /// message that says what the right one looks like — not a 3 a.m. 401.
    #[test]
    fn a_wrong_length_key_is_a_configuration_error() {
        let error = VerifyingKey::parse(JwtAlgorithm::EdDSA, &[0u8; 31]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("32 raw bytes"), "{text}");
        assert!(matches!(error, Error::Config(_)));
    }

    /// An empty HS256 secret is not a key.
    #[test]
    fn an_empty_symmetric_key_is_refused() {
        assert!(VerifyingKey::parse(JwtAlgorithm::HS256, &[]).is_err());
        assert!(VerifyingKey::parse(JwtAlgorithm::HS256, b"hunter2").is_ok());
    }

    /// A symmetric key has nothing to publish. That is the argument against it,
    /// stated as code.
    #[test]
    fn a_symmetric_key_renders_no_jwk() {
        let key = VerifyingKey::Hmac(b"shared".to_vec());
        assert!(key.to_jwk("k1", JwtAlgorithm::HS256).is_none());
    }

    /// Publishing a key and reading it back must produce the same bytes, or
    /// rotation silently breaks every consumer.
    #[test]
    fn a_jwk_round_trips_through_json() {
        let key = VerifyingKey::Ed25519(vec![9u8; 32]);
        let jwk = key
            .to_jwk("2026-07", JwtAlgorithm::EdDSA)
            .expect("asymmetric");
        let json = serde_json::to_string(&jwk).unwrap();
        let parsed: Jwk = serde_json::from_str(&json).unwrap();
        let back = VerifyingKey::from_jwk(&parsed, JwtAlgorithm::EdDSA).unwrap();
        match back {
            VerifyingKey::Ed25519(bytes) => assert_eq!(bytes, vec![9u8; 32]),
            other => panic!("expected Ed25519, got {other:?}"),
        }
        assert_eq!(parsed.alg.as_deref(), Some("EdDSA"));
        assert_eq!(parsed.key_use.as_deref(), Some("sig"));
    }

    /// An EC key round-trips through both coordinates.
    #[test]
    fn an_ec_jwk_round_trips_through_json() {
        let mut point = vec![0x04];
        point.extend_from_slice(&[1u8; 32]);
        point.extend_from_slice(&[2u8; 32]);
        let jwk = VerifyingKey::P256(point.clone())
            .to_jwk("ec", JwtAlgorithm::ES256)
            .expect("asymmetric");
        assert_eq!(jwk.crv.as_deref(), Some("P-256"));
        match VerifyingKey::from_jwk(&jwk, JwtAlgorithm::ES256).unwrap() {
            VerifyingKey::P256(back) => assert_eq!(back, point),
            other => panic!("expected P-256, got {other:?}"),
        }
    }

    /// An RSA key round-trips through n and e.
    #[test]
    fn an_rsa_jwk_round_trips_through_json() {
        let key = VerifyingKey::Rsa {
            n: vec![0xab; 256],
            e: vec![0x01, 0x00, 0x01],
        };
        let jwk = key.to_jwk("rsa", JwtAlgorithm::RS256).expect("asymmetric");
        assert_eq!(
            jwk.e.as_deref(),
            Some("AQAB"),
            "65537 is AQAB in every JWKS"
        );
        match VerifyingKey::from_jwk(&jwk, JwtAlgorithm::RS256).unwrap() {
            VerifyingKey::Rsa { n, e } => {
                assert_eq!(n, vec![0xab; 256]);
                assert_eq!(e, vec![0x01, 0x00, 0x01]);
            }
            other => panic!("expected RSA, got {other:?}"),
        }
    }

    /// Reading an OKP key as if it were EC is the shape of an
    /// algorithm-confusion attack, and it must not parse.
    #[test]
    fn a_jwk_is_refused_for_the_wrong_algorithm() {
        let jwk = VerifyingKey::Ed25519(vec![3u8; 32])
            .to_jwk("k", JwtAlgorithm::EdDSA)
            .unwrap();
        let error = VerifyingKey::from_jwk(&jwk, JwtAlgorithm::ES256).unwrap_err();
        assert!(error.to_string().contains("kty=EC"), "{error}");
        assert!(VerifyingKey::from_jwk(&jwk, JwtAlgorithm::HS256).is_err());
    }

    /// `use: enc` is not a signing key, and `find` must not return one.
    #[test]
    fn an_encryption_key_is_not_found_by_kid() {
        let set: JwkSet = serde_json::from_str(
            r#"{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"k1","use":"enc","x":"AA"}]}"#,
        )
        .unwrap();
        assert!(set.find("k1").is_none());
    }

    /// A member Moso does not model must not fail the parse — a JWKS gains
    /// members over time and a consumer that breaks on one is a consumer that
    /// breaks on the issuer's next release.
    #[test]
    fn an_unknown_member_does_not_fail_the_parse() {
        let set: JwkSet = serde_json::from_str(
            r#"{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"k1","x":"AA",
                "x5c":["…"],"key_ops":["verify"],"nbf":1}],"extra":true}"#,
        )
        .unwrap();
        assert_eq!(set.keys.len(), 1);
    }

    /// The URL split is the only parsing between configuration and a socket.
    #[test]
    fn urls_split_into_host_port_and_path() {
        let target = split_url("https://idp.example.com/.well-known/jwks.json").unwrap();
        assert!(target.tls);
        assert_eq!(target.host, "idp.example.com");
        assert_eq!(target.port, 443);
        assert_eq!(target.path, "/.well-known/jwks.json");

        let target = split_url("http://127.0.0.1:8080/jwks?v=2").unwrap();
        assert!(!target.tls);
        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.port, 8080);
        assert_eq!(target.path, "/jwks?v=2");

        let target = split_url("https://idp.example.com").unwrap();
        assert_eq!(target.path, "/", "a bare authority means the root");
    }

    /// Everything the split refuses, refused for a stated reason.
    #[test]
    fn a_url_that_is_not_a_plain_get_is_refused() {
        for url in [
            "ftp://idp.example.com/jwks",
            "idp.example.com/jwks",
            "https://user:pass@idp.example.com/jwks",
            "https://idp.example.com:notaport/jwks",
            "https:///jwks",
        ] {
            let error = split_url(url).unwrap_err();
            assert!(matches!(error, Error::Config(_)), "{url}: {error}");
        }
    }

    /// A timeout is `Unavailable` and not `InvalidCredentials`: a JWKS endpoint
    /// that is down must be a 503, because degrading it to "not logged in"
    /// logs everybody out of a system that is merely slow.
    #[tokio::test]
    async fn an_unreachable_endpoint_is_unavailable() {
        // Port 1 on the loopback: reserved, and nothing listens.
        let error = fetch("http://127.0.0.1:1/jwks", Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Unavailable { component, .. } if component == "jwks endpoint"),
            "{error}"
        );
    }

    /// Read a request head off a test socket.
    ///
    /// A server that answers without reading makes the kernel reset the
    /// connection on close, which is a property of the test and not of the
    /// client under test.
    async fn drain_request(stream: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt as _;

        let mut seen = Vec::new();
        let mut chunk = [0u8; 1024];
        while find_head_end(&seen).is_none() {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => seen.extend_from_slice(&chunk[..read]),
            }
        }
    }

    /// The happy path, against a real socket: a JWKS is fetched and parsed.
    #[tokio::test]
    async fn a_document_is_fetched_over_http() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = r#"{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"k1","x":"AA"}]}"#;
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;

            let (mut stream, _) = listener.accept().await.unwrap();
            drain_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
        });

        let set = fetch(&format!("http://{address}/jwks"), Duration::from_secs(5))
            .await
            .expect("fetched");
        assert_eq!(set.keys.len(), 1);
        assert_eq!(set.keys[0].kid.as_deref(), Some("k1"));
    }

    /// A non-2xx answer is an outage, not an empty key set — silently caching
    /// "no keys" from a 500 rejects every token until the cache expires.
    #[tokio::test]
    async fn a_failing_status_is_not_an_empty_key_set() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;

            let (mut stream, _) = listener.accept().await.unwrap();
            drain_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();
        });

        let error = fetch(&format!("http://{address}/jwks"), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("503"), "{error}");
    }

    /// A body that is not a JWKS is an outage too, and says so.
    #[tokio::test]
    async fn a_body_that_is_not_a_jwks_is_reported_as_such() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;

            let (mut stream, _) = listener.accept().await.unwrap();
            drain_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 13\r\n\r\nnot json here")
                .await
                .unwrap();
            stream.flush().await.unwrap();
        });

        let error = fetch(&format!("http://{address}/jwks"), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("did not return a JWKS document"),
            "{error}"
        );
    }

    /// The status line and the two framing headers, which is everything this
    /// client reads out of a response head.
    #[test]
    fn a_response_head_is_parsed_for_status_and_framing() {
        let head = Head::parse("HTTP/1.1 200 OK\r\ncontent-length: 42").expect("parses");
        assert_eq!(head.status, 200);
        assert_eq!(head.framing, Framing::Length(42));

        let head = Head::parse("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked").expect("parses");
        assert_eq!(head.framing, Framing::Chunked);

        // Both headers on one response is a request-smuggling shape; chunked
        // wins, per RFC 9112 § 6.1.
        let head =
            Head::parse("HTTP/1.1 200 OK\r\nContent-Length: 9\r\nTransfer-Encoding: chunked")
                .expect("parses");
        assert_eq!(head.framing, Framing::Chunked);

        // Neither: the body ends with the connection.
        let head = Head::parse("HTTP/1.0 204 No Content").expect("parses");
        assert_eq!(head.framing, Framing::ToEnd);

        assert!(
            Head::parse("HTTP/2 200").is_none(),
            "this client is HTTP/1.x"
        );
        assert!(Head::parse("nonsense").is_none());
        assert!(Head::parse("HTTP/1.1 not-a-number OK").is_none());
    }

    /// Chunked decoding, including the extension syntax and the terminator.
    #[test]
    fn a_chunked_body_is_decoded() {
        assert_eq!(
            dechunk(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n").as_deref(),
            Some(&b"hello world"[..])
        );
        assert_eq!(dechunk(b"0\r\n\r\n").as_deref(), Some(&b""[..]));
        // A chunk extension is ignored rather than refused.
        assert_eq!(
            dechunk(b"3;name=value\r\nabc\r\n0\r\n\r\n").as_deref(),
            Some(&b"abc"[..])
        );
        // Truncated, mis-sized and non-hex framings are all refused.
        assert!(dechunk(b"5\r\nhel").is_none());
        assert!(dechunk(b"zz\r\nabc\r\n0\r\n\r\n").is_none());
        assert!(dechunk(b"").is_none());
    }

    /// A chunked JWKS is fetched and parsed like any other. Some identity
    /// providers do not send a `Content-Length`.
    #[tokio::test]
    async fn a_chunked_document_is_fetched() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;

            let (mut stream, _) = listener.accept().await.unwrap();
            drain_request(&mut stream).await;
            // Chunked at awkward boundaries — mid-token and mid-string — so
            // the decoder is exercised rather than the happy alignment.
            let body = r#"{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"k1","x":"AA"}]}"#;
            let mut response = String::from(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 transfer-encoding: chunked\r\n\r\n",
            );
            for piece in body.as_bytes().chunks(7) {
                let piece = core::str::from_utf8(piece).expect("ascii");
                response.push_str(&format!("{:x}\r\n{piece}\r\n", piece.len()));
            }
            response.push_str("0\r\n\r\n");
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
        });

        let set = fetch(&format!("http://{address}/jwks"), Duration::from_secs(5))
            .await
            .expect("fetched");
        assert_eq!(set.keys.len(), 1);
        assert_eq!(set.keys[0].kid.as_deref(), Some("k1"));
    }

    /// A body past the cap is refused rather than buffered. An endpoint that
    /// streams forever must not be able to exhaust this process's memory.
    #[tokio::test]
    async fn an_oversized_document_is_refused() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;

            let (mut stream, _) = listener.accept().await.unwrap();
            drain_request(&mut stream).await;
            let _ = stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n",
                        MAX_JWKS_BYTES * 2
                    )
                    .as_bytes(),
                )
                .await;
            let _ = stream.write_all(&vec![b'x'; 4096]).await;
            let _ = stream.flush().await;
        });

        let error = fetch(&format!("http://{address}/jwks"), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cap"), "{error}");
    }

    /// A head that never ends is refused before it can be buffered.
    #[tokio::test]
    async fn an_endless_header_block_is_refused() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;

            let (mut stream, _) = listener.accept().await.unwrap();
            drain_request(&mut stream).await;
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n").await;
            for index in 0..4096 {
                if stream
                    .write_all(format!("x-filler-{index}: {}\r\n", "y".repeat(64)).as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });

        let error = fetch(&format!("http://{address}/jwks"), Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("headers exceeded"), "{error}");
    }
}
