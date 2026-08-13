//! Second factors and passwordless: TOTP codes, recovery codes, passkeys,
//! [`MagicLink`], and the challenge that binds the two halves of a two-step
//! login.
//!
//! Passkeys matter disproportionately to the "modern framework" positioning:
//! very few backends ship WebAuthn turnkey, and the ceremonies are exactly the
//! kind of thing an application should not be writing by hand — a
//! clone-detection check that is subtly wrong is a credential that can be
//! duplicated.
//!
//! # What is defined here and what is re-exported
//!
//! Two credentials are defined here — [`MagicLink`] and
//! [`SecondFactorChallenge`], with [`SecondFactorChallenges`] as the store the
//! second one is claimed from. The two other second factors grew into modules of
//! their own — the RFC 6238 skew window, the enrolment state machine and the
//! recovery-code set are [`crate::totp`]; the four ceremonies, the
//! clone-detection counter and the discoverable flow are `crate::webauthn`
//! (behind the `passkeys` feature) —
//! and both are re-exported here so that `moso_auth::mfa::Totp` and
//! `moso_auth::mfa::WebAuthn` keep resolving. There is exactly one definition of
//! each type in the crate: a second `Totp` that merely looked like the first is
//! how two verification paths end up disagreeing about the skew window.
//!
//! ```
//! use moso_auth::mfa::{MagicLink, Totp, WebAuthn};
//!
//! // The same type, whichever path names it.
//! let _: fn(&Totp, &moso_auth::TotpSecret, &str) -> moso_auth::Result<bool> = Totp::verify;
//! let _: fn(&str) -> String = MagicLink::hash_of;
//! let _ = core::any::type_name::<WebAuthn>();
//! ```
//!
//! # One partial authentication, one mechanism
//!
//! A login that stops to ask for a code is two requests, and something has to
//! say that the second one belongs to the same account as the first. That
//! something is [`SecondFactorChallenge`], and it is meant to be the only such
//! thing in the crate: [`Error::SecondFactorRequired`] carries the token it
//! mints, and [`DatabaseBackend::authenticate`](crate::DatabaseBackend) is what
//! puts it there. Three properties make it safe, and all three are enforced by
//! [`SecondFactorChallenges`] rather than left to a route handler:
//!
//! | Property | How | Why it matters |
//! | --- | --- | --- |
//! | **Bound** | the stored value is the account's subject, and redemption hands it back for the caller to compare | otherwise a challenge earned against one account signs in another |
//! | **Expiring** | a stored `expires_at` **and** the store's own ttl, both checked | a partial authentication is a credential; one that never dies is a password with extra steps |
//! | **Single-use** | the claim is the store's `delete`, whose answer says whether *this* caller removed it | two requests racing one challenge must not both win |
//!
//! The token is never stored: the key is its SHA-256 and the value holds no
//! secret at all, so a dump of this namespace is a list of pending logins and
//! not a set of live ones.
//!
//! The mounted `POST /auth/login` still keeps a second, session-scoped copy of
//! this idea, which expires only when the session does. It is on the owed list
//! in `docs/03-batteries/30-auth.md` and delegating it here is a small change,
//! because the token a client receives and echoes is the same field either way.
//! Until it lands, read this type as the mechanism of record and that one as the
//! thing being replaced — not as two designs to choose between.

use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, TimeDelta, Utc};
use moso_core::config::SecretString;
use serde::{Deserialize, Serialize};

use crate::jwks::{random_bytes, sha256_hex};
use crate::{Error, Result};

// ---------------------------------------------------------------------------
// Re-exports: one definition per second factor
// ---------------------------------------------------------------------------

pub use crate::totp::{RecoveryCodes, Totp, TotpEnrollment, TotpSecret, TotpState};
#[cfg(feature = "passkeys")]
#[cfg_attr(docsrs, doc(cfg(feature = "passkeys")))]
pub use crate::webauthn::{PasskeyCredential, PasskeyStore, WebAuthn, WebAuthnChallenge};

// ---------------------------------------------------------------------------
// One-shot tokens
// ---------------------------------------------------------------------------

/// How many bytes of entropy every one-shot token in this module carries.
const ONE_SHOT_TOKEN_BYTES: usize = 32;

/// How many bytes of entropy a magic-link token carries.
///
/// The same 256 bits a session identifier gets, for the same reason: for the
/// lifetime of the link the token *is* the credential, so it has to be as
/// unguessable as the session it will become.
///
/// ```
/// assert_eq!(moso_auth::mfa::MAGIC_LINK_TOKEN_BYTES, 32);
/// ```
pub const MAGIC_LINK_TOKEN_BYTES: usize = ONE_SHOT_TOKEN_BYTES;

