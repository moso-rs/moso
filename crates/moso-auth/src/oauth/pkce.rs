//! PKCE, and the random values an authorization request is built from.
//!
//! PKCE (RFC 7636) exists because an authorization code travelling back through
//! a browser can be stolen — by a malicious app claiming the same custom URL
//! scheme, by a referrer leak, by a redirect that logs its query string. The
//! code alone is then enough to get a token. PKCE makes the code useless
//! without a secret that never left the server.
//!
//! Two things are structural here rather than optional:
//!
//! 1. **There is no way to omit it.** [`Pkce::generate`] is the only
//!    constructor, and [`Provider::authorize`](crate::Provider::authorize) calls
//!    it unconditionally — including for a confidential client, where the
//!    current OAuth 2.1 draft requires it anyway.
//! 2. **The method is always `S256`.** `plain` is in the RFC and is worth
//!    nothing: the verifier and the challenge are the same string, so anybody
//!    who sees the challenge can complete the exchange. A provider that only
//!    speaks `plain` is a provider whose PKCE does nothing, and downgrading to
//!    it silently is exactly the attack.

use base64::Engine as _;
use moso_core::config::SecretString;
use sha2::{Digest as _, Sha256};

use crate::{Error, Result};

/// URL-safe base64 without padding — RFC 7636 §4.2's encoding.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// How many random bytes a verifier is built from.
///
/// Thirty-two bytes is 256 bits of entropy and encodes to 43 characters, the
/// minimum the RFC allows and the length every large provider uses.
const VERIFIER_BYTES: usize = 32;

/// A PKCE verifier and its challenge.
///
/// Always S256; `plain` is not offered, because a downgrade to `plain` is the
/// one thing PKCE has to defend against.
///
/// ```
/// use moso_auth::Pkce;
///
/// let pkce = Pkce::generate();
/// assert_eq!(pkce.method(), "S256");
/// ```
pub struct Pkce {
    /// The verifier, which never leaves the server.
    verifier: SecretString,
    /// The challenge, which does.
    challenge: String,
}

impl Pkce {
    /// A fresh verifier and its challenge.
    ///
    /// # Panics
    ///
    /// If the operating system's random generator fails. There is no useful
    /// recovery from that — every credential this crate mints depends on it —
    /// and returning a `Result` that every caller unwraps would only move the
    /// panic.
    ///
    /// ```
    /// use moso_auth::Pkce;
    ///
    /// let a = Pkce::generate();
    /// let b = Pkce::generate();
    /// assert_ne!(a.challenge(), b.challenge());
    /// ```
    #[must_use]
    pub fn generate() -> Self {
        let verifier = B64.encode(random_bytes::<VERIFIER_BYTES>());
        let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier: SecretString::new(verifier),
            challenge,
        }
    }

    /// Rebuild from a verifier that was stored in the session.
    ///
    /// The exchange half of the flow: the verifier came back out of the
    /// session, and the challenge is recomputed rather than stored, so a
    /// tampered session cannot present a verifier that does not match.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`] when the verifier is not between 43 and 128
    /// characters of the RFC's unreserved set. A verifier outside that range is
    /// not something this crate minted.
    ///
    /// ```
    /// use moso_auth::Pkce;
    ///
    /// let original = Pkce::generate();
    /// let restored = Pkce::from_verifier(original.verifier().expose())?;
    /// assert_eq!(original.challenge(), restored.challenge());
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn from_verifier(verifier: &str) -> Result<Self> {
        if !(43..=128).contains(&verifier.len()) {
            return Err(Error::Ceremony {
                ceremony: "oauth",
                reason: std::borrow::Cow::Borrowed(
                    "the PKCE verifier in the session is not 43-128 characters, so it was not \
                     issued by this application",
                ),
            });
        }
        if !verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
        {
            return Err(Error::Ceremony {
                ceremony: "oauth",
                reason: std::borrow::Cow::Borrowed(
                    "the PKCE verifier in the session contains characters RFC 7636 does not allow",
                ),
            });
        }

        Ok(Self {
            challenge: B64.encode(Sha256::digest(verifier.as_bytes())),
            verifier: SecretString::new(verifier),
        })
    }

    /// The verifier, for the token exchange.
    ///
    /// ```
    /// use moso_auth::Pkce;
    ///
    /// let pkce = Pkce::generate();
    /// assert_eq!(pkce.verifier().expose().len(), 43);
    /// ```
    #[must_use]
    pub fn verifier(&self) -> &SecretString {
        &self.verifier
    }

    /// The challenge, for the authorization URL.
    ///
    /// ```
    /// use moso_auth::Pkce;
    ///
    /// assert_eq!(Pkce::generate().challenge().len(), 43);
    /// ```
    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// The challenge method. Always `"S256"`.
    ///
    /// ```
    /// use moso_auth::Pkce;
    ///
    /// assert_eq!(Pkce::generate().method(), "S256");
    /// ```
    #[must_use]
    pub const fn method(&self) -> &'static str {
        "S256"
    }
}

