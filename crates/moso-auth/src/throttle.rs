//! Rate limiting and lockout for the credential endpoints.
//!
//! Two quotas, and the second one is the interesting one:
//!
//! - **Per address**, so one attacker cannot try many accounts.
//! - **Per identity**, so many attackers cannot try one account — *with
//!   exponential backoff rather than a hard lock*. A hard lock is itself an
//!   attack: anybody who knows a victim's address can lock them out by failing
//!   five logins. Backoff slows an attacker to nothing and leaves the victim
//!   able to sign in after a wait.
//!
//! Applied by default to `login`, `register`, `password/forgot`,
//! `verify-email/resend` and every `totp` route.
//!
//! # The per-address quota is `moso-kv`'s, not a second one
//!
//! [`Kv::rate_limit`](moso_kv::Kv::rate_limit) already implements GCRA — one
//! timestamp per bucket, one atomic operation per attempt, no window to fall
//! across — so the address tier is a [`RateQuota`] handed to it. A limiter
//! written here would be a second implementation of the same algorithm with its
//! own bugs, and the two would disagree the first time either was tuned.
//!
//! # Where the state lives
//!
//! Everything is in [`moso_kv`], so the quota is the same one whether the
//! deployment is one process or thirty. Six keys, all under the `throttle_`
//! stem inside the `moso:v1:<app>:` prefix `moso-kv` already gives every key,
//! so two applications sharing one Redis cannot see each other's counters.
//!
//! | Key | What it holds | How long it lives |
//! | --- | --- | --- |
//! | `moso-kv`'s own `rate` bucket | the GCRA arrival time for one address | the quota's own |
//! | `throttle_failures` | consecutive failures against one identity | refreshed to `max(per_identity_max, notify_window)` on each failure |
//! | `throttle_last_failure` | when the last of them was, in microseconds | the same |
//! | `throttle_window` | failures inside the notification window | [`ThrottleConfig::notify_window`], from the *first* failure in it |
//! | `throttle_notified` | the once-per-window notification marker | [`ThrottleConfig::notify_window`], from the notification |
//! | `throttle_attempts` | the last [`ATTEMPT_HISTORY`] attempts, newest first | [`ATTEMPT_RETENTION`] |
//!
//! # An identity is never a key
//!
//! A key can leak in ways a value cannot: a `SCAN` over a shared Redis, a
//! backend error message that quotes the key it failed on, a slow-log entry. So
//! every per-identity key is the lowercase hex SHA-256 of the *normalised*
//! identity, and the address bucket is the digest of the address. The plaintext
//! address is kept only inside the [`AttemptRecord`] value, which is what the
//! account's own security page renders.
//!
//! Hashing an address is a **key-shape** decision and not anonymisation: a
//! 32-bit address space is exhaustible in seconds. What it buys is a segment of
//! fixed, known length, so a hostile or very long `AuthCtx::ip` cannot push a
//! key past `moso-kv`'s length limit and turn a throttle into an error.
//!
//! # Fail-closed
//!
//! Every namespace here declares `on_failure = fail`, and every store call maps
//! its failure onto [`Error::Unavailable`]. A limiter that stops limiting when
//! its store blinks is a limiter an attacker can remove by making the store
//! blink; the caller must treat an `Err` from [`LoginThrottle::check`] as a
//! refusal, never as an [`ThrottleDecision::Allow`].
//!
//! # The challenge tier is off until something can answer it
//!
//! A [`ThrottleDecision::Challenge`] with no [`CaptchaVerifier`] registered is a
//! **refusal**, and that reading is right: treating "we cannot check" as "let
//! them through" would make the challenge tier a way to skip the throttle rather
//! than a way to slow it down. What was wrong was the default that met it.
//!
//! With [`ThrottleConfig::challenge_after`] at three and no verifier shipped,
//! three mistyped passwords put an account into a state with **no way out**: the
//! gate refuses before the credential check, so the user cannot succeed, and only
//! a success clears the consecutive-failure counter. The account came back when
//! the counter expired, a quarter of an hour later. Worse, anyone who knew a
//! victim's address could put them there on purpose with three wrong guesses —
//! which is precisely the hard lock this module's second paragraph refuses to
//! ship.
//!
//! So the tier is off by default ([`ThrottleConfig::CHALLENGE_OFF`]) and is
//! turned on by the same composition root that registers a verifier. Nothing
//! about the throttle is weakened by this: the per-address GCRA quota and the
//! per-identity exponential backoff are untouched, and they are the two tiers
//! that were ever doing the work. What is removed is a denial of service against
//! the deployment's own users.
//!
//! The alternatives were considered and rejected. Degrading `Challenge` to
//! `Allow` with a warning hides a misconfiguration in a per-request log line
//! instead of fixing it, and puts the decision in the caller rather than in the
//! configuration. Keeping the refusal and improving its wording still leaves the
//! user with nothing to do. [`crate::captcha`] closes the other half by shipping
//! a verifier, so "turn it on" is now a two-line change rather than an
//! implementation project.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use moso_kv::{RateQuota, SetOpts};
use serde::{Deserialize, Serialize};

use crate::jwks::sha256_hex;
use crate::lifecycle::normalise;
use crate::{AuthCtx, Error, Result};

// ---------------------------------------------------------------------------
// ThrottleDecision
// ---------------------------------------------------------------------------

/// What a throttle decided.
///
/// ```
/// use moso_auth::ThrottleDecision;
///
/// assert!(ThrottleDecision::Allow.is_allowed());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ThrottleDecision {
    /// Proceed.
    Allow,
    /// Refuse, and tell the client when to come back.
    Deny {
        /// How long until the next attempt is allowed.
        retry_after: Duration,
    },
    /// Proceed, but only after a CAPTCHA.
    ///
    /// The middle setting: an address that has failed a few times is suspicious
    /// and not yet worth refusing, and a challenge costs an attacker far more
    /// than it costs a user who mistyped their password.
    Challenge,
}

impl ThrottleDecision {
    /// Whether the attempt may proceed without a challenge.
    ///
    /// ```
    /// use moso_auth::ThrottleDecision;
    ///
    /// assert!(!ThrottleDecision::Challenge.is_allowed());
    /// ```
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

// ---------------------------------------------------------------------------
// AttemptRecord
// ---------------------------------------------------------------------------

/// One recorded attempt.
///
/// Kept so that "five failures in fifteen minutes" can trigger a notification
/// email to the account's owner — which is often the first the real user hears
/// of an attack on their account. Successes are recorded too: a login the owner
/// did not make is the thing a security page exists to show.
///
/// ```
/// use moso_auth::{AuthCtx, LoginThrottle, ThrottleConfig};
/// use moso_kv::Kv;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso_auth::Result<()> {
/// let throttle = LoginThrottle::new(
///     Kv::in_memory("shop").expect("an in-memory kv"),
///     ThrottleConfig::default(),
/// );
/// let ctx = AuthCtx::new()
///     .with_identity("Ada@Example.com")
///     .with_ip("203.0.113.7");
/// throttle.record(&ctx, false).await?;
///
/// let recent = throttle.recent("ada@example.com", 10).await?;
/// assert_eq!(recent[0].identity, "ada@example.com");
/// assert_eq!(recent[0].ip.as_deref(), Some("203.0.113.7"));
/// assert!(!recent[0].succeeded);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AttemptRecord {
    /// Which identity was attempted, lowercased.
    pub identity: String,
    /// From where.
    pub ip: Option<String>,
    /// With what.
    pub user_agent: Option<String>,
    /// Whether it worked.
    pub succeeded: bool,
    /// When.
    pub at: DateTime<Utc>,
}