/// The base64url alphabet, without padding, that a token is written in.
///
/// Unpadded so the token survives a URL, a query string and an email client's
/// line wrapping without an `=` that something along the way will escape.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The three fields every one-shot token in this module has, written once.
///
/// [`MagicLink`] and [`SecondFactorChallenge`] are different credentials, with
/// different lifetimes and different stores, but the *token* inside them obeys
/// one rule: [`ONE_SHOT_TOKEN_BYTES`] from the operating system's generator,
/// shown once, kept only as a digest, dead at a fixed instant. Writing that rule
/// out twice is how one of the two quietly ends up with a shorter token or an
/// exclusive expiry boundary, so it is written here and both hold one.
#[derive(Debug)]
struct OneShot {
    /// The token to hand out. Shown once, and redacted by `Debug`.
    token: SecretString,
    /// The digest to store, and the key to look it up by.
    hash: String,
    /// When it stops working.
    expires_at: DateTime<Utc>,
}

impl OneShot {
    /// Mint one, or say why this `ttl` cannot produce a usable credential.
    ///
    /// `what` names the credential and `hint` is the sentence that tells the
    /// caller what a sensible ttl looks like, because "invalid ttl" on its own
    /// sends somebody to the source.
    fn mint(
        what: &'static str,
        hint: &'static str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<Self> {
        if ttl.is_zero() {
            return Err(Error::Config(
                format!("a {what} needs a non-zero ttl; {hint}").into(),
            ));
        }

        let too_long = || {
            Error::Config(
                format!("a {what} ttl has to fit in a timestamp; use hours, not aeons").into(),
            )
        };
        let delta = TimeDelta::from_std(ttl).map_err(|_| too_long())?;
        let expires_at = now.checked_add_signed(delta).ok_or_else(too_long)?;

        let token = B64.encode(random_bytes(ONE_SHOT_TOKEN_BYTES)?);
        let hash = sha256_hex(token.as_bytes());

        Ok(Self {
            token: SecretString::new(token),
            hash,
            expires_at,
        })
    }
}

// ---------------------------------------------------------------------------
// Magic links
// ---------------------------------------------------------------------------

/// A single-use sign-in link.
///
/// The password-reset mechanism used as a login: an address, a token, an email.
/// Its safety rests on four properties, all enforced here rather than left to a
/// route handler:
///
/// - short-lived (fifteen minutes is the usual `ttl`),
/// - single-use, consumed on redemption,
/// - stored hashed, so a leaked database is not a set of live sessions,
/// - and the *same* response whether or not the address exists, so it is not an
///   enumeration oracle.
///
/// The fourth is the caller's: issue the link, store the hash, and answer
/// "check your email" whether or not [`MagicLink::identity`] names a user that
/// exists.
///
/// ```
/// use moso_auth::MagicLink;
///
/// # async fn f() -> moso_auth::Result<()> {
/// let link = MagicLink::issue("ada@example.com", std::time::Duration::from_secs(900))?;
/// // Mail `link.token()`; store `link.hash()` and `link.expires_at()`.
/// assert_eq!(MagicLink::hash_of(link.token().expose()), link.hash());
/// assert!(!link.is_expired());
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct MagicLink {
    /// Who it is for.
    identity: String,
    /// The token, its digest and its expiry.
    one_shot: OneShot,
}

impl MagicLink {
    /// Issue a link for `identity`, valid for `ttl`.
    ///
    /// Synchronous, and deliberately so: the token is high-entropy, so its
    /// storage form is a SHA-256 rather than a password hash (the argument for
    /// a slow hash — that the input is guessable and the attacker's cost must
    /// be raised — does not apply), and a digest that costs microseconds has no
    /// business on the blocking pool.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the operating system's random generator
    /// fails, and [`Error::Config`] when `ttl` is zero or longer than a
    /// `chrono` timestamp can express — a link that has already expired is a
    /// support ticket, not a login.
    ///
    /// ```
    /// # use moso_auth::MagicLink;
    /// # fn f() -> moso_auth::Result<MagicLink> {
    /// MagicLink::issue("ada@example.com", std::time::Duration::from_secs(900))
    /// # }
    /// # assert!(f().is_ok());
    /// ```
    pub fn issue(identity: impl Into<String>, ttl: Duration) -> Result<Self> {
        Self::issue_at(identity, ttl, Utc::now())
    }

