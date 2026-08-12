//! Time-based one-time codes (RFC 6238), the enrolment flow around them, and
//! recovery codes.
//!
//! TOTP is six digits and a shared secret, and almost every implementation of
//! it has the same two holes:
//!
//! 1. **No replay prevention.** A code is valid for a whole period, so an
//!    attacker who watches one being typed — over a shoulder, through a
//!    phishing proxy, in a screen recording — has thirty seconds to use it.
//!    [`Totp::verify`] cannot fix that on its own, because it has nowhere to
//!    remember what it already accepted; [`TotpEnrollment`] can, and does.
//! 2. **A drift window that is quietly enormous.** "Accept ±5 periods" turns a
//!    thirty-second code into a five-and-a-half-minute one. The default here is
//!    ±1, which is ninety seconds, and widening it is a decision somebody has
//!    to write down.
//!
//! ```
//! use moso_auth::{Totp, TotpEnrollment};
//!
//! // Setup: show the URI as a QR code, keep the secret pending.
//! let mut enrolment = TotpEnrollment::start("Example", "ada@example.com")?;
//! assert!(enrolment.provisioning_uri().starts_with("otpauth://totp/"));
//! assert!(!enrolment.is_confirmed());
//!
//! // Confirm: the user types a code, proving the secret arrived.
//! let code = Totp::default().current(enrolment.secret())?;
//! assert!(enrolment.confirm(&code)?);
//! assert!(enrolment.is_confirmed());
//!
//! // And the same code cannot be used again.
//! assert!(!enrolment.check(&code)?);
//! # Ok::<(), moso_auth::Error>(())
//! ```
//!
//! # Where the remembered period lives
//!
//! [`TotpEnrollment`] holds the last accepted period in a field, which makes
//! the replay refusal last exactly as long as the value does. Something has to
//! write it back, and two callers do: the mounted `/auth/totp` routes, under
//! their own `ConfirmedTotp` namespace, and
//! [`DatabaseBackend::second_factor`](crate::DatabaseBackend::second_factor),
//! into the column the application names. Both write it *before* the login is
//! allowed to succeed, so a store that refuses the write refuses the login
//! rather than leaving the code usable for the rest of its window — which is
//! hole 1 above, reopened at the last moment.

use std::time::Duration;

use chrono::{DateTime, Utc};
use moso_core::config::SecretString;

use crate::jwks::{ct_eq, random_bytes, sha256_hex};
use crate::{Error, Result};

// ---------------------------------------------------------------------------
// base32
// ---------------------------------------------------------------------------

/// RFC 4648 base32, which is what every authenticator app speaks.
///
/// Written here rather than taken from a crate: it is forty lines, it is
/// specified in a table, and the decode side needs to be *forgiving* in a way a
/// general-purpose decoder is not — a user typing a secret off a screen adds
/// spaces, drops the padding and gets the case wrong, and none of those should
/// be an authentication failure.
mod base32 {
    /// The RFC 4648 alphabet.
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

    /// Encode, uppercase, without padding.
    pub(super) fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
        let mut buffer: u32 = 0;
        let mut bits: u32 = 0;
        for byte in bytes {
            buffer = (buffer << 8) | u32::from(*byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                let index = ((buffer >> bits) & 0x1f) as usize;
                out.push(char::from(ALPHABET[index]));
            }
        }
        if bits > 0 {
            let index = ((buffer << (5 - bits)) & 0x1f) as usize;
            out.push(char::from(ALPHABET[index]));
        }
        out
    }

    /// Decode, ignoring case, padding, spaces and hyphens.
    ///
    /// `None` when a character is not in the alphabet at all.
    pub(super) fn decode(text: &str) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(text.len() * 5 / 8);
        let mut buffer: u32 = 0;
        let mut bits: u32 = 0;
        for character in text.chars() {
            let value = match character {
                'A'..='Z' => u32::from(character as u8 - b'A'),
                'a'..='z' => u32::from(character as u8 - b'a'),
                '2'..='7' => u32::from(character as u8 - b'2') + 26,
                // What a human adds, and what the padding rule leaves behind.
                '=' | ' ' | '-' | '\t' | '\n' | '\r' => continue,
                _ => return None,
            };
            buffer = (buffer << 5) | value;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                // The mask keeps this inside a byte, so the conversion is
                // total; `try_from` says so without an `as` cast to audit.
                out.push(u8::try_from((buffer >> bits) & 0xff).unwrap_or_default());
            }
        }
        // Any leftover bits must be zero; a non-zero remainder means the input
        // was truncated mid-byte.
        if bits > 0 && (buffer & ((1 << bits) - 1)) != 0 {
            return None;
        }
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// TotpSecret
// ---------------------------------------------------------------------------

/// How many bytes a fresh secret carries. RFC 4226 § 4 recommends 160 bits.
const SECRET_BYTES: usize = 20;

/// The shortest secret this crate will accept from storage.
///
/// RFC 4226 requires at least 128 bits. A shorter one is either a truncated
/// value or somebody's idea of a test fixture, and both should fail loudly.
const MIN_SECRET_BYTES: usize = 16;

/// A TOTP shared secret.
///
/// ```
/// use moso_auth::TotpSecret;
///
/// let secret = TotpSecret::generate()?;
/// let uri = secret.provisioning_uri("Example", "ada@example.com");
/// assert!(uri.starts_with("otpauth://totp/Example:ada%40example.com?"));
/// # Ok::<(), moso_auth::Error>(())
/// ```
#[derive(Clone)]
pub struct TotpSecret(SecretString);

impl TotpSecret {
    /// A fresh secret: 160 bits, the RFC 4226 recommendation.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the system random generator fails.
    ///
    /// ```
    /// use moso_auth::TotpSecret;
    ///
    /// let secret = TotpSecret::generate()?;
    /// assert_eq!(secret.as_secret().expose().len(), 32, "160 bits in base32");
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn generate() -> Result<Self> {
        Ok(Self(SecretString::new(base32::encode(&random_bytes(
            SECRET_BYTES,
        )?))))
    }

    /// Read a secret back out of storage.
    ///
    /// Forgiving about case, padding and the spaces a user types; strict about
    /// length, because a short secret is a weak second factor and nothing about
    /// the failure mode would tell anybody.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the text is not base32 or decodes to fewer than 128 bits.
    ///
    /// ```
    /// use moso_auth::TotpSecret;
    ///
    /// let secret = TotpSecret::from_base32("gezd gnbv gy3t qojq gezd gnbv gy3t qojq")?;
    /// assert_eq!(secret.as_secret().expose(), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
    /// assert!(TotpSecret::from_base32("not base32!").is_err());
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn from_base32(text: &str) -> Result<Self> {
        let Some(bytes) = base32::decode(text) else {
            return Err(Error::Config(
                "a TOTP secret must be base32 (A–Z and 2–7)".into(),
            ));
        };
        if bytes.len() < MIN_SECRET_BYTES {
            return Err(Error::Config(
                format!(
                    "a TOTP secret must be at least {MIN_SECRET_BYTES} bytes (RFC 4226 § 4); \
                     this one decodes to {}",
                    bytes.len()
                )
                .into(),
            ));
        }
        Ok(Self(SecretString::new(base32::encode(&bytes))))
    }