/// How many attempts are kept for one identity.
///
/// The list is written by an unauthenticated caller — anybody who can post a
/// login form — so it has to be bounded, or an attacker grows one value until
/// reading it is itself the denial of service. Twenty is what a security page
/// shows above the fold and comfortably more than the largest `notify_after`
/// anybody configures.
///
/// ```
/// assert_eq!(moso_auth::throttle::ATTEMPT_HISTORY, 20);
/// ```
pub const ATTEMPT_HISTORY: usize = 20;

/// How long the attempt list survives its last write.
///
/// Thirty days, because "was there a login from Lagos last month" is the
/// question a user asks after they hear about a breach, and a list that expires
/// with the notification window would have nothing to answer with.
///
/// ```
/// use std::time::Duration;
///
/// assert_eq!(moso_auth::throttle::ATTEMPT_RETENTION, Duration::from_secs(30 * 86_400));
/// ```
pub const ATTEMPT_RETENTION: Duration = Duration::from_secs(30 * 86_400);

/// The bucket every per-address quota in this module lives under.
///
/// Scoped so that the login throttle and any other `RateLimit` an application
/// attaches to the same address do not consume each other's quota.
const ADDRESS_SCOPE: &str = "auth-login-ip";

/// The bucket an attempt with no address falls into.
///
/// A shared bucket and *not* an exemption. An absent address is the state an
/// attacker arranges — a stripped `X-Forwarded-For`, a proxy the deployment
/// does not trust — so treating it as "unlimited" would hand them the removal
/// of the limiter. Everything anonymous shares one quota instead, which is the
/// same rule [`moso_kv::RateKey`] states for itself.
const UNKNOWN_ADDRESS: &str = "unknown";

// ---------------------------------------------------------------------------
// ThrottleConfig
// ---------------------------------------------------------------------------

/// How aggressive the throttle is.
///
/// ```
/// use moso_auth::ThrottleConfig;
///
/// let config = ThrottleConfig::default();
/// assert_eq!(config.per_ip_burst, 10);
/// assert_eq!(config.notify_after, 5);
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ThrottleConfig {
    /// How many attempts one address may make in a burst.
    pub per_ip_burst: u32,
    /// How long the per-address quota takes to refill by one.
    pub per_ip_period: Duration,
    /// How many failures against one identity before backoff starts.
    pub per_identity_free: u32,
    /// The first backoff delay. Doubles per failure after that, capped.
    pub per_identity_base: Duration,
    /// The longest per-identity backoff. Ten minutes: long enough to make
    /// guessing pointless, short enough that a locked-out user does not give up.
    pub per_identity_max: Duration,
    /// How many failures in the window before the account's owner is emailed.
    pub notify_after: u32,
    /// The window that count is measured over.
    pub notify_window: Duration,
    /// How many failures before a CAPTCHA is demanded.
    ///
    /// [`CHALLENGE_OFF`](ThrottleConfig::CHALLENGE_OFF) by default, and see the
    /// module documentation for why: a challenge nobody can answer is not a
    /// challenge, it is a lockout. Set this **and** register a
    /// [`CaptchaVerifier`] — one without the other is the misconfiguration this
    /// default exists to make impossible by accident.
    pub challenge_after: u32,
}

impl ThrottleConfig {
    /// The value of [`challenge_after`](ThrottleConfig::challenge_after) that
    /// turns the challenge tier off.
    ///
    /// A sentinel rather than an `Option<u32>`: the field is compared against a
    /// failure count on the login path, and `u32::MAX` failures is a number no
    /// attacker reaches, so "off" needs no second branch that could be forgotten.
    ///
    /// ```
    /// use moso_auth::ThrottleConfig;
    ///
    /// assert_eq!(ThrottleConfig::default().challenge_after, ThrottleConfig::CHALLENGE_OFF);
    /// ```
    pub const CHALLENGE_OFF: u32 = u32::MAX;
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            per_ip_burst: 10,
            per_ip_period: Duration::from_secs(60),
            per_identity_free: 3,
            per_identity_base: Duration::from_secs(2),
            per_identity_max: Duration::from_secs(600),
            notify_after: 5,
            notify_window: Duration::from_secs(900),
            challenge_after: Self::CHALLENGE_OFF,
        }
    }
}

// ---------------------------------------------------------------------------
// The keyspace
// ---------------------------------------------------------------------------

moso_kv::namespace! {
    /// How many failures in a row an identity has had. `Raw`, so the backend's
    /// own `INCR` advances it: a read-modify-write here would let two attempts
    /// racing each other cost the attacker only one failure.
    pub(crate) ThrottleFailures: str => u64, codec = Raw, on_failure = fail;

    /// When the last of those failures was, in microseconds since the Unix
    /// epoch. Last writer wins, which is exactly what "the last one" means.
    pub(crate) ThrottleLastFailure: str => u64, codec = Raw, on_failure = fail;

    /// How many failures fell inside the current notification window.
    pub(crate) ThrottleWindow: str => u64, codec = Raw, on_failure = fail;

    /// Present when the owner has already been told about this window.
    pub(crate) ThrottleNotified: str => u64, codec = Raw, on_failure = fail;

    /// The recent attempts against one identity, newest first.
    pub(crate) ThrottleAttempts: str => Vec<AttemptRecord>, on_failure = fail;
}

/// A key-value failure, as a throttle failure.
///
/// Always [`Error::Unavailable`], never a decision: the whole point of the
/// namespaces above declaring `on_failure = fail` is that an outage reaches
/// here rather than being degraded into an empty counter, which reads as
/// "nobody has failed yet" and lets every attempt through.
///
/// Written out rather than left to `?` so that this module does not depend on
/// the `From<moso_kv::Error>` conversion in `error.rs` choosing the same
/// component name.
fn store_failed(operation: &'static str, error: moso_kv::Error) -> Error {
    Error::Unavailable {
        component: "login throttle store",
        detail: format!("{operation}: {error}"),
        source: Some(Box::new(error)),
    }
}

/// The key segment an identity occupies: the hex SHA-256 of its normalised
/// form, so the identity itself never appears in a key.
fn subject_of(identity: &str) -> String {
    sha256_hex(normalise(identity).as_bytes())
}