    /// [`MagicLink::issue`], with the clock supplied.
    ///
    /// What a test uses to assert on expiry without sleeping.
    ///
    /// # Errors
    ///
    /// As [`MagicLink::issue`].
    ///
    /// ```
    /// use chrono::{TimeZone as _, Utc};
    /// use moso_auth::MagicLink;
    ///
    /// # fn f() -> moso_auth::Result<()> {
    /// let issued = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    /// let link = MagicLink::issue_at("ada@example.com", std::time::Duration::from_secs(900), issued)?;
    ///
    /// assert_eq!(link.expires_at(), issued + chrono::TimeDelta::seconds(900));
    /// assert!(link.is_expired_at(link.expires_at()));
    /// # Ok(()) }
    /// # f().unwrap();
    /// ```
    pub fn issue_at(
        identity: impl Into<String>,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<Self> {
        Ok(Self {
            identity: identity.into(),
            one_shot: OneShot::mint(
                "magic link",
                "fifteen minutes is the usual choice",
                ttl,
                now,
            )?,
        })
    }

    /// The token to put in the URL.
    ///
    /// ```
    /// # use moso_auth::MagicLink;
    /// # use moso_core::config::SecretString;
    /// # fn f(l: &MagicLink) { let _: &SecretString = l.token(); }
    /// ```
    #[must_use]
    pub fn token(&self) -> &SecretString {
        &self.one_shot.token
    }

    /// The hash to store.
    ///
    /// ```
    /// # use moso_auth::MagicLink;
    /// # fn f(l: &MagicLink) { let _: &str = l.hash(); }
    /// ```
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.one_shot.hash
    }

    /// Who the link is for.
    ///
    /// ```
    /// # use moso_auth::MagicLink;
    /// # fn f(l: &MagicLink) { let _: &str = l.identity(); }
    /// ```
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// When the link stops working. Store it beside the hash.
    ///
    /// ```
    /// # use moso_auth::MagicLink;
    /// # use chrono::{DateTime, Utc};
    /// # fn f(l: &MagicLink) { let _: DateTime<Utc> = l.expires_at(); }
    /// ```
    #[must_use]
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.one_shot.expires_at
    }

    /// Whether the link has expired, by the system clock.
    ///
    /// ```
    /// # use moso_auth::MagicLink;
    /// # fn f() -> moso_auth::Result<()> {
    /// let link = MagicLink::issue("ada@example.com", std::time::Duration::from_secs(900))?;
    /// assert!(!link.is_expired());
    /// # Ok(()) }
    /// # f().unwrap();
    /// ```
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(Utc::now())
    }

    /// Whether the link has expired at `now`.
    ///
    /// The boundary is inclusive: a link is dead *at* its expiry, not one
    /// microsecond after it.
    ///
    /// ```
    /// # use moso_auth::MagicLink;
    /// # use chrono::{DateTime, Utc};
    /// # fn f(l: &MagicLink, at: DateTime<Utc>) -> bool { l.is_expired_at(at) }
    /// ```
    #[must_use]
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        now >= self.one_shot.expires_at
    }

    /// The hash of a presented token, for the lookup.
    ///
    /// The redemption path is: hash what arrived, look the hash up, check the
    /// expiry, delete the row, mint a session. Comparing hashes rather than
    /// tokens is what keeps a leaked table from being a set of live logins.
    ///
    /// ```
    /// use moso_auth::MagicLink;
    ///
    /// assert_eq!(MagicLink::hash_of("abc"), MagicLink::hash_of("abc"));
    /// assert_ne!(MagicLink::hash_of("abc"), MagicLink::hash_of("abd"));
    /// assert_eq!(MagicLink::hash_of("abc").len(), 64);
    /// ```
    #[must_use]
    pub fn hash_of(token: &str) -> String {
        sha256_hex(token.as_bytes())
    }
}

// ---------------------------------------------------------------------------
// The partial-authentication challenge
// ---------------------------------------------------------------------------

/// How long a password that checked out waits for its second factor.
///
/// Five minutes. Long enough to unlock a phone, open an authenticator and read
/// six digits off it; short enough that a challenge left on a shared machine is
/// dead before anybody walks back to it. It is deliberately far shorter than a
/// magic link's fifteen minutes, because a magic link is mailed and waited for
/// while this one is answered in the same sitting.
///
/// ```
/// assert_eq!(moso_auth::mfa::SECOND_FACTOR_TTL.as_secs(), 300);
/// ```
pub const SECOND_FACTOR_TTL: Duration = Duration::from_secs(300);

/// The token that binds the second request of a two-step login to the first.
///
/// Minted when a password verifies against an account that has a second factor,
/// carried to the client in [`Error::SecondFactorRequired`], and presented again
/// beside the code. It is a credential for as long as it lives — it stands in
/// for a verified password — so it is treated as one: 256 bits, stored as a
/// digest, and claimed exactly once.
///
/// The `subject` is what makes it *this* account's challenge. Redemption hands
/// it back rather than taking it as an argument, so the caller compares what the
/// store says against the account it just verified, and a challenge earned
/// against one identity cannot complete a login for another.
///
/// ```
/// use moso_auth::mfa::{SecondFactorChallenge, SECOND_FACTOR_TTL};
///
/// # fn f() -> moso_auth::Result<()> {
/// let challenge = SecondFactorChallenge::issue("usr_1", SECOND_FACTOR_TTL)?;
///
/// assert_eq!(challenge.subject(), "usr_1");
/// assert_eq!(
///     SecondFactorChallenge::hash_of(challenge.token().expose()),
///     challenge.hash()
/// );
/// assert!(!challenge.is_expired());
/// # Ok(()) }
/// # f().unwrap();
/// ```
#[derive(Debug)]
pub struct SecondFactorChallenge {
    /// The account the second request will sign in.
    subject: String,
    /// The token, its digest and its expiry.
    one_shot: OneShot,
}