    /// The `otpauth://` URI an authenticator app scans.
    ///
    /// The label is `issuer:account` and the `issuer` parameter repeats it,
    /// which is what the de-facto specification asks for: apps disagree about
    /// which one they read, and one without the other shows up as a nameless
    /// entry in somebody's authenticator.
    ///
    /// ```
    /// use moso_auth::TotpSecret;
    ///
    /// let secret = TotpSecret::generate()?;
    /// let uri = secret.provisioning_uri("Example Inc", "ada@example.com");
    /// assert!(uri.contains("issuer=Example%20Inc"));
    /// assert!(uri.contains("algorithm=SHA1&digits=6&period=30"));
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn provisioning_uri(&self, issuer: &str, account: &str) -> String {
        self.provisioning_uri_with(issuer, account, &Totp::default())
    }

    /// The `otpauth://` URI for a non-default [`Totp`].
    ///
    /// ```
    /// use moso_auth::{Totp, TotpSecret};
    ///
    /// let secret = TotpSecret::generate()?;
    /// let totp = Totp::default().with_digits(8)?;
    /// let uri = secret.provisioning_uri_with("Example", "ada@example.com", &totp);
    /// assert!(uri.contains("digits=8"));
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn provisioning_uri_with(&self, issuer: &str, account: &str, totp: &Totp) -> String {
        format!(
            "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={}&period={}",
            percent_encode(issuer),
            percent_encode(account),
            self.0.expose(),
            percent_encode(issuer),
            totp.digits(),
            totp.period().as_secs(),
        )
    }

    /// The base32 secret, for a user who types it in by hand.
    ///
    /// ```
    /// use moso_auth::TotpSecret;
    ///
    /// let secret = TotpSecret::generate()?;
    /// assert!(!secret.as_secret().expose().is_empty());
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn as_secret(&self) -> &SecretString {
        &self.0
    }

    /// The raw key bytes, for the HMAC.
    fn key_bytes(&self) -> Result<Vec<u8>> {
        base32::decode(self.0.expose())
            .ok_or_else(|| Error::Config("the stored TOTP secret is not valid base32".into()))
    }
}

impl core::fmt::Debug for TotpSecret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("TotpSecret(<redacted>)")
    }
}

/// Percent-encode everything that is not an RFC 3986 unreserved character.
///
/// Deliberately aggressive: an authenticator's label is displayed to a human and
/// parsed by a dozen different apps, and encoding `:`, `/`, `?`, `&`, `=` and
/// space is the difference between "Example Inc" and a broken entry.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(*byte));
        } else {
            out.push('%');
            out.push(
                char::from_digit(u32::from(byte >> 4), 16)
                    .unwrap_or('0')
                    .to_ascii_uppercase(),
            );
            out.push(
                char::from_digit(u32::from(byte & 0x0f), 16)
                    .unwrap_or('0')
                    .to_ascii_uppercase(),
            );
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Totp
// ---------------------------------------------------------------------------

/// The largest drift window this crate will build.
///
/// Three periods either side is ninety seconds of drift in each direction and
/// three and a half minutes of replay window. Anything past that is not clock
/// skew, it is a decision to weaken the factor.
const MAX_SKEW: u32 = 3;

/// Time-based one-time codes.
///
/// ```
/// use moso_auth::{Totp, TotpSecret};
///
/// let secret = TotpSecret::generate()?;
/// let totp = Totp::default();
/// let code = totp.current(&secret)?;
/// assert_eq!(code.len(), 6);
/// assert!(totp.verify(&secret, &code)?);
/// # Ok::<(), moso_auth::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Totp {
    /// How long a code is valid. Thirty seconds, universally.
    period: Duration,
    /// How many digits.
    digits: u32,
    /// How many periods either side to accept, for clock drift.
    ///
    /// One means a code is good for about ninety seconds. Larger values are a
    /// wider replay window, which is why this is small and not configurable
    /// upward without meaning it.
    skew: u32,
}

impl Default for Totp {
    fn default() -> Self {
        Self {
            period: Duration::from_secs(30),
            digits: 6,
            skew: 1,
        }
    }
}

impl Totp {
    /// How long a code is valid.
    ///
    /// ```
    /// assert_eq!(moso_auth::Totp::default().period().as_secs(), 30);
    /// ```
    #[must_use]
    pub fn period(&self) -> Duration {
        self.period
    }

    /// How many digits a code has.
    ///
    /// ```
    /// assert_eq!(moso_auth::Totp::default().digits(), 6);
    /// ```
    #[must_use]
    pub fn digits(&self) -> u32 {
        self.digits
    }

    /// How many periods either side are accepted.
    ///
    /// ```
    /// assert_eq!(moso_auth::Totp::default().skew(), 1);
    /// ```
    #[must_use]
    pub fn skew(&self) -> u32 {
        self.skew
    }

    /// Use a different period.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for a period outside 15–120
    /// seconds: shorter is unusable on a phone, longer is a replay window
    /// dressed up as convenience.
    ///
    /// ```
    /// use moso_auth::Totp;
    ///
    /// assert!(Totp::default().with_period(std::time::Duration::from_secs(60)).is_ok());
    /// assert!(Totp::default().with_period(std::time::Duration::from_secs(600)).is_err());
    /// ```
    pub fn with_period(mut self, period: Duration) -> Result<Self> {
        if !(15..=120).contains(&period.as_secs()) {
            return Err(Error::Config(
                "a TOTP period must be between 15 and 120 seconds; 30 is what every \
                 authenticator app assumes"
                    .into(),
            ));
        }
        self.period = period;
        Ok(self)
    }

    /// Use a different number of digits.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] outside 6–8. Below six is
    /// guessable; above eight does not fit in the 31 bits RFC 4226's dynamic
    /// truncation produces.
    ///
    /// ```
    /// use moso_auth::Totp;
    ///
    /// assert!(Totp::default().with_digits(8).is_ok());
    /// assert!(Totp::default().with_digits(4).is_err());
    /// ```
    pub fn with_digits(mut self, digits: u32) -> Result<Self> {
        if !(6..=8).contains(&digits) {
            return Err(Error::Config(
                "a TOTP code must be 6, 7 or 8 digits (RFC 4226 § 5.3)".into(),
            ));
        }
        self.digits = digits;
        Ok(self)
    }