/// Now, in microseconds since the Unix epoch.
///
/// Wall clock rather than a monotonic instant, for the same reason
/// `moso_kv::rate` uses one: the value is compared across processes, where a
/// monotonic clock has no shared origin. A clock that jumps backwards makes the
/// backoff briefly shorter, which is the direction that cannot lock a user out.
fn now_micros(now: DateTime<Utc>) -> u64 {
    u64::try_from(now.timestamp_micros()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// LoginThrottle
// ---------------------------------------------------------------------------

/// The login throttle.
///
/// Backed by [`moso_kv`], so the quotas are shared across every process — a
/// per-process limiter multiplies the real limit by the pod count, which is how
/// a rate limit quietly stops being one.
///
/// ```
/// use moso_auth::{AuthCtx, LoginThrottle, ThrottleConfig, ThrottleDecision};
/// use moso_kv::Kv;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso_auth::Result<()> {
/// let throttle = LoginThrottle::new(
///     Kv::in_memory("shop").expect("an in-memory kv"),
///     ThrottleConfig::default(),
/// );
/// let ctx = AuthCtx::new()
///     .with_identity("ada@example.com")
///     .with_ip("203.0.113.7");
///
/// assert_eq!(throttle.check(&ctx).await?, ThrottleDecision::Allow);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct LoginThrottle {
    /// Where the counters live.
    kv: moso_kv::Kv,
    /// How aggressive it is.
    config: ThrottleConfig,
}

impl LoginThrottle {
    /// A throttle over `kv`.
    ///
    /// ```
    /// use moso_auth::{LoginThrottle, ThrottleConfig};
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::in_memory("shop").expect("an in-memory kv");
    /// let throttle = LoginThrottle::new(kv, ThrottleConfig::default());
    /// assert_eq!(throttle.config().per_identity_free, 3);
    /// ```
    #[must_use]
    pub fn new(kv: moso_kv::Kv, config: ThrottleConfig) -> Self {
        Self { kv, config }
    }

    /// How aggressive this throttle is.
    ///
    /// ```
    /// use moso_auth::{LoginThrottle, ThrottleConfig};
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::in_memory("shop").expect("an in-memory kv");
    /// let throttle = LoginThrottle::new(kv, ThrottleConfig::default());
    /// assert_eq!(throttle.config().notify_after, 5);
    /// ```
    #[must_use]
    pub fn config(&self) -> &ThrottleConfig {
        &self.config
    }

    // ── the decision ──────────────────────────────────────────────────────

    /// Decide whether an attempt may proceed.
    ///
    /// Checked **before** any credential work, so a refused attempt costs no
    /// password hash — which is the point: hashing is the expensive operation
    /// an attacker is trying to make the server do.
    ///
    /// Three tiers, in order. The per-address quota is charged first and
    /// unconditionally, so an attacker spraying one address across a thousand
    /// identities runs out whichever identities they name. Then the identity's
    /// backoff: past [`ThrottleConfig::per_identity_free`] failures the delay is
    /// `per_identity_base · 2^(failures − free − 1)`, saturating at
    /// [`ThrottleConfig::per_identity_max`], and an attempt inside that delay is
    /// refused with whatever is left of it. Then the challenge tier, which is
    /// what [`ThrottleConfig::challenge_after`] failures buy. An attempt with no
    /// identity in the `AuthCtx` has no per-identity state and is covered by the
    /// address quota alone.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the store cannot
    /// be reached. Deliberately **not** fail-open: a limiter that stops limiting
    /// when its store blinks is a limiter an attacker can remove.
    ///
    /// ```
    /// use moso_auth::{AuthCtx, LoginThrottle, ThrottleConfig, ThrottleDecision};
    /// use moso_kv::Kv;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// // `ThrottleConfig` is `#[non_exhaustive]`, so it is adjusted rather
    /// // than built from a literal.
    /// let mut config = ThrottleConfig::default();
    /// config.per_ip_burst = 1;
    ///
    /// let throttle = LoginThrottle::new(
    ///     Kv::in_memory("shop").expect("an in-memory kv"),
    ///     config,
    /// );
    /// let ctx = AuthCtx::new().with_ip("203.0.113.7");
    ///
    /// assert_eq!(throttle.check(&ctx).await?, ThrottleDecision::Allow);
    /// assert!(!throttle.check(&ctx).await?.is_allowed(), "the burst is spent");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn check(&self, ctx: &AuthCtx) -> Result<ThrottleDecision> {
        let address = self
            .kv
            .rate_limit(&self.address_bucket(ctx), self.address_quota())
            .await
            .map_err(|error| store_failed("rate_limit", error))?;
        if !address.allowed {
            return Ok(ThrottleDecision::Deny {
                retry_after: address.retry_after,
            });
        }

        let Some(identity) = ctx.identity() else {
            return Ok(ThrottleDecision::Allow);
        };
        let subject = subject_of(identity);

        let failures = self
            .kv
            .get::<ThrottleFailures>(&subject)
            .await
            .map_err(|error| store_failed("get failures", error))?
            .unwrap_or(0);

        if let Some(delay) = self.delay_for(failures) {
            let last = self
                .kv
                .get::<ThrottleLastFailure>(&subject)
                .await
                .map_err(|error| store_failed("get last failure", error))?
                .unwrap_or(0);
            let elapsed = Duration::from_micros(now_micros(Utc::now()).saturating_sub(last));
            let remaining = delay.saturating_sub(elapsed);
            if !remaining.is_zero() {
                return Ok(ThrottleDecision::Deny {
                    retry_after: remaining,
                });
            }
        }

        if failures >= u64::from(self.config.challenge_after) {
            return Ok(ThrottleDecision::Challenge);
        }
        Ok(ThrottleDecision::Allow)
    }

    /// Record the outcome of an attempt.
    ///
    /// A success clears the identity's backoff; a failure advances it and may
    /// trip the notification threshold. Both are appended to the attempt list,
    /// because a successful login the owner did not make is the one a security
    /// page most needs to show.
    ///
    /// A success clears the *consecutive* counter and not the windowed one. An
    /// attacker who guesses on the sixth try has still made five failures, and
    /// suppressing that notification because they eventually succeeded would
    /// silence the alert precisely when it matters.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```
    /// use moso_auth::{AuthCtx, LoginThrottle, ThrottleConfig};
    /// use moso_kv::Kv;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let throttle = LoginThrottle::new(
    ///     Kv::in_memory("shop").expect("an in-memory kv"),
    ///     ThrottleConfig::default(),
    /// );
    /// let ctx = AuthCtx::new().with_identity("ada@example.com");
    ///
    /// throttle.record(&ctx, false).await?;
    /// throttle.record(&ctx, true).await?;
    /// assert_eq!(throttle.recent("ada@example.com", 10).await?.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn record(&self, ctx: &AuthCtx, succeeded: bool) -> Result<()> {
        // Without an identity there is nothing to key the per-identity state
        // on; the attempt was already charged to the address quota in `check`.
        let Some(identity) = ctx.identity() else {
            return Ok(());
        };
        let identity = normalise(identity);
        let subject = subject_of(&identity);
        let now = Utc::now();

        if succeeded {
            self.clear_backoff(&subject).await?;
        } else {
            self.advance_backoff(&subject, now).await?;
        }

        self.append_attempt(&subject, &identity, ctx, succeeded, now)
            .await
    }

    /// Whether the failure count has crossed the notification threshold.
    ///
    /// Returns true **once** per window, so a sustained attack sends one email
    /// rather than one per attempt — which would itself be a way to use the
    /// application as a mail bomb.
    ///
    /// The marker is claimed with a set-if-absent, which is one atomic
    /// operation on every backend. A read followed by a write would let two
    /// requests both observe "not yet notified", and the mail bomb the sentence
    /// above rules out would arrive anyway, just with fewer copies.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```
    /// use moso_auth::{AuthCtx, LoginThrottle, ThrottleConfig};
    /// use moso_kv::Kv;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let mut config = ThrottleConfig::default();
    /// config.notify_after = 2;
    ///
    /// let throttle = LoginThrottle::new(
    ///     Kv::in_memory("shop").expect("an in-memory kv"),
    ///     config,
    /// );
    /// let ctx = AuthCtx::new().with_identity("ada@example.com");
    ///
    /// throttle.record(&ctx, false).await?;
    /// assert!(!throttle.should_notify("ada@example.com").await?);
    ///
    /// throttle.record(&ctx, false).await?;
    /// assert!(throttle.should_notify("ada@example.com").await?);
    /// assert!(!throttle.should_notify("ada@example.com").await?, "once a window");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn should_notify(&self, identity: &str) -> Result<bool> {
        let subject = subject_of(identity);
        let failures = self
            .kv
            .get::<ThrottleWindow>(&subject)
            .await
            .map_err(|error| store_failed("get window", error))?
            .unwrap_or(0);

        if failures < u64::from(self.config.notify_after) {
            return Ok(false);
        }

        let key = self
            .kv
            .key::<ThrottleNotified>(&subject)
            .map_err(|error| store_failed("key", error))?;
        self.kv
            .store()
            .set(
                &key,
                Bytes::from_static(b"1"),
                SetOpts::new().if_absent().ttl(self.config.notify_window),
            )
            .await
            .map_err(|error| store_failed("set notified", error))
    }

    /// The notice to send, when this window has just crossed the threshold.
    ///
    /// One call rather than [`should_notify`](LoginThrottle::should_notify)
    /// followed by [`recent`](LoginThrottle::recent), because the "once per
    /// window" claim and the evidence that goes in the mail have to come from
    /// the same moment: a caller that asked for the marker and then forgot to
    /// read the attempts would send an email with nothing in it, and one that
    /// read the attempts first would put a stale list in it.
    ///
    /// `Ok(None)` is the ordinary answer — most failures are somebody mistyping
    /// their own password — and the threshold has already been claimed when it
    /// is `Ok(Some(..))`, so a caller that drops the notice has spent this
    /// window's one notification. Deliver it, or log that you did not.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```
    /// use moso_auth::{AuthCtx, LoginThrottle, SecurityNoticeKind, ThrottleConfig};
    /// use moso_kv::Kv;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let mut config = ThrottleConfig::default();
    /// config.notify_after = 2;
    ///
    /// let throttle = LoginThrottle::new(Kv::in_memory("shop").expect("an in-memory kv"), config);
    /// let ctx = AuthCtx::new().with_identity("ada@example.com").with_ip("203.0.113.7");
    ///
    /// throttle.record(&ctx, false).await?;
    /// assert!(throttle.notice("ada@example.com").await?.is_none());
    ///
    /// throttle.record(&ctx, false).await?;
    /// let notice = throttle.notice("ada@example.com").await?.expect("past the threshold");
    ///
    /// assert_eq!(notice.kind(), SecurityNoticeKind::RepeatedSignInFailures);
    /// assert_eq!(notice.destination(), "ada@example.com");
    /// assert_eq!(notice.recent().len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn notice(&self, identity: &str) -> Result<Option<SecurityNotice>> {
        if !self.should_notify(identity).await? {
            return Ok(None);
        }

        let subject = subject_of(identity);
        let failures = self
            .kv
            .get::<ThrottleWindow>(&subject)
            .await
            .map_err(|error| store_failed("get window", error))?
            .unwrap_or_else(|| u64::from(self.config.notify_after));

        Ok(Some(SecurityNotice {
            kind: SecurityNoticeKind::RepeatedSignInFailures,
            destination: normalise(identity),
            failures,
            window: self.config.notify_window,
            // `u32::MAX` because the stored list is already bounded by
            // `ATTEMPT_HISTORY`; asking for more than exists is how this call
            // says "all of it" without restating the bound.
            recent: self.recent(identity, u32::MAX).await?,
            at: Utc::now(),
        }))
    }

    /// Recent attempts against an identity, for the account's security page.
    ///
    /// Newest first, at most `limit`, and at most [`ATTEMPT_HISTORY`] however
    /// large `limit` is.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```
    /// use moso_auth::{AuthCtx, LoginThrottle, ThrottleConfig};
    /// use moso_kv::Kv;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let throttle = LoginThrottle::new(
    ///     Kv::in_memory("shop").expect("an in-memory kv"),
    ///     ThrottleConfig::default(),
    /// );
    /// assert!(throttle.recent("ada@example.com", 20).await?.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn recent(&self, identity: &str, limit: u32) -> Result<Vec<AttemptRecord>> {
        let subject = subject_of(identity);
        let mut attempts = self
            .kv
            .get::<ThrottleAttempts>(&subject)
            .await
            .map_err(|error| store_failed("get attempts", error))?
            .unwrap_or_default();
        attempts.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(attempts)
    }

    // ── the pieces ────────────────────────────────────────────────────────

    /// The GCRA bucket this attempt's address falls into.
    fn address_bucket(&self, ctx: &AuthCtx) -> String {
        match ctx.ip() {
            Some(address) => format!("{ADDRESS_SCOPE}:{}", sha256_hex(address.as_bytes())),
            None => format!("{ADDRESS_SCOPE}:{UNKNOWN_ADDRESS}"),
        }
    }

    /// The per-address quota, as [`RateQuota`] spells it.
    ///
    /// [`ThrottleConfig::per_ip_period`] is how long the quota takes to refill
    /// by *one* — the emission interval — whereas [`RateQuota::new`] takes the
    /// period the whole limit is expressed over. The two differ by the burst,
    /// which is what the multiplication is: `burst` attempts at once, then one
    /// more every `per_ip_period`.
    fn address_quota(&self) -> RateQuota {
        let burst = self.config.per_ip_burst.max(1);
        RateQuota::new(burst, self.config.per_ip_period.saturating_mul(burst))
    }

    /// How long the per-identity counters survive their last write.
    ///
    /// The larger of the two windows they feed, so neither the backoff nor the
    /// notification loses its history while the other still needs it.
    fn backoff_ttl(&self) -> Duration {
        self.config.per_identity_max.max(self.config.notify_window)
    }

    /// The backoff `failures` consecutive failures earn, or `None` while the
    /// identity is still inside its free tier.
    ///
    /// Every step is checked rather than shifted blind: `1 << 32` is a panic in
    /// debug and a wrong answer in release, and this runs on the login path
    /// with a counter an attacker chooses.
    fn delay_for(&self, failures: u64) -> Option<Duration> {
        let free = u64::from(self.config.per_identity_free);
        let steps = failures.checked_sub(free)?.checked_sub(1)?;
        let delay = u32::try_from(steps)
            .ok()
            .and_then(|steps| 1_u32.checked_shl(steps))
            .and_then(|factor| self.config.per_identity_base.checked_mul(factor))
            .unwrap_or(self.config.per_identity_max);
        Some(delay.min(self.config.per_identity_max))
    }

    /// Forget an identity's consecutive failures.
    async fn clear_backoff(&self, subject: &str) -> Result<()> {
        self.kv
            .delete::<ThrottleFailures>(subject)
            .await
            .map_err(|error| store_failed("delete failures", error))?;
        self.kv
            .delete::<ThrottleLastFailure>(subject)
            .await
            .map_err(|error| store_failed("delete last failure", error))?;
        Ok(())
    }

    /// Count one more failure against an identity and stamp the time.
    async fn advance_backoff(&self, subject: &str, now: DateTime<Utc>) -> Result<()> {
        let ttl = self.backoff_ttl();
        let store = self.kv.store();

        let failures = self
            .kv
            .key::<ThrottleFailures>(subject)
            .map_err(|error| store_failed("key", error))?;
        store
            .incr(&failures, 1, Some(ttl))
            .await
            .map_err(|error| store_failed("incr failures", error))?;
        // `incr` applies its TTL only when it creates the key, so the expiry is
        // pushed out here: the counter has to mean "failures in a row ending
        // now", not "failures since the first one, whenever that was".
        store
            .expire(&failures, ttl)
            .await
            .map_err(|error| store_failed("expire failures", error))?;

        self.kv
            .set_ttl::<ThrottleLastFailure>(subject, &now_micros(now), ttl)
            .await
            .map_err(|error| store_failed("set last failure", error))?;

        // Deliberately *not* refreshed: the notification window is fixed, so it
        // ends a fixed time after the first failure in it and the next one
        // starts clean. A sliding window would notify once and then never
        // again for as long as the attack continued.
        let window = self
            .kv
            .key::<ThrottleWindow>(subject)
            .map_err(|error| store_failed("key", error))?;
        store
            .incr(&window, 1, Some(self.config.notify_window))
            .await
            .map_err(|error| store_failed("incr window", error))?;
        Ok(())
    }

    /// Put one attempt at the head of the identity's list.
    ///
    /// A read-modify-write, and knowingly so: two attempts racing can lose one
    /// entry from a display list, which costs a line on a security page and no
    /// security property. The counters that *are* security properties go
    /// through `incr` instead.
    async fn append_attempt(
        &self,
        subject: &str,
        identity: &str,
        ctx: &AuthCtx,
        succeeded: bool,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let mut attempts = self
            .kv
            .get::<ThrottleAttempts>(subject)
            .await
            .map_err(|error| store_failed("get attempts", error))?
            .unwrap_or_default();

        attempts.insert(
            0,
            AttemptRecord {
                identity: identity.to_owned(),
                ip: ctx.ip().map(str::to_owned),
                user_agent: ctx.user_agent().map(str::to_owned),
                succeeded,
                at,
            },
        );
        attempts.truncate(ATTEMPT_HISTORY);

        self.kv
            .set_ttl::<ThrottleAttempts>(subject, &attempts, ATTEMPT_RETENTION)
            .await
            .map_err(|error| store_failed("set attempts", error))
    }
}