impl SecondFactorChallenge {
    /// Mint a challenge for `subject`, valid for `ttl`.
    ///
    /// Minting does not store anything; [`SecondFactorChallenges::issue`] is
    /// what makes a challenge redeemable, and is what almost every caller wants.
    /// This constructor exists for an application keeping pending logins
    /// somewhere else.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the operating system's random generator
    /// fails, and [`Error::Config`] when `ttl` is zero or longer than a `chrono`
    /// timestamp can express.
    ///
    /// ```
    /// # use moso_auth::mfa::{SecondFactorChallenge, SECOND_FACTOR_TTL};
    /// # fn f() -> moso_auth::Result<SecondFactorChallenge> {
    /// SecondFactorChallenge::issue("usr_1", SECOND_FACTOR_TTL)
    /// # }
    /// # assert!(f().is_ok());
    /// ```
    pub fn issue(subject: impl Into<String>, ttl: Duration) -> Result<Self> {
        Self::issue_at(subject, ttl, Utc::now())
    }

    /// [`SecondFactorChallenge::issue`], with the clock supplied.
    ///
    /// # Errors
    ///
    /// As [`SecondFactorChallenge::issue`].
    ///
    /// ```
    /// use chrono::{TimeZone as _, Utc};
    /// use moso_auth::mfa::{SecondFactorChallenge, SECOND_FACTOR_TTL};
    ///
    /// # fn f() -> moso_auth::Result<()> {
    /// let issued = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    /// let challenge = SecondFactorChallenge::issue_at("usr_1", SECOND_FACTOR_TTL, issued)?;
    ///
    /// assert_eq!(challenge.expires_at(), issued + chrono::TimeDelta::seconds(300));
    /// assert!(challenge.is_expired_at(challenge.expires_at()));
    /// # Ok(()) }
    /// # f().unwrap();
    /// ```
    pub fn issue_at(subject: impl Into<String>, ttl: Duration, now: DateTime<Utc>) -> Result<Self> {
        Ok(Self {
            subject: subject.into(),
            one_shot: OneShot::mint(
                "second-factor challenge",
                "five minutes is the usual choice",
                ttl,
                now,
            )?,
        })
    }

    /// The account this challenge belongs to.
    ///
    /// ```
    /// # use moso_auth::mfa::SecondFactorChallenge;
    /// # fn f(c: &SecondFactorChallenge) { let _: &str = c.subject(); }
    /// ```
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// The token the client has to send back.
    ///
    /// ```
    /// # use moso_auth::mfa::SecondFactorChallenge;
    /// # use moso_core::config::SecretString;
    /// # fn f(c: &SecondFactorChallenge) { let _: &SecretString = c.token(); }
    /// ```
    #[must_use]
    pub fn token(&self) -> &SecretString {
        &self.one_shot.token
    }

    /// The digest the token is stored under.
    ///
    /// ```
    /// # use moso_auth::mfa::SecondFactorChallenge;
    /// # fn f(c: &SecondFactorChallenge) { let _: &str = c.hash(); }
    /// ```
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.one_shot.hash
    }

    /// When it stops working.
    ///
    /// ```
    /// # use chrono::{DateTime, Utc};
    /// # use moso_auth::mfa::SecondFactorChallenge;
    /// # fn f(c: &SecondFactorChallenge) { let _: DateTime<Utc> = c.expires_at(); }
    /// ```
    #[must_use]
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.one_shot.expires_at
    }

    /// Whether it has expired, by the system clock.
    ///
    /// ```
    /// # use moso_auth::mfa::{SecondFactorChallenge, SECOND_FACTOR_TTL};
    /// # fn f() -> moso_auth::Result<()> {
    /// assert!(!SecondFactorChallenge::issue("usr_1", SECOND_FACTOR_TTL)?.is_expired());
    /// # Ok(()) }
    /// # f().unwrap();
    /// ```
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(Utc::now())
    }

    /// Whether it has expired at `now`. The boundary is inclusive, as
    /// [`MagicLink::is_expired_at`]'s is.
    ///
    /// ```
    /// # use chrono::{DateTime, Utc};
    /// # use moso_auth::mfa::SecondFactorChallenge;
    /// # fn f(c: &SecondFactorChallenge, at: DateTime<Utc>) -> bool { c.is_expired_at(at) }
    /// ```
    #[must_use]
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        now >= self.one_shot.expires_at
    }

    /// The digest of a presented token, which is what it is looked up by.
    ///
    /// ```
    /// use moso_auth::mfa::SecondFactorChallenge;
    ///
    /// assert_eq!(SecondFactorChallenge::hash_of("abc").len(), 64);
    /// assert_ne!(
    ///     SecondFactorChallenge::hash_of("abc"),
    ///     SecondFactorChallenge::hash_of("abd")
    /// );
    /// ```
    #[must_use]
    pub fn hash_of(token: &str) -> String {
        sha256_hex(token.as_bytes())
    }
}