    /// Use a different drift window.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] past three periods.
    ///
    /// ```
    /// use moso_auth::Totp;
    ///
    /// assert!(Totp::default().with_skew(2).is_ok());
    /// assert!(Totp::default().with_skew(10).is_err());
    /// ```
    pub fn with_skew(mut self, skew: u32) -> Result<Self> {
        if skew > MAX_SKEW {
            return Err(Error::Config(
                format!(
                    "a TOTP drift window of {skew} periods is a replay window, not clock skew; \
                     {MAX_SKEW} is the most this crate will build"
                )
                .into(),
            ));
        }
        self.skew = skew;
        Ok(self)
    }

    /// Check a code.
    ///
    /// **Replay is not solved here, and cannot be:** this function has nowhere
    /// to remember what it accepted. A code is valid for a whole period, so an
    /// attacker who observes one has thirty seconds to use it. Use
    /// [`TotpEnrollment`], which records the accepted period and refuses a
    /// second use — `moso auth`'s own routes do.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the secret does not decode.
    ///
    /// ```
    /// use moso_auth::{Totp, TotpSecret};
    ///
    /// let secret = TotpSecret::generate()?;
    /// let totp = Totp::default();
    /// assert!(totp.verify(&secret, &totp.current(&secret)?)?);
    /// assert!(!totp.verify(&secret, "000000")? || totp.current(&secret)? == "000000");
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn verify(&self, secret: &TotpSecret, code: &str) -> Result<bool> {
        Ok(self.matching_period(secret, code)?.is_some())
    }

    /// Check a code at a given instant, for a test or a replayed audit log.
    ///
    /// # Errors
    ///
    /// As [`verify`](Totp::verify).
    ///
    /// ```
    /// use moso_auth::{Totp, TotpSecret};
    ///
    /// let secret = TotpSecret::generate()?;
    /// let totp = Totp::default();
    /// let now = chrono::Utc::now();
    /// let code = totp.current_at(&secret, now)?;
    /// assert!(totp.verify_at(&secret, &code, now)?);
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn verify_at(&self, secret: &TotpSecret, code: &str, at: DateTime<Utc>) -> Result<bool> {
        Ok(self.matching_period_at(secret, code, at)?.is_some())
    }

    /// The current code, for a test or a provisioning preview.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the secret does not decode.
    ///
    /// ```
    /// use moso_auth::{Totp, TotpSecret};
    ///
    /// let code = Totp::default().current(&TotpSecret::generate()?)?;
    /// assert!(code.chars().all(|c| c.is_ascii_digit()));
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn current(&self, secret: &TotpSecret) -> Result<String> {
        self.current_at(secret, Utc::now())
    }

    /// The code for a given instant.
    ///
    /// # Errors
    ///
    /// As [`current`](Totp::current).
    ///
    /// ```
    /// use moso_auth::{Totp, TotpSecret};
    ///
    /// // RFC 6238's own test vector: the ASCII secret "12345678901234567890",
    /// // eight digits, at T = 59.
    /// let secret = TotpSecret::from_base32("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ")?;
    /// let totp = Totp::default().with_digits(8)?;
    /// let at = chrono::DateTime::from_timestamp(59, 0).unwrap();
    /// assert_eq!(totp.current_at(&secret, at)?, "94287082");
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn current_at(&self, secret: &TotpSecret, at: DateTime<Utc>) -> Result<String> {
        let key = secret.key_bytes()?;
        self.code_for_counter(&key, self.counter_at(at))
    }

    /// Which period a code belongs to, for replay tracking.
    ///
    /// `None` when the code matches no period in the window.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the secret does not decode.
    ///
    /// ```
    /// use moso_auth::{Totp, TotpSecret};
    ///
    /// let secret = TotpSecret::generate()?;
    /// let totp = Totp::default();
    /// let code = totp.current(&secret)?;
    /// assert!(totp.matching_period(&secret, &code)?.is_some());
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn matching_period(&self, secret: &TotpSecret, code: &str) -> Result<Option<u64>> {
        self.matching_period_at(secret, code, Utc::now())
    }

    /// Which period a code belongs to at a given instant.
    ///
    /// The window is walked from the current period outwards, and **every**
    /// candidate is compared even after a match, so the number of comparisons
    /// does not depend on which period matched. A loop that returns early
    /// leaks, through timing, how far off the client's clock is — which is not
    /// a secret worth much, but is free to protect.
    ///
    /// # Errors
    ///
    /// As [`matching_period`](Totp::matching_period).
    ///
    /// ```
    /// use moso_auth::{Totp, TotpSecret};
    ///
    /// let secret = TotpSecret::generate()?;
    /// let totp = Totp::default();
    /// let now = chrono::Utc::now();
    /// let code = totp.current_at(&secret, now)?;
    /// let period = totp.matching_period_at(&secret, &code, now)?.unwrap();
    /// assert_eq!(period, now.timestamp() as u64 / 30);
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn matching_period_at(
        &self,
        secret: &TotpSecret,
        code: &str,
        at: DateTime<Utc>,
    ) -> Result<Option<u64>> {
        let code = code.trim();
        if code.len() != self.digits as usize || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return Ok(None);
        }
        let key = secret.key_bytes()?;
        let centre = self.counter_at(at);
        let skew = i64::from(self.skew);

        let mut matched = None;
        for offset in -skew..=skew {
            let Some(counter) = centre.checked_add_signed(offset) else {
                continue;
            };
            let candidate = self.code_for_counter(&key, counter)?;
            if ct_eq(candidate.as_bytes(), code.as_bytes()) && matched.is_none() {
                matched = Some(counter);
            }
        }
        Ok(matched)
    }

    /// Which period an instant falls in.
    fn counter_at(&self, at: DateTime<Utc>) -> u64 {
        let seconds = at.timestamp().max(0).unsigned_abs();
        let period = self.period.as_secs().max(1);
        seconds / period
    }

    /// HOTP over one counter (RFC 4226 § 5.3).
    fn code_for_counter(&self, key: &[u8], counter: u64) -> Result<String> {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key);
        let tag = ring::hmac::sign(&key, &counter.to_be_bytes());
        let digest = tag.as_ref();

        // Dynamic truncation: the low nibble of the last byte picks a four-byte
        // window, and the top bit of that window is masked off so the result is
        // the same on a signed and an unsigned reading.
        let offset = usize::from(digest.last().copied().unwrap_or(0) & 0x0f);
        let Some(window) = digest.get(offset..offset + 4) else {
            return Err(Error::Config(
                "the HMAC was shorter than RFC 4226's truncation requires".into(),
            ));
        };
        let binary = (u32::from(window[0] & 0x7f) << 24)
            | (u32::from(window[1]) << 16)
            | (u32::from(window[2]) << 8)
            | u32::from(window[3]);

        let modulus = 10u32.pow(self.digits);
        let width = self.digits as usize;
        Ok(format!("{:0width$}", binary % modulus))
    }
}