impl core::fmt::Debug for LoginThrottle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoginThrottle")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Security notices
// ---------------------------------------------------------------------------

/// What a [`SecurityNotice`] is about.
///
/// One variant today, and `#[non_exhaustive]` because the next one — a sign-in
/// from an address this account has never used, a second factor removed — is a
/// minor release rather than a breaking change.
///
/// ```
/// use moso_auth::SecurityNoticeKind;
///
/// assert_eq!(SecurityNoticeKind::RepeatedSignInFailures.as_str(), "repeated_sign_in_failures");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SecurityNoticeKind {
    /// "Somebody has been trying to sign in to your account."
    RepeatedSignInFailures,
}

impl SecurityNoticeKind {
    /// The name a template or a log line uses.
    ///
    /// ```
    /// use moso_auth::SecurityNoticeKind;
    ///
    /// assert!(!SecurityNoticeKind::RepeatedSignInFailures.as_str().is_empty());
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepeatedSignInFailures => "repeated_sign_in_failures",
        }
    }
}

/// Something the account's owner should be told about, once.
///
/// [`LoginThrottle::should_notify`] has always known when to send one; this is
/// the *what*. `moso-auth` does not depend on `moso-mail` and should not —
/// which provider, which template and whether the send goes through a job queue
/// are the application's decisions — so what the battery owes it is the fact and
/// the evidence, which is exactly what this carries.
///
/// # It holds no secret, and that is structural
///
/// A [`Delivery`](crate::routes::Delivery) carries a live token and has an
/// `expose()`; a `SecurityNotice` has neither, and there is no conversion
/// between them. That is the same separation
/// [`DeliveryPurpose`](crate::routes::DeliveryPurpose) keeps from
/// [`TokenPurpose`](crate::TokenPurpose), for the same reason: two vocabularies
/// that can be substituted for one another eventually are, and here the
/// substitution would mail a credential to an address that has just been the
/// target of a guessing attack. A sink registered for notices cannot be handed a
/// token, because the type it takes has nowhere to put one.
///
/// ```
/// use moso_auth::{SecurityNotice, SecurityNoticeKind};
///
/// # fn f(notice: &SecurityNotice) {
/// assert_eq!(notice.kind(), SecurityNoticeKind::RepeatedSignInFailures);
/// // There is no `notice.expose()`, and there is not meant to be.
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct SecurityNotice {
    /// What happened.
    kind: SecurityNoticeKind,
    /// Who to tell: the identity that was attempted, normalised.
    destination: String,
    /// How many failures fell inside the window.
    failures: u64,
    /// How long that window is.
    window: Duration,
    /// The attempts themselves, newest first, for the "was this you?" table.
    recent: Vec<AttemptRecord>,
    /// When the threshold was crossed.
    at: DateTime<Utc>,
}