impl core::fmt::Debug for Pkce {
    /// The verifier is a credential and the challenge identifies it, so neither
    /// belongs in a log line.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Pkce(S256, <redacted>)")
    }
}

/// `N` bytes from the operating system's random generator.
///
/// # Panics
///
/// If the generator fails. See [`Pkce::generate`].
pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).expect(
        "the operating system's random generator failed; every credential this crate mints \
         depends on it, so there is nothing safe to continue with",
    );
    bytes
}

/// A fresh opaque token, URL-safe base64 of 32 random bytes.
///
/// What `state` and `nonce` are made of. Both are single-use values compared
/// for equality, so their only requirement is that an attacker cannot guess one.
pub(crate) fn random_token() -> String {
    B64.encode(random_bytes::<32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The challenge must be the S256 of the verifier and nothing else — a
    /// provider computes the same hash, and a mismatch is a flow that always
    /// fails at the last step.
    #[test]
    fn the_challenge_is_the_s256_of_the_verifier() {
        let pkce = Pkce::generate();
        let expected = B64.encode(Sha256::digest(pkce.verifier().expose().as_bytes()));
        assert_eq!(pkce.challenge(), expected);
    }

    /// RFC 7636's own test vector, so the encoding is checked against the
    /// specification rather than against itself.
    #[test]
    fn the_rfc_7636_test_vector_matches() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let pkce = Pkce::from_verifier(verifier).expect("the RFC's verifier is valid");
        assert_eq!(
            pkce.challenge(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    /// Two verifiers must never collide; a repeated one would let a stolen code
    /// be redeemed.
    #[test]
    fn verifiers_do_not_repeat() {
        let a = Pkce::generate();
        let b = Pkce::generate();
        assert_ne!(a.verifier().expose(), b.verifier().expose());
        assert_ne!(a.challenge(), b.challenge());
    }

    /// The lengths RFC 7636 §4.1 requires.
    #[test]
    fn the_verifier_is_within_the_rfcs_range() {
        let pkce = Pkce::generate();
        let length = pkce.verifier().expose().len();
        assert!((43..=128).contains(&length), "43 <= {length} <= 128");
        assert!(
            pkce.verifier()
                .expose()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')),
            "the verifier must be from the unreserved set"
        );
    }

    /// A verifier that did not come from here is refused rather than hashed
    /// into a challenge that will never match.
    #[test]
    fn a_malformed_verifier_is_refused() {
        for bad in [
            "",
            "too-short",
            &"x".repeat(129),
            &format!("{}!", "a".repeat(43)),
        ] {
            let error =
                Pkce::from_verifier(bad).expect_err("a malformed verifier must not be accepted");
            assert!(matches!(
                error,
                Error::Ceremony {
                    ceremony: "oauth",
                    ..
                }
            ));
        }
    }

    /// `Debug` must not print the verifier.
    #[test]
    fn debug_redacts() {
        let pkce = Pkce::generate();
        let printed = format!("{pkce:?}");
        assert!(!printed.contains(pkce.verifier().expose()));
        assert!(!printed.contains(pkce.challenge()));
        assert!(printed.contains("S256"));
    }

    /// The state and nonce generator has to produce distinct, URL-safe values:
    /// `state` travels in a query string and is compared byte for byte.
    #[test]
    fn tokens_are_distinct_and_url_safe() {
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        );
    }
}