// ---------------------------------------------------------------------------
// Enrolment
// ---------------------------------------------------------------------------

/// Where a second factor is in its lifecycle.
///
/// ```
/// use moso_auth::TotpState;
///
/// assert_ne!(TotpState::Pending, TotpState::Confirmed);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TotpState {
    /// The secret has been generated and shown, and the user has not yet proved
    /// it arrived. A pending enrolment must never satisfy a login: a secret the
    /// user never scanned would lock them out at the next sign-in.
    Pending,
    /// Confirmed and in use.
    Confirmed,
    /// Turned off. Kept as a state rather than a deleted row so that "was TOTP
    /// ever on for this account" is answerable.
    Disabled,
}

/// One account's TOTP enrolment: setup, confirm, disable, and the replay
/// prevention [`Totp`] cannot do on its own.
///
/// The three fields an application persists are [`secret`](Self::secret),
/// [`state`](Self::state) and [`last_period`](Self::last_period). The last one
/// is what makes a code single-use, and it is one integer.
///
/// ```
/// use moso_auth::{Totp, TotpEnrollment, TotpState};
///
/// let mut enrolment = TotpEnrollment::start("Example", "ada@example.com")?;
/// assert_eq!(enrolment.state(), TotpState::Pending);
///
/// // A wrong code does not confirm anything.
/// assert!(!enrolment.confirm("000000")? || enrolment.is_confirmed());
///
/// let code = Totp::default().current(enrolment.secret())?;
/// assert!(enrolment.confirm(&code)?);
///
/// enrolment.disable();
/// assert_eq!(enrolment.state(), TotpState::Disabled);
/// assert!(!enrolment.check(&code)?, "a disabled factor accepts nothing");
/// # Ok::<(), moso_auth::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct TotpEnrollment {
    /// The shared secret.
    secret: TotpSecret,
    /// The parameters codes are checked with.
    totp: Totp,
    /// The URI to render as a QR code.
    provisioning_uri: String,
    /// Where the enrolment is.
    state: TotpState,
    /// The last period accepted, so it cannot be accepted again.
    last_period: Option<u64>,
}

impl TotpEnrollment {
    /// Begin an enrolment: a fresh secret and the URI to show as a QR code.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the system random generator fails.
    ///
    /// ```
    /// use moso_auth::TotpEnrollment;
    ///
    /// let enrolment = TotpEnrollment::start("Example", "ada@example.com")?;
    /// assert!(!enrolment.is_confirmed());
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn start(issuer: &str, account: &str) -> Result<Self> {
        Self::start_with(issuer, account, Totp::default())
    }

    /// Begin an enrolment with non-default parameters.
    ///
    /// # Errors
    ///
    /// As [`start`](TotpEnrollment::start).
    ///
    /// ```
    /// use moso_auth::{Totp, TotpEnrollment};
    ///
    /// let enrolment =
    ///     TotpEnrollment::start_with("Example", "ada@example.com", Totp::default().with_digits(8)?)?;
    /// assert!(enrolment.provisioning_uri().contains("digits=8"));
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn start_with(issuer: &str, account: &str, totp: Totp) -> Result<Self> {
        let secret = TotpSecret::generate()?;
        let provisioning_uri = secret.provisioning_uri_with(issuer, account, &totp);
        Ok(Self {
            secret,
            totp,
            provisioning_uri,
            state: TotpState::Pending,
            last_period: None,
        })
    }

    /// Rebuild a confirmed enrolment from storage.
    ///
    /// `last_period` is the value [`last_period`](Self::last_period) returned
    /// when the row was written. Passing `None` re-opens exactly one replay
    /// window, which is why it is a parameter and not an `Option` somebody can
    /// forget.
    ///
    /// ```
    /// use moso_auth::{TotpEnrollment, TotpSecret};
    ///
    /// let secret = TotpSecret::generate()?;
    /// let enrolment = TotpEnrollment::resume(secret, Some(58_000_000));
    /// assert!(enrolment.is_confirmed());
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn resume(secret: TotpSecret, last_period: Option<u64>) -> Self {
        Self::resume_with(secret, last_period, Totp::default())
    }

    /// Rebuild a confirmed enrolment with non-default parameters.
    ///
    /// ```
    /// use moso_auth::{Totp, TotpEnrollment, TotpSecret};
    ///
    /// let enrolment =
    ///     TotpEnrollment::resume_with(TotpSecret::generate()?, None, Totp::default());
    /// assert!(enrolment.is_confirmed());
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn resume_with(secret: TotpSecret, last_period: Option<u64>, totp: Totp) -> Self {
        Self {
            secret,
            totp,
            provisioning_uri: String::new(),
            state: TotpState::Confirmed,
            last_period,
        }
    }

    /// The URI to render as a QR code. Empty for a resumed enrolment, which has
    /// nothing left to show.
    ///
    /// ```
    /// use moso_auth::TotpEnrollment;
    ///
    /// let enrolment = TotpEnrollment::start("Example", "ada@example.com")?;
    /// assert!(enrolment.provisioning_uri().starts_with("otpauth://totp/"));
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn provisioning_uri(&self) -> &str {
        &self.provisioning_uri
    }

    /// The secret, to store.
    ///
    /// ```
    /// use moso_auth::TotpEnrollment;
    ///
    /// let enrolment = TotpEnrollment::start("Example", "ada@example.com")?;
    /// assert_eq!(enrolment.secret().as_secret().expose().len(), 32);
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn secret(&self) -> &TotpSecret {
        &self.secret
    }

    /// Where the enrolment is.
    ///
    /// ```
    /// use moso_auth::{TotpEnrollment, TotpState};
    ///
    /// let enrolment = TotpEnrollment::start("Example", "ada@example.com")?;
    /// assert_eq!(enrolment.state(), TotpState::Pending);
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn state(&self) -> TotpState {
        self.state
    }

    /// Whether codes from this enrolment are accepted.
    ///
    /// ```
    /// use moso_auth::TotpEnrollment;
    ///
    /// assert!(!TotpEnrollment::start("Example", "ada@example.com")?.is_confirmed());
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn is_confirmed(&self) -> bool {
        self.state == TotpState::Confirmed
    }

    /// The last period accepted. Persist this; it is the replay prevention.
    ///
    /// ```
    /// use moso_auth::TotpEnrollment;
    ///
    /// assert_eq!(TotpEnrollment::start("Example", "a@b.c")?.last_period(), None);
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn last_period(&self) -> Option<u64> {
        self.last_period
    }

    /// The parameters codes are checked with.
    ///
    /// ```
    /// use moso_auth::TotpEnrollment;
    ///
    /// assert_eq!(TotpEnrollment::start("Example", "a@b.c")?.totp().digits(), 6);
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn totp(&self) -> &Totp {
        &self.totp
    }