impl SecurityNotice {
    /// What happened.
    ///
    /// ```
    /// # use moso_auth::{SecurityNotice, SecurityNoticeKind};
    /// # fn f(n: &SecurityNotice) { let _: SecurityNoticeKind = n.kind(); }
    /// ```
    #[must_use]
    pub const fn kind(&self) -> SecurityNoticeKind {
        self.kind
    }

    /// Who to tell.
    ///
    /// The identity that was attempted, normalised — which is an address in
    /// almost every application and a username in the rest. An application whose
    /// identities are not addresses looks the account up before mailing.
    ///
    /// ```
    /// # use moso_auth::SecurityNotice;
    /// # fn f(n: &SecurityNotice) { let _: &str = n.destination(); }
    /// ```
    #[must_use]
    pub fn destination(&self) -> &str {
        &self.destination
    }

    /// How many failures fell inside the window.
    ///
    /// ```
    /// # use moso_auth::SecurityNotice;
    /// # fn f(n: &SecurityNotice) { let _: u64 = n.failures(); }
    /// ```
    #[must_use]
    pub const fn failures(&self) -> u64 {
        self.failures
    }

    /// How long that window is, so the mail can say "in the last fifteen
    /// minutes" rather than "recently".
    ///
    /// ```
    /// # use moso_auth::SecurityNotice;
    /// # fn f(n: &SecurityNotice) { let _ = n.window(); }
    /// ```
    #[must_use]
    pub const fn window(&self) -> Duration {
        self.window
    }

    /// The attempts themselves, newest first.
    ///
    /// ```
    /// # use moso_auth::{AttemptRecord, SecurityNotice};
    /// # fn f(n: &SecurityNotice) { let _: &[AttemptRecord] = n.recent(); }
    /// ```
    #[must_use]
    pub fn recent(&self) -> &[AttemptRecord] {
        &self.recent
    }

    /// When the threshold was crossed.
    ///
    /// ```
    /// # use chrono::{DateTime, Utc};
    /// # use moso_auth::SecurityNotice;
    /// # fn f(n: &SecurityNotice) { let _: DateTime<Utc> = n.at(); }
    /// ```
    #[must_use]
    pub const fn at(&self) -> DateTime<Utc> {
        self.at
    }
}

/// Where a [`SecurityNotice`] is handed to whoever sends the email.
///
/// The same shape as [`TokenSink`](crate::routes::TokenSink) — an `Arc`'d
/// closure returning a boxed future — because it is the same job: the battery
/// hands the application something to send and does not care what happens next.
/// Registering none is legal and is what a prototype does; the caller logs a
/// warning saying the notice was dropped, because an alert that silently sends
/// nothing is worse than one that says so.
///
/// ```
/// use std::sync::Arc;
///
/// use moso_auth::NoticeSink;
///
/// let sink: NoticeSink = Arc::new(|notice| {
///     Box::pin(async move {
///         // Hand `notice.destination()` and `notice.recent()` to the mailer.
///         let _ = notice.failures();
///     })
/// });
/// let _ = sink;
/// ```
pub type NoticeSink = Arc<dyn Fn(SecurityNotice) -> BoxFuture<'static, ()> + Send + Sync + 'static>;