/// What a pending second factor looks like at rest.
///
/// No secret is in it, and that is the point rather than an accident:
/// [`SecretString`] refuses to serialise, so the pattern the rest of this crate
/// follows is a private stored form that holds only what a lookup needs. Here
/// the token is the *key* (as its digest) and the value is the account it
/// belongs to, so this namespace can be dumped without yielding one live
/// credential.
#[derive(Debug, Deserialize, Serialize)]
struct StoredPending {
    /// The account the second request will sign in.
    subject: String,
    /// When the challenge stops working.
    ///
    /// Stored as well as handed to the store's ttl, because a
    /// [`KvStore`](moso_kv::KvStore) is allowed not to support expiry and a
    /// challenge that outlived its window because the backend was a plain map
    /// would be the one failure this type exists to prevent.
    expires_at: DateTime<Utc>,
}

moso_kv::namespace! {
    /// A password that checked out, waiting for the code that finishes the
    /// login. `on_failure = fail`, because a store that answered "no such
    /// challenge" while it was down would refuse every second factor, and one
    /// that answered "here it is" would be worse.
    PendingSecondFactor: str => StoredPending, on_failure = fail;
}

/// The component a store failure here is reported under.
const CHALLENGE_STORE: &str = "second-factor challenge store";

/// Where pending second factors live: mint here, claim here, once.
///
/// Backed by [`moso_kv`], for the same reason the login throttle is: a partial
/// authentication minted on one process has to be redeemable on another, and a
/// per-process map turns "single-use" into "single-use per pod".
///
/// ```
/// use moso_auth::mfa::SecondFactorChallenges;
/// use moso_kv::Kv;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso_auth::Result<()> {
/// let pending = SecondFactorChallenges::new(Kv::in_memory("shop").expect("an in-memory kv"));
/// let challenge = pending.issue("usr_1").await?;
/// let token = challenge.token().expose().to_owned();
///
/// assert_eq!(pending.redeem(&token).await?.as_deref(), Some("usr_1"));
/// assert_eq!(pending.redeem(&token).await?, None, "claimed exactly once");
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct SecondFactorChallenges {
    /// Where the pending logins live.
    kv: moso_kv::Kv,
    /// How long one waits.
    ttl: Duration,
}

impl SecondFactorChallenges {
    /// A store over `kv`, with the [`SECOND_FACTOR_TTL`] window.
    ///
    /// ```
    /// use moso_auth::mfa::{SecondFactorChallenges, SECOND_FACTOR_TTL};
    /// use moso_kv::Kv;
    ///
    /// let pending = SecondFactorChallenges::new(Kv::in_memory("shop").expect("an in-memory kv"));
    /// assert_eq!(pending.lifetime(), SECOND_FACTOR_TTL);
    /// ```
    #[must_use]
    pub fn new(kv: moso_kv::Kv) -> Self {
        Self {
            kv,
            ttl: SECOND_FACTOR_TTL,
        }
    }

    /// Wait a different length of time.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_auth::mfa::SecondFactorChallenges;
    /// use moso_kv::Kv;
    ///
    /// let pending = SecondFactorChallenges::new(Kv::in_memory("shop").expect("an in-memory kv"))
    ///     .ttl(Duration::from_secs(120));
    /// assert_eq!(pending.lifetime(), Duration::from_secs(120));
    /// ```
    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// How long a challenge from this store lives.
    ///
    /// ```
    /// # use moso_auth::mfa::SecondFactorChallenges;
    /// # fn f(p: &SecondFactorChallenges) { let _ = p.lifetime(); }
    /// ```
    #[must_use]
    pub fn lifetime(&self) -> Duration {
        self.ttl
    }