    /// Confirm the enrolment with a code from the user's authenticator.
    ///
    /// This is what proves the secret actually reached a device. Until it does,
    /// the factor must not be switched on, or a mis-scanned QR code locks the
    /// account at the next sign-in.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the secret does not decode.
    ///
    /// ```
    /// use moso_auth::{Totp, TotpEnrollment};
    ///
    /// let mut enrolment = TotpEnrollment::start("Example", "ada@example.com")?;
    /// let code = Totp::default().current(enrolment.secret())?;
    /// assert!(enrolment.confirm(&code)?);
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn confirm(&mut self, code: &str) -> Result<bool> {
        self.confirm_at(code, Utc::now())
    }

    /// Confirm at a given instant.
    ///
    /// # Errors
    ///
    /// As [`confirm`](TotpEnrollment::confirm).
    ///
    /// ```no_run
    /// # use moso_auth::TotpEnrollment;
    /// # fn f(e: &mut TotpEnrollment, code: &str) -> moso_auth::Result<bool> {
    /// e.confirm_at(code, chrono::Utc::now())
    /// # }
    /// ```
    pub fn confirm_at(&mut self, code: &str, at: DateTime<Utc>) -> Result<bool> {
        if self.state == TotpState::Disabled {
            return Ok(false);
        }
        let Some(period) = self.accept(code, at)? else {
            return Ok(false);
        };
        let _ = period;
        self.state = TotpState::Confirmed;
        Ok(true)
    }

    /// Check a code at sign-in, consuming its period.
    ///
    /// Returns `false` for a wrong code, for a code from a period that has
    /// already been used, and for an enrolment that is not confirmed.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the secret does not decode.
    ///
    /// ```
    /// use moso_auth::{Totp, TotpEnrollment};
    ///
    /// let mut enrolment = TotpEnrollment::start("Example", "ada@example.com")?;
    /// let code = Totp::default().current(enrolment.secret())?;
    /// enrolment.confirm(&code)?;
    /// assert!(!enrolment.check(&code)?, "the confirming code is spent");
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn check(&mut self, code: &str) -> Result<bool> {
        self.check_at(code, Utc::now())
    }

    /// Check a code at a given instant.
    ///
    /// # Errors
    ///
    /// As [`check`](TotpEnrollment::check).
    ///
    /// ```no_run
    /// # use moso_auth::TotpEnrollment;
    /// # fn f(e: &mut TotpEnrollment, code: &str) -> moso_auth::Result<bool> {
    /// e.check_at(code, chrono::Utc::now())
    /// # }
    /// ```
    pub fn check_at(&mut self, code: &str, at: DateTime<Utc>) -> Result<bool> {
        if self.state != TotpState::Confirmed {
            return Ok(false);
        }
        Ok(self.accept(code, at)?.is_some())
    }

    /// Turn the factor off.
    ///
    /// ```
    /// use moso_auth::{TotpEnrollment, TotpState};
    ///
    /// let mut enrolment = TotpEnrollment::start("Example", "ada@example.com")?;
    /// enrolment.disable();
    /// assert_eq!(enrolment.state(), TotpState::Disabled);
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn disable(&mut self) {
        self.state = TotpState::Disabled;
    }

    /// Match a code and consume its period, or refuse it.
    ///
    /// The single-use rule: a period is accepted only if it is *newer* than the
    /// last one accepted. That covers the replay of the same code and the
    /// replay of an older code inside the drift window, which is the case
    /// implementations usually miss.
    fn accept(&mut self, code: &str, at: DateTime<Utc>) -> Result<Option<u64>> {
        let Some(period) = self.totp.matching_period_at(&self.secret, code, at)? else {
            return Ok(None);
        };
        if self.last_period.is_some_and(|last| period <= last) {
            tracing::warn!(
                target: "moso_auth::audit",
                event = "totp_replay",
                period,
                last_period = self.last_period,
                "a TOTP code from an already-used period was presented"
            );
            return Ok(None);
        }
        self.last_period = Some(period);
        Ok(Some(period))
    }
}

// ---------------------------------------------------------------------------
// Recovery codes
// ---------------------------------------------------------------------------

/// How many random bytes each recovery code carries. Six bytes is 48 bits,
/// which as base32 is ten characters shown as `xxxxx-xxxxx`.
const RECOVERY_CODE_BYTES: usize = 6;

/// The most codes a single call will generate.
const MAX_RECOVERY_CODES: usize = 32;

/// One-time recovery codes, for a lost authenticator.
///
/// Generated as a set, shown once, stored hashed. A used code is consumed.
///
/// # Why SHA-256 and not argon2
///
/// A recovery code is 48 bits from the system CSPRNG, not a password: there is
/// no dictionary to run against it, and an attacker who could brute-force it
/// could brute-force the argon2 version in the same number of guesses. What a
/// slow hash *would* buy is a second-factor path that runs on the blocking pool
/// and compares against ten hashes at 250 ms each. The stored form here is the
/// same as an API key's, for the same reason, and the entropy is what makes
/// that safe.
///
/// ```
/// use moso_auth::RecoveryCodes;
///
/// # async fn f() -> moso_auth::Result<()> {
/// let codes = RecoveryCodes::generate(10).await?;
/// assert_eq!(codes.plaintext().len(), 10);
/// assert_eq!(codes.hashes().len(), 10);
///
/// let presented = codes.plaintext()[3].expose().to_owned();
/// assert_eq!(RecoveryCodes::check(&presented, codes.hashes()).await?, Some(3));
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct RecoveryCodes {
    /// The codes, shown once and then unrecoverable.
    plaintext: Vec<SecretString>,
    /// What to store: one hash per code.
    hashes: Vec<String>,
}

impl RecoveryCodes {
    /// Generate `count` codes.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the system random
    /// generator fails, and [`Error::Config`] for a count
    /// of zero or above 32.
    ///
    /// ```
    /// use moso_auth::RecoveryCodes;
    ///
    /// # async fn f() -> moso_auth::Result<()> {
    /// let codes = RecoveryCodes::generate(10).await?;
    /// assert!(codes.plaintext()[0].expose().contains('-'));
    /// # Ok(()) }
    /// ```
    pub async fn generate(count: usize) -> Result<Self> {
        if count == 0 || count > MAX_RECOVERY_CODES {
            return Err(Error::Config(
                format!("a recovery-code set must hold between 1 and {MAX_RECOVERY_CODES} codes")
                    .into(),
            ));
        }
        let mut plaintext = Vec::with_capacity(count);
        let mut hashes = Vec::with_capacity(count);
        for _ in 0..count {
            let raw = base32::encode(&random_bytes(RECOVERY_CODE_BYTES)?);
            // Grouped for a human to read off a printout and type back.
            let code = match raw.split_at_checked(5) {
                Some((left, right)) => format!("{left}-{right}"),
                None => raw.clone(),
            };
            hashes.push(Self::hash_of(&code));
            plaintext.push(SecretString::new(code));
        }
        Ok(Self { plaintext, hashes })
    }