// ---------------------------------------------------------------------------
// CaptchaVerifier
// ---------------------------------------------------------------------------

/// Verifies a CAPTCHA response.
///
/// Dyn-compatible so an application can plug in whichever provider it uses. The
/// throttle asks for one only after [`ThrottleConfig::challenge_after`]
/// failures, so an ordinary user never sees it.
///
/// ```no_run
/// use moso_auth::CaptchaVerifier;
///
/// // `no_run`: every verifier is a network call to a provider.
/// async fn check(v: &dyn CaptchaVerifier, token: &str) -> moso_auth::Result<bool> {
///     v.verify(token, None).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot verify a CAPTCHA",
    label = "not a CAPTCHA verifier",
    note = "a CAPTCHA verifier implements `provider` and `verify`",
    note = "help: register one with `.provide_dyn::<dyn CaptchaVerifier>(v)`; without one, \
            `ThrottleDecision::Challenge` is treated as `Deny`, which is the safe reading"
)]
pub trait CaptchaVerifier: Send + Sync + 'static {
    /// Which provider, for the log.
    fn provider(&self) -> &'static str;

    /// Check a response token.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the provider
    /// cannot be reached. A *failed* verification is `Ok(false)`, not an error,
    /// so a `?` cannot accidentally turn a failed CAPTCHA into a 500 that some
    /// middleware retries.
    fn verify<'a>(&'a self, token: &'a str, ip: Option<&'a str>) -> BoxFuture<'a, Result<bool>>;
}