    /// Mint a challenge for `subject` and store it, redeemable once.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the configured ttl cannot produce a usable
    /// challenge, and [`Error::Unavailable`] when the generator or the store
    /// refuses — never a challenge that was not written down, which would be a
    /// login nobody could finish.
    ///
    /// ```
    /// use moso_auth::mfa::SecondFactorChallenges;
    /// use moso_kv::Kv;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let pending = SecondFactorChallenges::new(Kv::in_memory("shop").expect("an in-memory kv"));
    /// assert_eq!(pending.issue("usr_1").await?.subject(), "usr_1");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn issue(&self, subject: &str) -> Result<SecondFactorChallenge> {
        self.issue_at(subject, Utc::now()).await
    }

    /// [`SecondFactorChallenges::issue`], with the clock supplied.
    ///
    /// # Errors
    ///
    /// As [`SecondFactorChallenges::issue`].
    ///
    /// ```
    /// use chrono::Utc;
    /// use moso_auth::mfa::SecondFactorChallenges;
    /// use moso_kv::Kv;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let pending = SecondFactorChallenges::new(Kv::in_memory("shop").expect("an in-memory kv"));
    /// let challenge = pending.issue_at("usr_1", Utc::now()).await?;
    /// assert!(!challenge.is_expired());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn issue_at(
        &self,
        subject: &str,
        now: DateTime<Utc>,
    ) -> Result<SecondFactorChallenge> {
        let challenge = SecondFactorChallenge::issue_at(subject, self.ttl, now)?;
        let stored = StoredPending {
            subject: subject.to_owned(),
            expires_at: challenge.expires_at(),
        };

        self.kv
            .set_ttl::<PendingSecondFactor>(challenge.hash(), &stored, self.ttl)
            .await
            .map_err(|error| store_failed("set pending", error))?;
        Ok(challenge)
    }

    /// Claim `token`, returning the account it was minted for.
    ///
    /// `Ok(None)` covers every way a token can fail to be a live challenge —
    /// never issued, already claimed, expired, or claimed by another request a
    /// microsecond earlier — because a caller must not be able to tell those
    /// apart. The caller then compares the returned subject against the account
    /// whose password it has just verified; anything else and the challenge was
    /// earned somewhere it does not apply.
    ///
    /// The claim *is* the delete. `delete` answers whether the key was there, so
    /// the request the store says removed it is the one allowed to proceed, and
    /// two requests racing one challenge cannot both win.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the store cannot be reached. Deliberately not
    /// degraded into `Ok(None)`: that would refuse every second factor during an
    /// outage while looking, to a client, exactly like a wrong code.
    ///
    /// ```
    /// use moso_auth::mfa::SecondFactorChallenges;
    /// use moso_kv::Kv;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let pending = SecondFactorChallenges::new(Kv::in_memory("shop").expect("an in-memory kv"));
    /// assert_eq!(pending.redeem("never-issued").await?, None);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn redeem(&self, token: &str) -> Result<Option<String>> {
        self.redeem_at(token, Utc::now()).await
    }

    /// [`SecondFactorChallenges::redeem`], with the clock supplied.
    ///
    /// # Errors
    ///
    /// As [`SecondFactorChallenges::redeem`].
    ///
    /// ```
    /// use chrono::{TimeDelta, Utc};
    /// use moso_auth::mfa::SecondFactorChallenges;
    /// use moso_kv::Kv;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let pending = SecondFactorChallenges::new(Kv::in_memory("shop").expect("an in-memory kv"));
    /// let challenge = pending.issue("usr_1").await?;
    /// let later = challenge.expires_at() + TimeDelta::seconds(1);
    ///
    /// assert_eq!(
    ///     pending.redeem_at(challenge.token().expose(), later).await?,
    ///     None,
    ///     "an expired challenge is not a login"
    /// );
    /// let _ = Utc::now();
    /// # Ok(())
    /// # }
    /// ```
    pub async fn redeem_at(&self, token: &str, now: DateTime<Utc>) -> Result<Option<String>> {
        let key = SecondFactorChallenge::hash_of(token);

        let Some(stored) = self
            .kv
            .get::<PendingSecondFactor>(&key)
            .await
            .map_err(|error| store_failed("get pending", error))?
        else {
            return Ok(None);
        };

        let claimed = self
            .kv
            .delete::<PendingSecondFactor>(&key)
            .await
            .map_err(|error| store_failed("delete pending", error))?;

        if !claimed || now >= stored.expires_at {
            return Ok(None);
        }
        Ok(Some(stored.subject))
    }
}