    /// The codes to show the user, once.
    ///
    /// ```
    /// use moso_auth::RecoveryCodes;
    ///
    /// # async fn f() -> moso_auth::Result<()> {
    /// let codes = RecoveryCodes::generate(2).await?;
    /// assert_eq!(codes.plaintext().len(), 2);
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn plaintext(&self) -> &[SecretString] {
        &self.plaintext
    }

    /// The hashes to store.
    ///
    /// ```
    /// use moso_auth::RecoveryCodes;
    ///
    /// # async fn f() -> moso_auth::Result<()> {
    /// let codes = RecoveryCodes::generate(2).await?;
    /// assert!(codes.hashes().iter().all(|hash| hash.len() == 64));
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn hashes(&self) -> &[String] {
        &self.hashes
    }

    /// The stored form of a code, normalised so that case, spacing and the
    /// grouping hyphen do not matter.
    ///
    /// ```
    /// use moso_auth::RecoveryCodes;
    ///
    /// assert_eq!(RecoveryCodes::hash_of("abcde-fghij"), RecoveryCodes::hash_of("ABCDEFGHIJ"));
    /// ```
    #[must_use]
    pub fn hash_of(code: &str) -> String {
        let normalised: String = code
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .map(|character| character.to_ascii_uppercase())
            .collect();
        sha256_hex(normalised.as_bytes())
    }