// `pub(crate)` for one item: `DownStore`, below. Fail-closed is a property two
// modules in this crate have to prove, and a second hand-written "always down"
// store would be a second opinion about what "down" means.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use moso_core::HealthStatus;
    use moso_kv::{Capabilities, Key, KvStore};

    // ── fixtures ──────────────────────────────────────────────────────────

    /// A throttle over a fresh in-memory store.
    fn throttle(config: ThrottleConfig) -> LoginThrottle {
        LoginThrottle::new(
            moso_kv::Kv::in_memory("throttle-test").expect("an in-memory kv"),
            config,
        )
    }

    /// A config whose address quota is wide enough never to be the reason an
    /// attempt is refused, so a test of the identity tier tests only that.
    fn identity_only(config: ThrottleConfig) -> ThrottleConfig {
        ThrottleConfig {
            per_ip_burst: 10_000,
            ..config
        }
    }

    fn ctx() -> AuthCtx {
        AuthCtx::new()
            .with_identity("ada@example.com")
            .with_ip("203.0.113.7")
    }

    /// A backend that is always down, for every fail-closed test in the crate.
    ///
    /// Shared with [`crate::mfa`] rather than copied: two stores that both
    /// claimed to be "down" would eventually disagree about which operations
    /// fail, and the module whose copy was gentler would stop proving anything.
    #[derive(Debug)]
    pub(crate) struct DownStore;

    /// What every `DownStore` operation answers.
    fn down(operation: &'static str) -> moso_kv::Error {
        moso_kv::Error::backend("down", operation, "the store is unreachable")
    }

    impl KvStore for DownStore {
        fn name(&self) -> &'static str {
            "down"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::none()
        }

        fn health(&self) -> BoxFuture<'_, HealthStatus> {
            Box::pin(async { HealthStatus::Down("test fixture".to_owned()) })
        }

        fn get<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, moso_kv::Result<Option<Bytes>>> {
            Box::pin(async { Err(down("get")) })
        }

        fn set<'a>(
            &'a self,
            _key: &'a Key,
            _value: Bytes,
            _opts: SetOpts,
        ) -> BoxFuture<'a, moso_kv::Result<bool>> {
            Box::pin(async { Err(down("set")) })
        }

        fn delete<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, moso_kv::Result<bool>> {
            Box::pin(async { Err(down("delete")) })
        }

        fn exists<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, moso_kv::Result<bool>> {
            Box::pin(async { Err(down("exists")) })
        }

        fn expire<'a>(
            &'a self,
            _key: &'a Key,
            _ttl: Duration,
        ) -> BoxFuture<'a, moso_kv::Result<bool>> {
            Box::pin(async { Err(down("expire")) })
        }

        fn ttl<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, moso_kv::Result<Option<Duration>>> {
            Box::pin(async { Err(down("ttl")) })
        }

        fn incr<'a>(
            &'a self,
            _key: &'a Key,
            _by: i64,
            _ttl: Option<Duration>,
        ) -> BoxFuture<'a, moso_kv::Result<i64>> {
            Box::pin(async { Err(down("incr")) })
        }
    }

    // ── the decision ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_fresh_identity_is_allowed() {
        let throttle = throttle(ThrottleConfig::default());
        assert_eq!(
            throttle.check(&ctx()).await.expect("decided"),
            ThrottleDecision::Allow
        );
    }

    #[tokio::test]
    async fn the_address_quota_refuses_the_attempt_after_the_burst_and_says_when_to_return() {
        let throttle = throttle(ThrottleConfig {
            per_ip_burst: 3,
            per_ip_period: Duration::from_secs(60),
            ..ThrottleConfig::default()
        });
        let ctx = AuthCtx::new().with_ip("203.0.113.7");

        for attempt in 1..=3 {
            assert!(
                throttle.check(&ctx).await.expect("decided").is_allowed(),
                "attempt {attempt} is inside the burst"
            );
        }

        match throttle.check(&ctx).await.expect("decided") {
            ThrottleDecision::Deny { retry_after } => {
                assert!(
                    retry_after > Duration::ZERO,
                    "a deny must say when to return"
                );
                assert!(retry_after <= Duration::from_secs(60));
            }
            other => panic!("the fourth attempt was {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_attempt_with_no_address_shares_one_bucket_rather_than_escaping_the_quota() {
        let throttle = throttle(ThrottleConfig {
            per_ip_burst: 1,
            ..ThrottleConfig::default()
        });
        let anonymous = AuthCtx::new();

        assert!(
            throttle
                .check(&anonymous)
                .await
                .expect("decided")
                .is_allowed()
        );
        assert!(
            !throttle
                .check(&anonymous)
                .await
                .expect("decided")
                .is_allowed()
        );
    }

    #[tokio::test]
    async fn two_addresses_do_not_share_a_quota() {
        let throttle = throttle(ThrottleConfig {
            per_ip_burst: 1,
            ..ThrottleConfig::default()
        });
        let first = AuthCtx::new().with_ip("203.0.113.7");
        let second = AuthCtx::new().with_ip("198.51.100.9");

        assert!(throttle.check(&first).await.expect("decided").is_allowed());
        assert!(throttle.check(&second).await.expect("decided").is_allowed());
        assert!(!throttle.check(&first).await.expect("decided").is_allowed());
    }

    // ── the per-identity backoff ──────────────────────────────────────────

    #[test]
    fn the_free_tier_earns_no_delay_at_all() {
        let throttle = throttle(ThrottleConfig::default());
        assert_eq!(throttle.delay_for(0), None);
        assert_eq!(throttle.delay_for(3), None);
        assert_eq!(throttle.delay_for(4), Some(Duration::from_secs(2)));
    }

    #[test]
    fn the_per_identity_backoff_doubles_and_saturates_at_the_configured_maximum() {
        let throttle = throttle(ThrottleConfig {
            per_identity_free: 0,
            per_identity_base: Duration::from_secs(2),
            per_identity_max: Duration::from_secs(16),
            ..ThrottleConfig::default()
        });

        assert_eq!(throttle.delay_for(1), Some(Duration::from_secs(2)));
        assert_eq!(throttle.delay_for(2), Some(Duration::from_secs(4)));
        assert_eq!(throttle.delay_for(3), Some(Duration::from_secs(8)));
        assert_eq!(throttle.delay_for(4), Some(Duration::from_secs(16)));
        assert_eq!(
            throttle.delay_for(5),
            Some(Duration::from_secs(16)),
            "saturated"
        );
    }

    #[test]
    fn the_backoff_never_overflows_however_many_failures_an_attacker_buys() {
        let throttle = throttle(ThrottleConfig {
            per_identity_free: 0,
            per_identity_base: Duration::from_secs(2),
            per_identity_max: Duration::from_secs(600),
            ..ThrottleConfig::default()
        });

        // 1 << 63 and 1 << 64 are both a shift overflow, and both must be the
        // cap rather than a panic on the login path.
        assert_eq!(throttle.delay_for(64), Some(Duration::from_secs(600)));
        assert_eq!(throttle.delay_for(u64::MAX), Some(Duration::from_secs(600)));
    }

    #[tokio::test]
    async fn a_failure_past_the_free_tier_is_refused_for_the_backoff_it_earned() {
        let throttle = throttle(identity_only(ThrottleConfig {
            per_identity_free: 1,
            per_identity_base: Duration::from_secs(60),
            per_identity_max: Duration::from_secs(600),
            ..ThrottleConfig::default()
        }));
        let ctx = ctx();

        throttle.record(&ctx, false).await.expect("recorded");
        throttle.record(&ctx, false).await.expect("recorded");

        match throttle.check(&ctx).await.expect("decided") {
            ThrottleDecision::Deny { retry_after } => {
                assert!(retry_after > Duration::from_secs(55), "{retry_after:?}");
                assert!(retry_after <= Duration::from_secs(60), "{retry_after:?}");
            }
            other => panic!("the third attempt was {other:?}"),
        }

        // One more failure doubles the wait.
        throttle.record(&ctx, false).await.expect("recorded");
        match throttle.check(&ctx).await.expect("decided") {
            ThrottleDecision::Deny { retry_after } => {
                assert!(retry_after > Duration::from_secs(115), "{retry_after:?}");
                assert!(retry_after <= Duration::from_secs(120), "{retry_after:?}");
            }
            other => panic!("the fourth attempt was {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_success_clears_the_backoff() {
        let throttle = throttle(identity_only(ThrottleConfig {
            per_identity_free: 0,
            per_identity_base: Duration::from_secs(600),
            challenge_after: u32::MAX,
            ..ThrottleConfig::default()
        }));
        let ctx = ctx();

        throttle.record(&ctx, false).await.expect("recorded");
        assert!(!throttle.check(&ctx).await.expect("decided").is_allowed());

        throttle.record(&ctx, true).await.expect("recorded");
        assert_eq!(
            throttle.check(&ctx).await.expect("decided"),
            ThrottleDecision::Allow,
            "a success wipes the slate"
        );
    }

    #[tokio::test]
    async fn the_challenge_tier_sits_between_allowing_and_refusing() {
        let throttle = throttle(identity_only(ThrottleConfig {
            per_identity_free: 10,
            challenge_after: 2,
            ..ThrottleConfig::default()
        }));
        let ctx = ctx();

        throttle.record(&ctx, false).await.expect("recorded");
        assert_eq!(
            throttle.check(&ctx).await.expect("decided"),
            ThrottleDecision::Allow
        );

        throttle.record(&ctx, false).await.expect("recorded");
        assert_eq!(
            throttle.check(&ctx).await.expect("decided"),
            ThrottleDecision::Challenge
        );
    }

    /// The default must not be able to produce a challenge, because with no
    /// verifier registered a challenge is a refusal nobody can clear — and the
    /// user who earned it did so by mistyping their own password.
    #[tokio::test]
    async fn the_challenge_tier_is_off_until_it_is_turned_on() {
        let backing_off = throttle(identity_only(ThrottleConfig::default()));
        let ctx = ctx();

        // Far past `challenge_after`'s old default of three, and past the free
        // tier too, so the only thing that can refuse here is the backoff.
        for _ in 0..6 {
            backing_off.record(&ctx, false).await.expect("recorded");
        }

        assert!(
            matches!(
                backing_off.check(&ctx).await.expect("decided"),
                ThrottleDecision::Deny { .. }
            ),
            "the backoff still applies; it is the challenge that is off"
        );

        // And a user past the failure count that used to demand a challenge is
        // allowed through rather than held at one they have no way to answer.
        let patient = throttle(identity_only(ThrottleConfig {
            per_identity_free: 100,
            ..ThrottleConfig::default()
        }));
        for _ in 0..6 {
            patient.record(&ctx, false).await.expect("recorded");
        }
        assert_eq!(
            patient.check(&ctx).await.expect("decided"),
            ThrottleDecision::Allow,
            "an unclearable challenge is a denial of service on your own users"
        );
    }

    #[tokio::test]
    async fn one_identitys_failures_do_not_throttle_another() {
        let throttle = throttle(identity_only(ThrottleConfig {
            per_identity_free: 0,
            per_identity_base: Duration::from_secs(600),
            ..ThrottleConfig::default()
        }));

        let ada = AuthCtx::new().with_identity("ada@example.com");
        let grace = AuthCtx::new().with_identity("grace@example.com");

        throttle.record(&ada, false).await.expect("recorded");
        assert!(!throttle.check(&ada).await.expect("decided").is_allowed());
        assert_eq!(
            throttle.check(&grace).await.expect("decided"),
            ThrottleDecision::Allow
        );
    }

    #[tokio::test]
    async fn an_identity_is_keyed_by_its_normalised_form() {
        let throttle = throttle(identity_only(ThrottleConfig::default()));
        let shouted = AuthCtx::new().with_identity("  ADA@Example.COM ");

        throttle.record(&shouted, false).await.expect("recorded");
        assert_eq!(
            throttle
                .recent("ada@example.com", 10)
                .await
                .expect("read")
                .len(),
            1,
            "capitalisation is not an evasion"
        );
    }

    // ── notification ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_notify_fires_once_inside_a_window_however_often_it_is_asked() {
        let throttle = throttle(identity_only(ThrottleConfig {
            notify_after: 2,
            notify_window: Duration::from_secs(900),
            ..ThrottleConfig::default()
        }));
        let ctx = ctx();

        throttle.record(&ctx, false).await.expect("recorded");
        assert!(
            !throttle
                .should_notify("ada@example.com")
                .await
                .expect("asked"),
            "one failure is under the threshold"
        );

        throttle.record(&ctx, false).await.expect("recorded");
        assert!(
            throttle
                .should_notify("ada@example.com")
                .await
                .expect("asked")
        );

        for _ in 0..5 {
            assert!(
                !throttle
                    .should_notify("ada@example.com")
                    .await
                    .expect("asked"),
                "the window has already been reported"
            );
        }
    }

    #[tokio::test]
    async fn a_success_does_not_silence_the_failures_that_came_before_it() {
        let throttle = throttle(identity_only(ThrottleConfig {
            notify_after: 2,
            ..ThrottleConfig::default()
        }));
        let ctx = ctx();

        throttle.record(&ctx, false).await.expect("recorded");
        throttle.record(&ctx, false).await.expect("recorded");
        throttle.record(&ctx, true).await.expect("recorded");

        assert!(
            throttle
                .should_notify("ada@example.com")
                .await
                .expect("asked"),
            "guessing right on the third try is the case most worth an email"
        );
    }

    #[tokio::test]
    async fn a_notice_arrives_once_a_window_and_carries_the_evidence_for_the_mail() {
        let throttle = throttle(identity_only(ThrottleConfig {
            notify_after: 2,
            notify_window: Duration::from_secs(900),
            ..ThrottleConfig::default()
        }));
        let ctx = ctx();

        throttle.record(&ctx, false).await.expect("recorded");
        assert!(
            throttle
                .notice("ada@example.com")
                .await
                .expect("asked")
                .is_none(),
            "one failure is somebody mistyping their own password"
        );

        throttle.record(&ctx, false).await.expect("recorded");
        let notice = throttle
            .notice("  ADA@Example.COM ")
            .await
            .expect("asked")
            .expect("past the threshold");

        assert_eq!(notice.kind(), SecurityNoticeKind::RepeatedSignInFailures);
        assert_eq!(
            notice.destination(),
            "ada@example.com",
            "the address is normalised, so the mail is not addressed to whatever was typed"
        );
        assert_eq!(notice.failures(), 2);
        assert_eq!(notice.window(), Duration::from_secs(900));
        assert_eq!(notice.recent().len(), 2, "the table the mail renders");
        assert_eq!(notice.recent()[0].ip.as_deref(), Some("203.0.113.7"));

        for _ in 0..3 {
            assert!(
                throttle
                    .notice("ada@example.com")
                    .await
                    .expect("asked")
                    .is_none(),
                "one email per window, not one per attempt"
            );
        }
    }

    #[tokio::test]
    async fn a_notice_needs_a_reachable_store_rather_than_reporting_nothing() {
        let kv = moso_kv::Kv::builder("throttle-test")
            .store(DownStore)
            .build()
            .expect("built");
        let throttle = LoginThrottle::new(kv, ThrottleConfig::default());

        assert!(throttle.notice("ada@example.com").await.is_err());
    }

    // ── the attempt list ──────────────────────────────────────────────────

    #[tokio::test]
    async fn recent_returns_the_newest_attempt_first_and_honours_the_limit() {
        let throttle = throttle(identity_only(ThrottleConfig::default()));
        let ctx = ctx();

        throttle.record(&ctx, false).await.expect("recorded");
        throttle.record(&ctx, false).await.expect("recorded");
        throttle.record(&ctx, true).await.expect("recorded");

        let all = throttle.recent("ada@example.com", 10).await.expect("read");
        assert_eq!(all.len(), 3);
        assert!(all[0].succeeded, "the newest attempt comes first");
        assert!(!all[1].succeeded);
        assert!(all[0].at >= all[1].at);
        assert_eq!(all[0].ip.as_deref(), Some("203.0.113.7"));

        let one = throttle.recent("ada@example.com", 1).await.expect("read");
        assert_eq!(one.len(), 1);
        assert!(one[0].succeeded);

        assert!(
            throttle
                .recent("ada@example.com", 0)
                .await
                .expect("read")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_attempt_list_is_bounded_so_it_cannot_be_grown_without_limit() {
        let throttle = throttle(identity_only(ThrottleConfig::default()));
        let ctx = ctx();

        for _ in 0..(ATTEMPT_HISTORY + 7) {
            throttle.record(&ctx, false).await.expect("recorded");
        }

        let all = throttle
            .recent("ada@example.com", u32::MAX)
            .await
            .expect("read");
        assert_eq!(all.len(), ATTEMPT_HISTORY);
    }

    #[tokio::test]
    async fn an_attempt_with_no_identity_has_no_per_identity_state_to_record() {
        let throttle = throttle(ThrottleConfig::default());
        let anonymous = AuthCtx::new().with_ip("203.0.113.7");

        throttle.record(&anonymous, false).await.expect("recorded");
        assert!(
            throttle
                .recent("ada@example.com", 10)
                .await
                .expect("read")
                .is_empty()
        );
    }

    // ── fail-closed ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_store_failure_is_unavailable_and_never_an_allow() {
        let kv = moso_kv::Kv::builder("throttle-test")
            .store(DownStore)
            .build()
            .expect("built");
        let throttle = LoginThrottle::new(kv, ThrottleConfig::default());

        match throttle.check(&ctx()).await {
            Err(Error::Unavailable { component, .. }) => {
                assert_eq!(component, "login throttle store");
            }
            Err(other) => panic!("the wrong error: {other}"),
            Ok(decision) => panic!("an unreachable store must not decide: {decision:?}"),
        }
    }

    #[tokio::test]
    async fn every_read_path_fails_closed_rather_than_answering_from_nothing() {
        let kv = moso_kv::Kv::builder("throttle-test")
            .store(DownStore)
            .build()
            .expect("built");
        let throttle = LoginThrottle::new(kv, ThrottleConfig::default());

        assert!(throttle.record(&ctx(), false).await.is_err());
        assert!(throttle.should_notify("ada@example.com").await.is_err());
        assert!(throttle.recent("ada@example.com", 10).await.is_err());
    }

    // ── the keyspace ──────────────────────────────────────────────────────

    #[test]
    fn no_key_carries_the_identity_or_the_address_in_the_clear() {
        let throttle = throttle(ThrottleConfig::default());
        let kv = &throttle.kv;

        let subject = subject_of("ada@example.com");
        assert_eq!(subject.len(), 64);
        assert!(subject.bytes().all(|byte| byte.is_ascii_hexdigit()));

        for key in [
            kv.key::<ThrottleFailures>(&subject).expect("short"),
            kv.key::<ThrottleLastFailure>(&subject).expect("short"),
            kv.key::<ThrottleWindow>(&subject).expect("short"),
            kv.key::<ThrottleNotified>(&subject).expect("short"),
            kv.key::<ThrottleAttempts>(&subject).expect("short"),
        ] {
            assert!(
                !key.as_str().contains("ada@example.com"),
                "{}",
                key.as_str()
            );
            assert!(key.as_str().contains(&subject), "{}", key.as_str());
        }

        let bucket = throttle.address_bucket(&ctx());
        assert!(!bucket.contains("203.0.113.7"), "{bucket}");
        assert!(bucket.starts_with(ADDRESS_SCOPE), "{bucket}");
    }

    #[test]
    fn the_address_quota_reads_the_period_as_the_refill_of_one() {
        let throttle = throttle(ThrottleConfig {
            per_ip_burst: 10,
            per_ip_period: Duration::from_secs(60),
            ..ThrottleConfig::default()
        });
        let quota = throttle.address_quota();

        assert_eq!(quota.burst, 10, "ten at once");
        assert_eq!(
            quota.emission_interval(),
            Duration::from_secs(60),
            "then one more a minute"
        );
    }

    #[test]
    fn a_throttle_never_prints_its_store() {
        let rendered = format!("{:?}", throttle(ThrottleConfig::default()));
        assert!(rendered.contains("LoginThrottle"), "{rendered}");
        assert!(rendered.contains("per_ip_burst"), "{rendered}");
    }
}