impl core::fmt::Debug for SecondFactorChallenges {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SecondFactorChallenges")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

/// A key-value failure, as a challenge failure.
///
/// Always [`Error::Unavailable`], never a decision. Written out rather than left
/// to `?` so that this module does not depend on the `From<moso_kv::Error>`
/// conversion in `error.rs` choosing the same component name — that one says
/// "session store", which would send an operator to the wrong dashboard.
fn store_failed(operation: &'static str, error: moso_kv::Error) -> Error {
    Error::Unavailable {
        component: CHALLENGE_STORE,
        detail: format!("{operation}: {error}"),
        source: Some(Box::new(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0)
            .single()
            .expect("a valid timestamp")
    }

    /// The four documented properties, one assertion each.
    #[test]
    fn an_issued_link_is_random_hashed_and_dated() {
        let ttl = Duration::from_secs(900);
        let first = MagicLink::issue_at("ada@example.com", ttl, at(1_700_000_000))
            .expect("the generator works");
        let second = MagicLink::issue_at("ada@example.com", ttl, at(1_700_000_000))
            .expect("the generator works");

        // Random: two links for the same address at the same instant differ.
        assert_ne!(first.token().expose(), second.token().expose());
        assert_ne!(first.hash(), second.hash());

        // Hashed: what is stored is the digest of what is mailed.
        assert_eq!(MagicLink::hash_of(first.token().expose()), first.hash());
        assert_eq!(first.hash().len(), 64);
        assert!(first.hash().bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(first.hash(), first.token().expose());

        // Dated, and the identity is carried through untouched.
        assert_eq!(first.expires_at(), at(1_700_000_900));
        assert_eq!(first.identity(), "ada@example.com");
    }

    /// 256 bits, base64url, no padding: it has to survive a URL.
    #[test]
    fn the_token_is_url_safe_and_full_entropy() {
        let link = MagicLink::issue("ada@example.com", Duration::from_secs(900))
            .expect("the generator works");
        let token = link.token().expose();

        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "{token}"
        );
        assert_eq!(
            B64.decode(token).expect("base64url").len(),
            MAGIC_LINK_TOKEN_BYTES
        );
    }

    /// The expiry boundary is inclusive, so a link is never valid at exactly
    /// the moment it dies.
    #[test]
    fn expiry_is_inclusive_at_the_boundary() {
        let link = MagicLink::issue_at(
            "ada@example.com",
            Duration::from_secs(900),
            at(1_700_000_000),
        )
        .expect("the generator works");

        assert!(!link.is_expired_at(at(1_700_000_899)));
        assert!(link.is_expired_at(at(1_700_000_900)));
        assert!(link.is_expired_at(at(1_700_000_901)));

        // `is_expired` is `is_expired_at(Utc::now())`, so a link issued against
        // a fixed past instant is dead by the system clock and a live one is
        // not. Both directions, so neither is a constant.
        assert!(link.is_expired(), "issued in the past, expired in the past");
        assert!(
            !MagicLink::issue("ada@example.com", Duration::from_secs(900))
                .expect("the generator works")
                .is_expired()
        );
    }

    /// A ttl that cannot produce a usable link is refused rather than issued.
    #[test]
    fn a_zero_or_absurd_ttl_is_a_configuration_error() {
        let zero = MagicLink::issue("ada@example.com", Duration::ZERO);
        assert!(matches!(zero, Err(Error::Config(_))), "{zero:?}");
        assert!(
            zero.unwrap_err().to_string().contains("non-zero ttl"),
            "the error names the fix"
        );

        let absurd = MagicLink::issue("ada@example.com", Duration::from_secs(u64::MAX));
        assert!(matches!(absurd, Err(Error::Config(_))), "{absurd:?}");
    }

    /// The token never reaches a log line through `Debug`.
    #[test]
    fn debug_redacts_the_token() {
        let link = MagicLink::issue("ada@example.com", Duration::from_secs(900))
            .expect("the generator works");
        let rendered = format!("{link:?}");

        assert!(
            !rendered.contains(link.token().expose()),
            "the token leaked: {rendered}"
        );
    }

    /// `hash_of` is what the redemption path compares, so it has to agree with
    /// what `issue` stored — byte for byte, and only for the same token.
    #[test]
    fn the_lookup_hash_matches_only_the_token_that_was_issued() {
        let link = MagicLink::issue("ada@example.com", Duration::from_secs(900))
            .expect("the generator works");

        assert_eq!(MagicLink::hash_of(link.token().expose()), link.hash());
        assert_ne!(
            MagicLink::hash_of(&format!("{}x", link.token().expose())),
            link.hash()
        );
        // Unlike a recovery code, a magic-link token is exact: it is copied
        // from a URL, never typed, so nothing is normalised away.
        assert_ne!(
            MagicLink::hash_of(&link.token().expose().to_ascii_uppercase()),
            MagicLink::hash_of(link.token().expose()),
            "a token differing only in case is a different token"
        );
    }

    // ── the partial-authentication challenge ──────────────────────────────

    /// A store over a fresh in-memory key-value store, which is a real store.
    fn pending() -> SecondFactorChallenges {
        SecondFactorChallenges::new(
            moso_kv::Kv::in_memory("second-factor-test").expect("an in-memory kv"),
        )
    }

    /// The three properties the whole mechanism rests on, one at a time. This
    /// one: the challenge names the account, and redemption says which.
    #[tokio::test]
    async fn a_challenge_is_bound_to_the_account_it_was_minted_for() {
        let pending = pending();

        let ada = pending.issue("usr_ada").await.expect("issued");
        let grace = pending.issue("usr_grace").await.expect("issued");

        assert_eq!(ada.subject(), "usr_ada");
        assert_ne!(
            ada.token().expose(),
            grace.token().expose(),
            "two challenges minted at the same instant must differ"
        );

        assert_eq!(
            pending
                .redeem(ada.token().expose())
                .await
                .expect("claimed")
                .as_deref(),
            Some("usr_ada"),
            "the store hands back whose challenge it was, so the caller can compare"
        );
        assert_eq!(
            pending
                .redeem(grace.token().expose())
                .await
                .expect("claimed")
                .as_deref(),
            Some("usr_grace"),
            "and one account's challenge never answers with another's subject"
        );
    }

    /// The second: it is claimed once, however many requests race for it.
    #[tokio::test]
    async fn a_challenge_is_claimed_exactly_once() {
        let pending = pending();
        let challenge = pending.issue("usr_ada").await.expect("issued");
        let token = challenge.token().expose().to_owned();

        assert_eq!(
            pending.redeem(&token).await.expect("claimed").as_deref(),
            Some("usr_ada")
        );
        for _ in 0..3 {
            assert_eq!(
                pending.redeem(&token).await.expect("claimed"),
                None,
                "a replayed challenge is not a login"
            );
        }
    }

    /// The third: it dies, and the boundary is the inclusive one.
    #[tokio::test]
    async fn an_expired_challenge_cannot_be_claimed() {
        let pending = pending();
        let issued = at(1_700_000_000);
        let challenge = pending.issue_at("usr_ada", issued).await.expect("issued");

        assert_eq!(challenge.expires_at(), at(1_700_000_300));
        assert_eq!(
            pending
                .redeem_at(challenge.token().expose(), challenge.expires_at())
                .await
                .expect("claimed"),
            None,
            "dead at its expiry, not one microsecond after it"
        );

        // …and the claim consumed it either way, so a client that waits out the
        // window and retries does not get a second attempt at the same token.
        let fresh = pending.issue_at("usr_ada", issued).await.expect("issued");
        assert_eq!(
            pending
                .redeem_at(fresh.token().expose(), at(1_700_000_299))
                .await
                .expect("claimed")
                .as_deref(),
            Some("usr_ada"),
            "one second before, it still works"
        );
    }

    /// A token never reaches the store: the key is its digest and the value
    /// holds no secret, so a dump of the namespace is a list of pending logins
    /// rather than a set of live ones.
    #[tokio::test]
    async fn the_token_is_never_what_is_written_down() {
        let pending = pending();
        let challenge = pending.issue("usr_ada").await.expect("issued");
        let token = challenge.token().expose();

        assert_eq!(challenge.hash(), SecondFactorChallenge::hash_of(token));
        assert_ne!(challenge.hash(), token);

        let key = pending
            .kv
            .key::<PendingSecondFactor>(challenge.hash())
            .expect("a short key");
        assert!(!key.as_str().contains(token), "{}", key.as_str());

        let stored = pending
            .kv
            .get::<PendingSecondFactor>(challenge.hash())
            .await
            .expect("read")
            .expect("stored");
        assert_eq!(stored.subject, "usr_ada");
        assert!(!format!("{stored:?}").contains(token));
        assert!(!format!("{challenge:?}").contains(token), "nor does Debug");
    }

    /// Fail closed. An unreachable store must not read as "no such challenge",
    /// which would look to a client exactly like a wrong code and would hide an
    /// outage behind a refused login.
    #[tokio::test]
    async fn an_unreachable_store_is_unavailable_and_never_a_refusal() {
        let kv = moso_kv::Kv::builder("second-factor-test")
            .store(crate::throttle::tests::DownStore)
            .build()
            .expect("built");
        let pending = SecondFactorChallenges::new(kv);

        for outcome in [
            pending.issue("usr_ada").await.map(|_| ()),
            pending.redeem("whatever").await.map(|_| ()),
        ] {
            match outcome {
                Err(Error::Unavailable { component, .. }) => {
                    assert_eq!(component, CHALLENGE_STORE);
                }
                Err(other) => panic!("the wrong error: {other}"),
                Ok(()) => panic!("an unreachable store must not decide"),
            }
        }
    }

    /// A ttl that cannot produce a usable challenge is refused rather than
    /// issued — the same rule a magic link is held to, because it is the same
    /// code.
    #[test]
    fn a_zero_challenge_ttl_is_a_configuration_error() {
        let zero = SecondFactorChallenge::issue("usr_ada", Duration::ZERO);
        assert!(matches!(zero, Err(Error::Config(_))), "{zero:?}");
        let message = zero.unwrap_err().to_string();
        assert!(message.contains("second-factor challenge"), "{message}");
        assert!(message.contains("non-zero ttl"), "{message}");
    }

    /// The re-exports are the crate's own types and not copies of them.
    #[test]
    fn the_re_exports_are_the_one_definition() {
        fn same<T: ?Sized>(_: &T, _: &T) {}

        let totp = Totp::default();
        same(&totp, &crate::totp::Totp::default());
        assert_eq!(
            core::any::TypeId::of::<TotpSecret>(),
            core::any::TypeId::of::<crate::totp::TotpSecret>()
        );
        #[cfg(feature = "passkeys")]
        assert_eq!(
            core::any::TypeId::of::<WebAuthn>(),
            core::any::TypeId::of::<crate::webauthn::WebAuthn>()
        );
        assert_eq!(
            core::any::TypeId::of::<RecoveryCodes>(),
            core::any::TypeId::of::<crate::totp::RecoveryCodes>()
        );
    }
}