    /// Check a presented code against stored hashes, returning which matched.
    ///
    /// The caller deletes that hash: a recovery code is single-use, and a code
    /// that survives its use is a permanent bypass of the second factor.
    ///
    /// Every hash is compared even after a match, so the position of the code in
    /// the set is not observable through timing.
    ///
    /// # Errors
    ///
    /// Never, today. The signature is fallible because a store-backed
    /// implementation of the same check would be.
    ///
    /// ```
    /// use moso_auth::RecoveryCodes;
    ///
    /// # async fn f() -> moso_auth::Result<()> {
    /// let codes = RecoveryCodes::generate(4).await?;
    /// assert_eq!(RecoveryCodes::check("nope", codes.hashes()).await?, None);
    /// # Ok(()) }
    /// ```
    pub async fn check(code: &str, hashes: &[String]) -> Result<Option<usize>> {
        let presented = Self::hash_of(code);
        let mut found = None;
        for (index, stored) in hashes.iter().enumerate() {
            if ct_eq(presented.as_bytes(), stored.as_bytes()) && found.is_none() {
                found = Some(index);
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An instant, from a Unix timestamp.
    fn at(timestamp: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(timestamp, 0).expect("in range")
    }

    /// RFC 4648 § 10's base32 vectors, unpadded.
    #[test]
    fn base32_matches_rfc_4648() {
        for (bytes, encoded) in [
            (&b""[..], ""),
            (&b"f"[..], "MY"),
            (&b"fo"[..], "MZXQ"),
            (&b"foo"[..], "MZXW6"),
            (&b"foob"[..], "MZXW6YQ"),
            (&b"fooba"[..], "MZXW6YTB"),
            (&b"foobar"[..], "MZXW6YTBOI"),
        ] {
            assert_eq!(base32::encode(bytes), encoded, "encoding {bytes:?}");
            assert_eq!(
                base32::decode(encoded).as_deref(),
                Some(bytes),
                "decoding {encoded:?}"
            );
        }
    }

    /// A user typing a secret off a screen adds spaces, drops the padding and
    /// gets the case wrong. None of that is an authentication failure.
    #[test]
    fn base32_decoding_forgives_what_humans_type() {
        let canonical = base32::decode("MZXW6YTBOI").unwrap();
        for spelling in [
            "mzxw6ytboi",
            "MZXW 6YTB OI",
            "MZXW-6YTB-OI",
            "MZXW6YTBOI======",
            " MZXW6YTBOI ",
        ] {
            assert_eq!(
                base32::decode(spelling).as_deref(),
                Some(canonical.as_slice()),
                "{spelling}"
            );
        }
        assert!(
            base32::decode("MZXW6YTBO!").is_none(),
            "0 and 1 are not in the alphabet"
        );
        assert!(base32::decode("01234567").is_none());
    }

    /// RFC 6238's test vectors, which is the only way to know the algorithm is
    /// the algorithm and not something that merely round-trips with itself.
    #[test]
    fn totp_matches_the_rfc_6238_vectors() {
        // The RFC's SHA-1 seed: the ASCII string "12345678901234567890".
        let secret = TotpSecret::from_base32("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").unwrap();
        let totp = Totp::default().with_digits(8).unwrap();
        for (timestamp, expected) in [
            (59_i64, "94287082"),
            (1_111_111_109, "07081804"),
            (1_111_111_111, "14050471"),
            (1_234_567_890, "89005924"),
            (2_000_000_000, "69279037"),
        ] {
            assert_eq!(
                totp.current_at(&secret, at(timestamp)).unwrap(),
                expected,
                "at T = {timestamp}"
            );
        }
    }

    /// Six digits is the default, and the code is zero-padded rather than
    /// short — a five-character "code" is rejected by every client.
    #[test]
    fn codes_are_the_configured_width() {
        let secret = TotpSecret::generate().unwrap();
        for digits in 6..=8 {
            let totp = Totp::default().with_digits(digits).unwrap();
            for offset in 0..200 {
                let code = totp
                    .current_at(&secret, at(1_700_000_000 + offset * 30))
                    .unwrap();
                assert_eq!(code.len(), digits as usize, "{code}");
                assert!(code.bytes().all(|byte| byte.is_ascii_digit()), "{code}");
            }
        }
    }

    /// The drift window: ±1 period accepted, ±2 not.
    #[test]
    fn the_drift_window_is_one_period_by_default() {
        let secret = TotpSecret::generate().unwrap();
        let totp = Totp::default();
        let now = at(1_700_000_000);

        for offset in [-30, 0, 30] {
            let code = totp
                .current_at(&secret, at(1_700_000_000 + offset))
                .unwrap();
            assert!(
                totp.verify_at(&secret, &code, now).unwrap(),
                "a code {offset}s away must be accepted"
            );
        }
        for offset in [-60, 60, -300, 300] {
            let code = totp
                .current_at(&secret, at(1_700_000_000 + offset))
                .unwrap();
            assert!(
                !totp.verify_at(&secret, &code, now).unwrap(),
                "a code {offset}s away must not be"
            );
        }
    }

    /// A wider window is available, and is bounded.
    #[test]
    fn the_drift_window_is_configurable_within_reason() {
        let secret = TotpSecret::generate().unwrap();
        let wide = Totp::default().with_skew(2).unwrap();
        let now = at(1_700_000_000);
        let code = wide.current_at(&secret, at(1_700_000_060)).unwrap();
        assert!(wide.verify_at(&secret, &code, now).unwrap());

        let error = Totp::default().with_skew(10).unwrap_err();
        assert!(error.to_string().contains("replay window"), "{error}");
    }

    /// `matching_period` names the period, which is what replay prevention
    /// needs.
    #[test]
    fn the_matching_period_is_reported() {
        let secret = TotpSecret::generate().unwrap();
        let totp = Totp::default();
        let now = at(1_700_000_000);
        let code = totp.current_at(&secret, now).unwrap();
        assert_eq!(
            totp.matching_period_at(&secret, &code, now).unwrap(),
            Some(1_700_000_000 / 30)
        );

        let previous = totp.current_at(&secret, at(1_699_999_970)).unwrap();
        assert_eq!(
            totp.matching_period_at(&secret, &previous, now).unwrap(),
            Some(1_699_999_970 / 30),
            "a code from the previous period reports that period, not this one"
        );
    }

    /// Anything that is not a code of the right shape is refused before the
    /// HMAC runs.
    #[test]
    fn malformed_codes_are_refused() {
        let secret = TotpSecret::generate().unwrap();
        let totp = Totp::default();
        for code in [
            "",
            "12345",
            "1234567",
            "abcdef",
            "12345a",
            "-12345",
            &"9".repeat(1000),
        ] {
            assert!(!totp.verify(&secret, code).unwrap(), "accepted {code:?}");
        }
        // Whitespace around a correct code is a paste artefact, not an attack.
        let good = totp.current(&secret).unwrap();
        assert!(totp.verify(&secret, &format!("  {good} ")).unwrap());
    }

    /// A secret must be a secret. Both halves are checked: the alphabet and the
    /// length.
    #[test]
    fn a_secret_is_validated_on_the_way_in() {
        assert!(TotpSecret::from_base32("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").is_ok());
        assert!(
            TotpSecret::from_base32("GEZDGNBVGY3TQOJQGEZDGNBV").is_err(),
            "15 bytes is below RFC 4226's 128-bit floor"
        );
        assert!(TotpSecret::from_base32("not base32!").is_err());

        let error = TotpSecret::from_base32("GEZDGNBV").unwrap_err();
        assert!(error.to_string().contains("at least 16 bytes"), "{error}");
        assert!(TotpSecret::from_base32("").is_err());
    }

    /// A generated secret is 160 bits and never repeats.
    #[test]
    fn generated_secrets_are_160_bits_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..128 {
            let secret = TotpSecret::generate().unwrap();
            let text = secret.as_secret().expose().to_owned();
            assert_eq!(text.len(), 32, "160 bits is 32 base32 characters");
            assert_eq!(base32::decode(&text).unwrap().len(), SECRET_BYTES);
            assert!(seen.insert(text), "a secret repeated");
        }
    }

    /// The provisioning URI is what an authenticator app parses, so its shape
    /// is a contract with software nobody here controls.
    #[test]
    fn the_provisioning_uri_is_the_de_facto_shape() {
        let secret = TotpSecret::from_base32("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").unwrap();
        let uri = secret.provisioning_uri("Example Inc", "ada+test@example.com");
        assert!(uri.starts_with("otpauth://totp/Example%20Inc:ada%2Btest%40example.com?"));
        assert!(uri.contains("secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"));
        assert!(uri.contains("issuer=Example%20Inc"));
        assert!(uri.contains("algorithm=SHA1"));
        assert!(uri.contains("digits=6"));
        assert!(uri.contains("period=30"));
        // The characters that would end the label early must not survive raw.
        assert!(!uri["otpauth://totp/".len()..].contains(' '));
    }

    /// A `Debug` must never be a way to read the secret.
    #[test]
    fn the_secret_does_not_debug_print() {
        let secret = TotpSecret::generate().unwrap();
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "TotpSecret(<redacted>)");
        assert!(!rendered.contains(secret.as_secret().expose()));
    }

    /// The whole enrolment lifecycle, in the order the routes call it.
    #[test]
    fn the_enrolment_lifecycle_is_setup_confirm_use_disable() {
        let mut enrolment = TotpEnrollment::start("Example", "ada@example.com").unwrap();
        assert_eq!(enrolment.state(), TotpState::Pending);
        assert!(enrolment.provisioning_uri().contains("otpauth://totp/"));

        let now = at(1_700_000_000);
        let code = enrolment
            .totp()
            .current_at(enrolment.secret(), now)
            .unwrap();

        // A pending enrolment must not satisfy a login.
        assert!(
            !enrolment.check_at(&code, now).unwrap(),
            "an unconfirmed factor accepts nothing"
        );

        assert!(enrolment.confirm_at(&code, now).unwrap());
        assert_eq!(enrolment.state(), TotpState::Confirmed);
        assert_eq!(enrolment.last_period(), Some(1_700_000_000 / 30));

        // The next period's code works.
        let later = at(1_700_000_060);
        let next = enrolment
            .totp()
            .current_at(enrolment.secret(), later)
            .unwrap();
        assert!(enrolment.check_at(&next, later).unwrap());

        enrolment.disable();
        let after = at(1_700_000_120);
        let last = enrolment
            .totp()
            .current_at(enrolment.secret(), after)
            .unwrap();
        assert!(!enrolment.check_at(&last, after).unwrap());
    }

    /// The point of the whole module: a code works exactly once.
    #[test]
    fn a_code_cannot_be_used_twice() {
        let mut enrolment = TotpEnrollment::start("Example", "ada@example.com").unwrap();
        let now = at(1_700_000_000);
        let code = enrolment
            .totp()
            .current_at(enrolment.secret(), now)
            .unwrap();
        assert!(enrolment.confirm_at(&code, now).unwrap());
        assert!(!enrolment.check_at(&code, now).unwrap(), "replayed");
        assert!(
            !enrolment.check_at(&code, at(1_700_000_025)).unwrap(),
            "replayed later in the same period"
        );
        assert!(
            !enrolment.check_at(&code, at(1_700_000_045)).unwrap(),
            "replayed in the next period, still inside the drift window"
        );
    }

    /// The case implementations miss: an *older* code inside the drift window,
    /// after a newer one has been accepted.
    #[test]
    fn an_older_code_inside_the_window_is_refused_after_a_newer_one() {
        let secret = TotpSecret::generate().unwrap();
        let mut enrolment = TotpEnrollment::resume(secret.clone(), None);
        let totp = Totp::default();

        let older = totp.current_at(&secret, at(1_699_999_970)).unwrap();
        let newer = totp.current_at(&secret, at(1_700_000_000)).unwrap();
        assert_ne!(older, newer);

        assert!(enrolment.check_at(&newer, at(1_700_000_000)).unwrap());
        assert!(
            !enrolment.check_at(&older, at(1_700_000_000)).unwrap(),
            "the previous period is inside the drift window and must still be refused"
        );
    }

    /// A confirmed enrolment restored from storage keeps its replay guard.
    #[test]
    fn a_resumed_enrolment_keeps_the_replay_guard() {
        let secret = TotpSecret::generate().unwrap();
        let totp = Totp::default();
        let now = at(1_700_000_000);
        let code = totp.current_at(&secret, now).unwrap();

        let mut fresh = TotpEnrollment::resume(secret.clone(), None);
        assert!(fresh.check_at(&code, now).unwrap());

        let mut restored = TotpEnrollment::resume(secret, fresh.last_period());
        assert!(
            !restored.check_at(&code, now).unwrap(),
            "the persisted period must survive a restart"
        );
    }

    /// A wrong code does not confirm, and does not consume a period either.
    #[test]
    fn a_wrong_code_confirms_nothing() {
        let mut enrolment = TotpEnrollment::start("Example", "ada@example.com").unwrap();
        let now = at(1_700_000_000);
        assert!(!enrolment.confirm_at("000000", now).unwrap());
        assert_eq!(enrolment.state(), TotpState::Pending);
        assert_eq!(enrolment.last_period(), None);

        let code = enrolment
            .totp()
            .current_at(enrolment.secret(), now)
            .unwrap();
        assert!(enrolment.confirm_at(&code, now).unwrap());
    }

    /// A disabled factor cannot be re-confirmed by presenting a code; the flow
    /// is a fresh enrolment, with a fresh secret.
    #[test]
    fn a_disabled_factor_cannot_be_revived_with_a_code() {
        let mut enrolment = TotpEnrollment::start("Example", "ada@example.com").unwrap();
        let now = at(1_700_000_000);
        let code = enrolment
            .totp()
            .current_at(enrolment.secret(), now)
            .unwrap();
        enrolment.disable();
        assert!(!enrolment.confirm_at(&code, now).unwrap());
        assert_eq!(enrolment.state(), TotpState::Disabled);
    }

    /// Non-default parameters survive into the URI and the checking.
    #[test]
    fn an_eight_digit_enrolment_is_consistent_end_to_end() {
        let totp = Totp::default()
            .with_digits(8)
            .unwrap()
            .with_period(Duration::from_secs(60))
            .unwrap();
        let mut enrolment = TotpEnrollment::start_with("Example", "ada@example.com", totp).unwrap();
        assert!(enrolment.provisioning_uri().contains("digits=8"));
        assert!(enrolment.provisioning_uri().contains("period=60"));

        let now = at(1_700_000_000);
        let code = enrolment
            .totp()
            .current_at(enrolment.secret(), now)
            .unwrap();
        assert_eq!(code.len(), 8);
        assert!(enrolment.confirm_at(&code, now).unwrap());
    }

    /// The parameter bounds, each with a reason in the message.
    #[test]
    fn parameters_are_bounded() {
        assert!(Totp::default().with_digits(5).is_err());
        assert!(Totp::default().with_digits(9).is_err());
        assert!(Totp::default().with_period(Duration::from_secs(5)).is_err());
        assert!(
            Totp::default()
                .with_period(Duration::from_secs(3600))
                .is_err()
        );
        assert!(Totp::default().with_skew(3).is_ok());
        assert!(Totp::default().with_skew(4).is_err());
    }

    // -----------------------------------------------------------------------
    // Recovery codes
    // -----------------------------------------------------------------------

    /// Generation, shape, and the hash that gets stored.
    #[tokio::test]
    async fn recovery_codes_are_generated_and_hashed() {
        let codes = RecoveryCodes::generate(10).await.unwrap();
        assert_eq!(codes.plaintext().len(), 10);
        assert_eq!(codes.hashes().len(), 10);

        let mut seen = std::collections::HashSet::new();
        for (code, hash) in codes.plaintext().iter().zip(codes.hashes()) {
            let text = code.expose();
            assert_eq!(text.len(), 11, "xxxxx-xxxxx");
            assert_eq!(text.as_bytes()[5], b'-');
            assert_eq!(hash.len(), 64);
            assert_eq!(hash, &RecoveryCodes::hash_of(text));
            assert!(seen.insert(text.to_owned()), "a code repeated");
        }
    }

    /// A code is matched by position, so the caller knows which hash to delete.
    #[tokio::test]
    async fn a_recovery_code_matches_its_own_position() {
        let codes = RecoveryCodes::generate(8).await.unwrap();
        for (index, code) in codes.plaintext().iter().enumerate() {
            assert_eq!(
                RecoveryCodes::check(code.expose(), codes.hashes())
                    .await
                    .unwrap(),
                Some(index)
            );
        }
        assert_eq!(
            RecoveryCodes::check("aaaaa-bbbbb", codes.hashes())
                .await
                .unwrap(),
            None
        );
    }

    /// Somebody reading a code off a printout gets the case and the hyphen
    /// wrong. Neither should lock them out of their own recovery.
    #[tokio::test]
    async fn a_recovery_code_is_matched_however_it_is_typed() {
        let codes = RecoveryCodes::generate(3).await.unwrap();
        let code = codes.plaintext()[1].expose().to_owned();
        for spelling in [
            code.to_ascii_lowercase(),
            code.replace('-', ""),
            code.replace('-', " "),
            format!(" {code} "),
        ] {
            assert_eq!(
                RecoveryCodes::check(&spelling, codes.hashes())
                    .await
                    .unwrap(),
                Some(1),
                "{spelling}"
            );
        }
    }

    /// Deleting the hash is what makes a code single-use, and the caller is the
    /// one who does it — so this test is the documentation of that contract.
    #[tokio::test]
    async fn consuming_a_code_is_deleting_its_hash() {
        let codes = RecoveryCodes::generate(4).await.unwrap();
        let code = codes.plaintext()[2].expose().to_owned();
        let mut stored = codes.hashes().to_vec();

        let index = RecoveryCodes::check(&code, &stored).await.unwrap().unwrap();
        stored.remove(index);
        assert_eq!(RecoveryCodes::check(&code, &stored).await.unwrap(), None);
        assert_eq!(stored.len(), 3);
    }

    /// The set size is bounded at both ends.
    #[tokio::test]
    async fn the_recovery_code_count_is_bounded() {
        assert!(RecoveryCodes::generate(0).await.is_err());
        assert!(RecoveryCodes::generate(33).await.is_err());
        assert!(RecoveryCodes::generate(1).await.is_ok());
        assert!(RecoveryCodes::generate(32).await.is_ok());
    }

    /// The plaintext must not reach a log through `Debug`.
    #[tokio::test]
    async fn recovery_codes_do_not_debug_print() {
        let codes = RecoveryCodes::generate(2).await.unwrap();
        let rendered = format!("{codes:?}");
        for code in codes.plaintext() {
            assert!(!rendered.contains(code.expose()), "{rendered}");
        }
    }
}
