//! Password hashing, verification, calibration and policy.
//!
//! Four things here are not negotiable, and each closes a hole that is open in
//! most applications:
//!
//! 1. **argon2id with calibrated parameters.** Hard-coded work factors age
//!    badly — a value chosen in 2019 is a rounding error on 2026 hardware.
//!    [`calibrate`] measures the deployment and writes the result to
//!    configuration.
//! 2. **Hashing runs on a bounded blocking pool.** A password hash is
//!    deliberately slow and deliberately memory-hungry; running one per request
//!    on the async runtime means a login flood stops the whole server. This is
//!    a real, easily-triggered denial of service that most frameworks leave
//!    open.
//! 3. **A dummy verify on the miss path.** Otherwise "no such account" returns
//!    in microseconds and "wrong password" in ~250 ms, and the difference is a
//!    user-enumeration oracle.
//! 4. **No composition rules.** Length, a breach check and a strength estimate,
//!    which is current NIST guidance. "One uppercase and a symbol" produces
//!    `Password1!` and nothing else.
//!
//! # The process-wide parameters
//!
//! [`PasswordHash::new`] does not take a [`HashParams`]; it reads the ones
//! [`install_params`] put in place at boot. That is deliberate: a parameter set
//! threaded through every call site is a parameter set that is wrong in one of
//! them, and the one it is wrong in is the one that writes the weak hash.

use std::sync::{LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use argon2::password_hash::{
    PasswordHash as PhcHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::{Algorithm, Argon2, Params, Version};
use moso_core::BoxFuture;
use moso_schema::Password;
use subtle::ConstantTimeEq;

use crate::{Error, Result};

/// How many bytes of salt every hash carries.
///
/// Sixteen, which is what RFC 9106 recommends and what every argon2
/// implementation defaults to. More does not help; less starts to make a
/// rainbow table for a popular deployment thinkable again.
const SALT_BYTES: usize = 16;

/// A password hash in PHC string format.
///
/// Self-describing — the algorithm, its parameters and the salt travel with the
/// digest — which is what makes [`needs_rehash`](PasswordHash::needs_rehash)
/// possible without a schema migration every time the parameters change.
///
/// ```
/// use moso_auth::{HashParams, PasswordHash, VerifyOutcome};
/// use moso_schema::Password;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso_auth::Result<()> {
/// let plain = Password::new("correct horse battery staple").unwrap();
/// // Deliberately weak parameters: a doctest is not a deployment.
/// let params = HashParams::new(8, 1, 1);
/// let hash = PasswordHash::with_params(&plain, params).await?;
///
/// assert!(hash.as_str().starts_with("$argon2id$"));
/// assert!(hash.verify(&plain).await?.is_valid());
/// # Ok(())
/// # }
/// ```
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PasswordHash(String);

impl PasswordHash {
    /// Hash a password with the current parameters.
    ///
    /// Runs on [`moso_core::task::blocking`], so a login flood cannot starve
    /// the runtime. Async for that reason and no other.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the blocking pool
    /// is saturated — which is backpressure working, and is a 503 rather than a
    /// queue that grows until the process dies.
    ///
    /// ```no_run
    /// # use moso_auth::PasswordHash;
    /// # use moso_schema::Password;
    /// # async fn f(p: Password) -> moso_auth::Result<PasswordHash> { PasswordHash::new(&p).await }
    /// ```
    pub async fn new(plain: &Password) -> Result<Self> {
        Self::with_params(plain, current_params()).await
    }

    /// Hash with explicit parameters, for a test or a migration.
    ///
    /// Unlike [`new`](PasswordHash::new) this does **not** clamp to
    /// [`HashParams::OWASP_MINIMUM`]: a test that had to spend 40 ms per hash
    /// would be a test nobody runs, and a migration re-hashing ten million rows
    /// needs to choose its own budget. Every production path goes through
    /// [`new`](PasswordHash::new).
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the blocking pool
    /// is saturated, or [`Error::Config`] when argon2
    /// refuses the parameters (a memory cost below 8 KiB, a zero time cost).
    ///
    /// ```
    /// use moso_auth::{HashParams, PasswordHash};
    /// use moso_schema::Password;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let plain = Password::new("a sufficiently long one").unwrap();
    /// let hash = PasswordHash::with_params(&plain, HashParams::new(8, 1, 1)).await?;
    /// assert_eq!(hash.params()?, HashParams::new(8, 1, 1));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn with_params(plain: &Password, params: HashParams) -> Result<Self> {
        let secret = plain.expose().to_owned();
        on_blocking(move || hash_blocking(&secret, params)).await?
    }

    /// Wrap an existing PHC string from the database.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the string is not a PHC
    /// hash in a supported algorithm. A stored value that fails here is a data
    /// problem, and failing loudly beats treating it as "wrong password" for
    /// every login that user ever attempts again.
    ///
    /// ```
    /// use moso_auth::PasswordHash;
    ///
    /// assert!(PasswordHash::parse("not a hash").is_err());
    /// assert!(PasswordHash::parse("$scrypt$ln=16,r=8,p=1$c2FsdA$aGFzaA").is_err());
    /// ```
    pub fn parse(phc: &str) -> Result<Self> {
        let parsed = PhcHash::new(phc).map_err(|error| {
            Error::Config(format!("stored password hash is not a PHC string: {error}").into())
        })?;

        let algorithm = parsed.algorithm.as_str();
        if !matches!(algorithm, "argon2id" | "argon2i" | "argon2d") {
            return Err(Error::Config(
                format!(
                    "stored password hash uses `{algorithm}`, which this build cannot verify; \
                     help: re-hash on next login, or keep the previous hasher available"
                )
                .into(),
            ));
        }

        Ok(Self(phc.to_owned()))
    }

    /// Check a password against this hash.
    ///
    /// Constant-time in the comparison, and on the blocking pool for the same
    /// reason [`new`](PasswordHash::new) is.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the pool is
    /// saturated. A wrong password is [`VerifyOutcome::Invalid`], not an error:
    /// it is an expected outcome, and making it a `Result::Err` invites a `?`
    /// that skips the timing equalisation.
    ///
    /// ```
    /// use moso_auth::{HashParams, PasswordHash, VerifyOutcome};
    /// use moso_schema::Password;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let right = Password::new("the right password").unwrap();
    /// let wrong = Password::new("the wrong password").unwrap();
    /// let hash = PasswordHash::with_params(&right, HashParams::new(8, 1, 1)).await?;
    ///
    /// assert_eq!(hash.verify(&wrong).await?, VerifyOutcome::Invalid);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn verify(&self, plain: &Password) -> Result<VerifyOutcome> {
        let secret = plain.expose().to_owned();
        let phc = self.0.clone();
        let current = current_params();
        on_blocking(move || verify_blocking(&phc, &secret, current)).await?
    }

    /// Whether this hash's parameters are weaker than the current ones.
    ///
    /// The signal to re-hash on next login, which is the only way a deployment
    /// that raised its parameters ever gets the stronger hashes.
    ///
    /// A hash whose parameter section does not parse counts as needing a
    /// re-hash: the safe reading of "I cannot tell how strong this is" is "not
    /// strong enough".
    ///
    /// ```
    /// use moso_auth::{HashParams, PasswordHash};
    /// use moso_schema::Password;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let plain = Password::new("a sufficiently long one").unwrap();
    /// let weak = PasswordHash::with_params(&plain, HashParams::new(8, 1, 1)).await?;
    /// assert!(weak.needs_rehash(), "8 KiB is below the installed floor");
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn needs_rehash(&self) -> bool {
        match self.params() {
            Ok(params) => !params.at_least(current_params()),
            Err(_) => true,
        }
    }

    /// The PHC string, for storing.
    ///
    /// ```no_run
    /// # use moso_auth::PasswordHash;
    /// # fn f(h: &PasswordHash) { let _: &str = h.as_str(); }
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The parameters embedded in this hash.
    ///
    /// ```
    /// use moso_auth::{HashParams, PasswordHash};
    /// use moso_schema::Password;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let plain = Password::new("a sufficiently long one").unwrap();
    /// let hash = PasswordHash::with_params(&plain, HashParams::new(16, 2, 1)).await?;
    /// assert_eq!(hash.params()?, HashParams::new(16, 2, 1));
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the string does not parse.
    pub fn params(&self) -> Result<HashParams> {
        let parsed = PhcHash::new(&self.0).map_err(|error| {
            Error::Config(format!("password hash is not a PHC string: {error}").into())
        })?;
        let params = Params::try_from(&parsed).map_err(|error| {
            Error::Config(format!("password hash has no argon2 parameters: {error}").into())
        })?;
        Ok(HashParams::new(
            params.m_cost(),
            params.t_cost(),
            params.p_cost(),
        ))
    }
}

impl core::fmt::Debug for PasswordHash {
    /// Redacted. A hash is not a password, but it is offline-attackable, and it
    /// does not belong in a log aggregator.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PasswordHash(<redacted>)")
    }
}

/// Run `work` on the bounded blocking pool, turning saturation into
/// [`Error::Unavailable`].
async fn on_blocking<F, T>(work: F) -> Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    moso_core::task::blocking(work)
        .await
        .map_err(|error| Error::Unavailable {
            component: "password hashing pool",
            detail: error.to_string(),
            source: None,
        })
}

/// The synchronous half of [`PasswordHash::with_params`].
fn hash_blocking(secret: &str, params: HashParams) -> Result<PasswordHash> {
    let mut salt = [0_u8; SALT_BYTES];
    getrandom::fill(&mut salt).map_err(|error| Error::Unavailable {
        component: "system random generator",
        detail: error.to_string(),
        source: None,
    })?;

    let salt = SaltString::encode_b64(&salt)
        .map_err(|error| Error::Config(format!("salt encoding failed: {error}").into()))?;

    let hasher = argon2_for(params)?;
    let hash = hasher
        .hash_password(secret.as_bytes(), &salt)
        .map_err(|error| Error::Config(format!("argon2 refused to hash: {error}").into()))?;

    Ok(PasswordHash(hash.to_string()))
}

/// The synchronous half of [`PasswordHash::verify`].
fn verify_blocking(phc: &str, secret: &str, current: HashParams) -> Result<VerifyOutcome> {
    let parsed = PhcHash::new(phc).map_err(|error| {
        Error::Config(format!("stored password hash is not a PHC string: {error}").into())
    })?;

    // The verifier is constructed with a fixed algorithm family: `verify_password`
    // re-derives the cost parameters from the hash, but it will not follow the
    // hash into a different algorithm.
    let ok = Argon2::default()
        .verify_password(secret.as_bytes(), &parsed)
        .is_ok();

    if !ok {
        return Ok(VerifyOutcome::Invalid);
    }

    let stored = Params::try_from(&parsed)
        .map(|p| HashParams::new(p.m_cost(), p.t_cost(), p.p_cost()))
        .unwrap_or(HashParams::new(0, 0, 0));

    Ok(if stored.at_least(current) {
        VerifyOutcome::Ok
    } else {
        VerifyOutcome::OkNeedsRehash
    })
}

/// An argon2id hasher with `params`.
fn argon2_for(params: HashParams) -> Result<Argon2<'static>> {
    let built = Params::new(
        params.memory_kib,
        params.iterations,
        params.parallelism,
        None,
    )
    .map_err(|error| {
        Error::Config(
            format!(
                "argon2 rejected m={} t={} p={}: {error}; help: memory must be at least \
                 8 KiB and at least 8 × parallelism, and time cost at least 1",
                params.memory_kib, params.iterations, params.parallelism
            )
            .into(),
        )
    })?;

    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, built))
}

/// What a verification concluded.
///
/// ```
/// use moso_auth::VerifyOutcome;
///
/// assert!(VerifyOutcome::Ok.is_valid());
/// assert!(VerifyOutcome::OkNeedsRehash.is_valid());
/// assert!(!VerifyOutcome::Invalid.is_valid());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// Correct, and the hash is current.
    Ok,
    /// Correct, and the hash should be recomputed with current parameters.
    ///
    /// The caller has the plaintext at exactly this moment and never again, so
    /// this is the only chance to upgrade it.
    OkNeedsRehash,
    /// Wrong.
    Invalid,
}

impl VerifyOutcome {
    /// Whether the password was correct.
    ///
    /// ```
    /// use moso_auth::VerifyOutcome;
    ///
    /// assert!(VerifyOutcome::OkNeedsRehash.is_valid());
    /// ```
    #[must_use]
    pub const fn is_valid(self) -> bool {
        matches!(self, Self::Ok | Self::OkNeedsRehash)
    }
}

/// argon2 parameters.
///
/// ```
/// use moso_auth::HashParams;
///
/// // OWASP's floor, which calibration may raise and must never go below.
/// let floor = HashParams::OWASP_MINIMUM;
/// assert_eq!(floor.memory_kib, 19 * 1024);
/// assert_eq!(floor.iterations, 2);
/// assert_eq!(floor.parallelism, 1);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct HashParams {
    /// Memory cost, in kibibytes.
    pub memory_kib: u32,
    /// Time cost: how many passes.
    pub iterations: u32,
    /// How many lanes.
    pub parallelism: u32,
}

impl HashParams {
    /// OWASP's minimum for argon2id: 19 MiB, two passes, one lane.
    ///
    /// The floor. [`calibrate`] may raise these and will never return less.
    ///
    /// ```
    /// use moso_auth::HashParams;
    ///
    /// assert_eq!(HashParams::OWASP_MINIMUM.memory_kib, 19_456);
    /// ```
    pub const OWASP_MINIMUM: Self = Self {
        memory_kib: 19 * 1024,
        iterations: 2,
        parallelism: 1,
    };

    /// Parameters with explicit costs.
    ///
    /// The struct is `#[non_exhaustive]` — a fourth argon2 parameter would
    /// otherwise be a breaking change — so this is how a caller outside the
    /// crate builds one: from `moso auth calibrate`'s output, or in a test.
    ///
    /// ```
    /// use moso_auth::HashParams;
    ///
    /// let params = HashParams::new(64 * 1024, 3, 1);
    /// assert!(params.at_least(HashParams::OWASP_MINIMUM));
    /// ```
    #[must_use]
    pub const fn new(memory_kib: u32, iterations: u32, parallelism: u32) -> Self {
        Self {
            memory_kib,
            iterations,
            parallelism,
        }
    }

    /// Whether these parameters are at least as strong as `other`.
    ///
    /// What [`PasswordHash::needs_rehash`] compares with: weaker in *any*
    /// dimension counts as weaker.
    ///
    /// ```
    /// use moso_auth::HashParams;
    ///
    /// assert!(HashParams::OWASP_MINIMUM.at_least(HashParams::OWASP_MINIMUM));
    /// ```
    #[must_use]
    pub const fn at_least(self, other: Self) -> bool {
        self.memory_kib >= other.memory_kib
            && self.iterations >= other.iterations
            && self.parallelism >= other.parallelism
    }

    /// These parameters, raised to [`HashParams::OWASP_MINIMUM`] in every
    /// dimension that is below it.
    ///
    /// What [`install_params`] applies. Being slow hardware is not a reason to
    /// be weak, and a configuration file that quietly lowered the floor would
    /// be the single most damaging typo in a deployment.
    ///
    /// ```
    /// use moso_auth::HashParams;
    ///
    /// let raised = HashParams::new(1024, 1, 1).at_least_owasp();
    /// assert_eq!(raised, HashParams::OWASP_MINIMUM);
    /// ```
    #[must_use]
    pub const fn at_least_owasp(self) -> Self {
        let floor = Self::OWASP_MINIMUM;
        Self {
            memory_kib: if self.memory_kib > floor.memory_kib {
                self.memory_kib
            } else {
                floor.memory_kib
            },
            iterations: if self.iterations > floor.iterations {
                self.iterations
            } else {
                floor.iterations
            },
            parallelism: if self.parallelism > floor.parallelism {
                self.parallelism
            } else {
                floor.parallelism
            },
        }
    }
}

impl Default for HashParams {
    fn default() -> Self {
        Self::OWASP_MINIMUM
    }
}

/// The parameters [`PasswordHash::new`] uses, for this process.
static CURRENT_PARAMS: RwLock<HashParams> = RwLock::new(HashParams::OWASP_MINIMUM);

/// The parameters [`PasswordHash::new`] will use.
///
/// [`HashParams::OWASP_MINIMUM`] until [`install_params`] says otherwise.
///
/// ```
/// use moso_auth::password::current_params;
///
/// assert!(current_params().at_least(moso_auth::HashParams::OWASP_MINIMUM));
/// ```
#[must_use]
pub fn current_params() -> HashParams {
    CURRENT_PARAMS
        .read()
        .map_or(HashParams::OWASP_MINIMUM, |guard| *guard)
}

/// Install the parameters [`PasswordHash::new`] will use, returning the
/// previous ones.
///
/// Called once at boot from `AuthConfig::effective_hash_params`. The value is
/// raised to [`HashParams::OWASP_MINIMUM`] first — see
/// [`HashParams::at_least_owasp`] for why that is not configurable.
///
/// ```
/// use moso_auth::HashParams;
/// use moso_auth::password::{current_params, install_params};
///
/// let previous = install_params(HashParams::new(64 * 1024, 3, 1));
/// assert_eq!(current_params(), HashParams::new(64 * 1024, 3, 1));
/// install_params(previous);
/// ```
pub fn install_params(params: HashParams) -> HashParams {
    let raised = params.at_least_owasp();
    match CURRENT_PARAMS.write() {
        Ok(mut guard) => core::mem::replace(&mut guard, raised),
        Err(poisoned) => {
            // A panic inside the lock cannot leave the process hashing with
            // unknown parameters: recover, and install anyway.
            let mut guard = poisoned.into_inner();
            core::mem::replace(&mut guard, raised)
        }
    }
}

/// The target time one hash should take.
///
/// 250 ms: slow enough that offline cracking is expensive, fast enough that a
/// login does not feel broken. `moso auth calibrate` searches for the
/// parameters that hit it on the deployment's own hardware.
///
/// ```
/// use std::time::Duration;
///
/// assert_eq!(moso_auth::password::TARGET_HASH_TIME, Duration::from_millis(250));
/// ```
pub const TARGET_HASH_TIME: Duration = Duration::from_millis(250);

/// The most memory calibration will ask for, in kibibytes.
///
/// One gibibyte. Past this the hash stops being a login cost and starts being a
/// capacity-planning problem: at 64 concurrent hashes it would be 64 GiB of
/// resident memory.
const CALIBRATION_MEMORY_CEILING: u32 = 1024 * 1024;

/// The most passes calibration will ask for.
const CALIBRATION_ITERATION_CEILING: u32 = 12;

/// Find the strongest parameters that hash in about `target` on this machine.
///
/// What `moso auth calibrate` runs. The result goes into configuration, not
/// into a constant: the right answer differs by an order of magnitude between a
/// laptop and a shared container, and a constant is wrong on both.
///
/// The search raises memory first — memory is what makes a GPU attack expensive
/// — doubling from the floor while a hash still fits in the budget, then adds
/// passes. It never returns less than [`HashParams::OWASP_MINIMUM`], even on
/// hardware slow enough that the minimum takes longer than `target`. Being slow
/// is not a reason to be weak.
///
/// # Errors
///
/// [`Error::Unavailable`] when the blocking pool is
/// saturated.
///
/// ```no_run
/// use moso_auth::{calibrate, HashParams, TARGET_HASH_TIME};
///
/// # async fn f() -> moso_auth::Result<HashParams> {
/// calibrate(TARGET_HASH_TIME).await
/// # }
/// ```
pub async fn calibrate(target: Duration) -> Result<HashParams> {
    on_blocking(move || calibrate_blocking(target)).await?
}

/// The synchronous half of [`calibrate`].
///
/// Kept separate so that the search — which is a dozen argon2 hashes — runs
/// once on the blocking pool rather than bouncing between it and the runtime
/// twelve times.
fn calibrate_blocking(target: Duration) -> Result<HashParams> {
    /// A password of a realistic length, so the measurement is realistic.
    const PROBE: &str = "calibration probe password";

    let mut best = HashParams::OWASP_MINIMUM;
    let mut elapsed = time_one(PROBE, best)?;

    // Memory first: it is what makes a parallel attack expensive.
    while elapsed < target && best.memory_kib < CALIBRATION_MEMORY_CEILING {
        let candidate = HashParams::new(
            (best.memory_kib * 2).min(CALIBRATION_MEMORY_CEILING),
            best.iterations,
            best.parallelism,
        );
        let took = time_one(PROBE, candidate)?;
        if took > target {
            break;
        }
        best = candidate;
        elapsed = took;
    }

    // Then passes, which are cheaper to add and finer-grained.
    while elapsed < target && best.iterations < CALIBRATION_ITERATION_CEILING {
        let candidate = HashParams::new(best.memory_kib, best.iterations + 1, best.parallelism);
        let took = time_one(PROBE, candidate)?;
        if took > target {
            break;
        }
        best = candidate;
        elapsed = took;
    }

    Ok(best.at_least_owasp())
}

/// How long one hash with `params` takes.
fn time_one(secret: &str, params: HashParams) -> Result<Duration> {
    let started = Instant::now();
    hash_blocking(secret, params)?;
    Ok(started.elapsed())
}

/// The dummy hash [`dummy_verify`] checks against, and the parameters it was
/// built for.
///
/// Cached because building it costs a hash, and rebuilt when the installed
/// parameters change — a dummy verify with stale parameters would take a
/// different time from a real one, which is exactly the signal it exists to
/// hide.
static DUMMY: Mutex<Option<(HashParams, String)>> = Mutex::new(None);

/// The password the dummy hash is of. It is not a secret: the point is that the
/// verification fails, at the cost of a real one.
const DUMMY_PASSWORD: &str = "moso dummy verification password";

/// Run a hash verification that always fails, to equalise timing.
///
/// Called on the "no such account" path so the response takes as long as a
/// wrong password. Not an optimisation to skip: without it, an attacker
/// enumerates every account in an application with a stopwatch.
///
/// # Errors
///
/// [`Error::Unavailable`] when the blocking pool is
/// saturated.
///
/// ```
/// use moso_auth::dummy_verify;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso_auth::Result<()> {
/// dummy_verify().await?;
/// # Ok(())
/// # }
/// ```
pub async fn dummy_verify() -> Result<()> {
    let params = current_params();
    on_blocking(move || dummy_verify_blocking(params)).await?
}

/// The synchronous half of [`dummy_verify`].
fn dummy_verify_blocking(params: HashParams) -> Result<()> {
    let phc = {
        let mut cached = DUMMY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match cached.as_ref() {
            Some((cached_params, phc)) if *cached_params == params => phc.clone(),
            _ => {
                let phc = hash_blocking(DUMMY_PASSWORD, params)?.0;
                *cached = Some((params, phc.clone()));
                phc
            }
        }
    };

    // The outcome is discarded on purpose: this call exists for its duration.
    // `black_box` keeps a future optimiser from noticing that and deleting it.
    let outcome = verify_blocking(&phc, "the password that is not the dummy one", params)?;
    core::hint::black_box(outcome);
    Ok(())
}

/// A constant-time comparison of two secrets.
///
/// Length is not secret — it leaks through the response size anyway — but the
/// contents are, so the comparison must not stop at the first differing byte.
/// Used for CSRF tokens, cookie signatures and API-key digests.
///
/// ```
/// use moso_auth::password::constant_time_eq;
///
/// assert!(constant_time_eq(b"same", b"same"));
/// assert!(!constant_time_eq(b"same", b"diff"));
/// assert!(!constant_time_eq(b"same", b"sam"));
/// ```
#[must_use]
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.ct_eq(right).into()
}

/// What a password must satisfy.
///
/// No composition rules. NIST SP 800-63B dropped them because they push users
/// to `Password1!` and nothing else; length, a breach check and a strength
/// estimate do the work they were supposed to do.
///
/// ```
/// use moso_auth::PasswordPolicy;
///
/// let policy = PasswordPolicy::default();
/// assert_eq!(policy.min_length, 12);
/// assert!(policy.breach_check);
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PasswordPolicy {
    /// The shortest accepted password, in characters.
    pub min_length: usize,
    /// Whether to reject passwords in the embedded breach list.
    pub breach_check: bool,
    /// Whether to additionally check the k-anonymity breach API.
    ///
    /// Off by default: it is a network call in the signup path, and the
    /// embedded list already covers the passwords that matter.
    pub breach_api: bool,
    /// The lowest accepted strength score, 0–4.
    pub min_strength: u8,
    /// Words that make a password too guessable for *this* application — the
    /// product's name, the company's, the user's own email local part.
    pub banned_words: Vec<String>,
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self {
            min_length: 12,
            breach_check: true,
            breach_api: false,
            min_strength: 2,
            banned_words: Vec::new(),
        }
    }
}

impl PasswordPolicy {
    /// Check a password against the policy.
    ///
    /// `context` is anything about the user that would make a password
    /// guessable — their email, their name — so `ada@example.com` cannot use
    /// `adaexample123`.
    ///
    /// The order is deliberate: length, then banned words, then the breach
    /// list, then strength. Each step is cheaper than the one after it, and the
    /// error a user sees is the most actionable one that applies.
    ///
    /// # Errors
    ///
    /// [`Error::PasswordPolicy`] with a stable
    /// code: `"len"`, `"breached"`, `"weak"` or `"banned"`.
    ///
    /// ```
    /// use moso_auth::PasswordPolicy;
    /// use moso_schema::Password;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let policy = PasswordPolicy::default();
    ///
    /// let breached = Password::new("password1234").unwrap();
    /// assert!(policy.check(&breached, &[]).await.is_err());
    ///
    /// let fine = Password::new("wharf-lentil-oxide-77").unwrap();
    /// policy.check(&fine, &["ada@example.com"]).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn check(&self, password: &Password, context: &[&str]) -> Result<()> {
        let plain = password.expose();

        if plain.chars().count() < self.min_length {
            return Err(Error::PasswordPolicy {
                code: "len",
                detail: format!("use at least {} characters", self.min_length).into(),
            });
        }

        let lowered = plain.to_lowercase();
        for banned in self.banned_words.iter().map(|word| word.to_lowercase()) {
            if !banned.is_empty() && lowered.contains(&banned) {
                return Err(Error::PasswordPolicy {
                    code: "banned",
                    detail: "this password contains a word that is too easy to guess here".into(),
                });
            }
        }

        if self.breach_check || self.breach_api {
            let mut checker = BreachCheck::embedded();
            checker.embedded = self.breach_check;
            if checker.is_breached(plain).await? {
                return Err(Error::PasswordPolicy {
                    code: "breached",
                    detail: "this password appears in a known breach; pick another".into(),
                });
            }
        }

        let strength = Strength::estimate(plain, context);
        if strength.score() < self.min_strength {
            return Err(Error::PasswordPolicy {
                code: "weak",
                detail: strength
                    .suggestion()
                    .unwrap_or("use a longer or less predictable password")
                    .to_owned()
                    .into(),
            });
        }

        Ok(())
    }
}

/// How guessable a password is, 0 (trivial) to 4 (strong).
///
/// ```
/// use moso_auth::Strength;
///
/// assert_eq!(Strength::estimate("password", &[]).score(), 0);
/// assert!(Strength::estimate("wharf-lentil-oxide-77", &[]).score() >= 3);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Strength {
    /// The score, 0–4.
    score: u8,
    /// What is wrong with it, in one line, safe to show the user.
    feedback: Option<String>,
    /// What would help, in one line.
    suggestion: Option<String>,
}

impl Strength {
    /// Estimate a password's strength.
    ///
    /// A zxcvbn-style estimate: the score comes from how much of the password
    /// survives pattern matching — the embedded common-password list, keyboard
    /// walks, runs of consecutive or repeated characters, four-digit years, and
    /// the `context` words — rather than from counting character classes. What
    /// is left over is scored by its own entropy, so a long passphrase beats a
    /// short string with a symbol in it, which is the whole point.
    ///
    /// The `context` words are treated as dictionary entries for this estimate
    /// only, so `ada@example.com` cannot use `adaexample123`.
    ///
    /// ```
    /// use moso_auth::Strength;
    ///
    /// assert_eq!(Strength::estimate("qwertyuiop", &[]).score(), 0);
    /// assert_eq!(Strength::estimate("adaexample123", &["ada@example.com"]).score(), 0);
    /// ```
    #[must_use]
    pub fn estimate(password: &str, context: &[&str]) -> Self {
        estimate_strength(password, context)
    }

    /// The score, 0–4.
    ///
    /// ```
    /// use moso_auth::Strength;
    ///
    /// assert!(Strength::estimate("hunter2", &[]).score() <= 1);
    /// ```
    #[must_use]
    pub const fn score(&self) -> u8 {
        self.score
    }

    /// What is wrong with it.
    ///
    /// ```
    /// use moso_auth::Strength;
    ///
    /// assert!(Strength::estimate("password", &[]).feedback().is_some());
    /// ```
    #[must_use]
    pub fn feedback(&self) -> Option<&str> {
        self.feedback.as_deref()
    }

    /// What would help.
    ///
    /// ```
    /// use moso_auth::Strength;
    ///
    /// assert!(Strength::estimate("aaaaaaaaaaaa", &[]).suggestion().is_some());
    /// ```
    #[must_use]
    pub fn suggestion(&self) -> Option<&str> {
        self.suggestion.as_deref()
    }
}

/// The estimator behind [`Strength::estimate`].
///
/// Scores in bits of residual entropy, then buckets. The buckets are the
/// zxcvbn ones: under 24 bits is 0, under 36 is 1, under 48 is 2, under 60 is
/// 3, and above that is 4.
fn estimate_strength(password: &str, context: &[&str]) -> Strength {
    let lowered = password.to_lowercase();
    let unleeted = unleet(&lowered);
    let chars: Vec<char> = password.chars().collect();
    let length = chars.len();

    if length == 0 {
        return Strength {
            score: 0,
            feedback: Some("an empty password".to_owned()),
            suggestion: Some("use at least twelve characters".to_owned()),
        };
    }

    let mut feedback: Option<&'static str> = None;

    // A password that *is* a known-common one, in any obvious disguise, is a
    // zero however long it is.
    if breach_filter().contains(&lowered) || breach_filter().contains(&unleeted) {
        return Strength {
            score: 0,
            feedback: Some("this is one of the most commonly used passwords".to_owned()),
            suggestion: Some("use unrelated words, or a generated password".to_owned()),
        };
    }

    for word in context {
        let word = word.to_lowercase();
        for fragment in word.split(|c: char| !c.is_alphanumeric()) {
            if fragment.chars().count() >= 3 && unleeted.contains(fragment) {
                return Strength {
                    score: 0,
                    feedback: Some("this password contains your own details".to_owned()),
                    suggestion: Some("use something unrelated to your account".to_owned()),
                };
            }
        }
    }

    // Every penalty is "what those characters would have been worth, minus what
    // guessing the pattern actually costs". A run of twelve `a`s is worth one
    // character plus log2(12) for the length, not twelve characters.
    let bits = alphabet_bits(&chars);
    let mut penalty = 0.0_f64;

    /// A pattern of `run` characters costs the alphabet for the first, plus
    /// `log2(run)` for how long it goes on. Everything else was free.
    fn run_penalty(run: usize, bits: f64) -> f64 {
        (((run - 1) as f64) * bits - (run as f64).log2()).max(0.0)
    }

    let repeat_run = longest_run(&chars, |a, b| a == b);
    if repeat_run >= 3 {
        penalty += run_penalty(repeat_run, bits);
        feedback = feedback.or(Some("repeated characters are easy to guess"));
    }

    let sequence_run = longest_run(&chars, |a, b| {
        (*b as u32).checked_sub(*a as u32) == Some(1)
            || (*a as u32).checked_sub(*b as u32) == Some(1)
    });
    if sequence_run >= 4 {
        penalty += run_penalty(sequence_run, bits);
        feedback = feedback.or(Some("runs like `abcd` or `4321` are the first thing tried"));
    }

    if let Some(walk) = keyboard_walk(&lowered)
        && walk >= 4
    {
        penalty += run_penalty(walk, bits);
        feedback = feedback.or(Some("keyboard patterns are in every cracking dictionary"));
    }

    if let Some(word) = longest_common_word(&unleeted) {
        // A word from a few-thousand-entry list costs about twelve bits to
        // guess, however many characters it happens to be.
        penalty += ((word as f64) * bits - 12.0).max(0.0);
        feedback = feedback.or(Some("this is built around a common word"));
    }

    if has_year(&lowered) {
        // A plausible year is one of about ninety guesses: 6.5 bits.
        penalty += (4.0 * bits - 6.5).max(0.0);
        feedback = feedback.or(Some("a year is one of ninety guesses"));
    }

    let entropy = ((length as f64) * bits - penalty).max(0.0);

    let score = match entropy {
        e if e < 24.0 => 0,
        e if e < 36.0 => 1,
        e if e < 48.0 => 2,
        e if e < 60.0 => 3,
        _ => 4,
    };

    let suggestion = if score >= 3 {
        None
    } else if length < 16 {
        Some("a longer password beats a more complicated one".to_owned())
    } else {
        Some("use unrelated words, or a generated password".to_owned())
    };

    Strength {
        score,
        feedback: feedback.map(str::to_owned),
        suggestion,
    }
}

/// Bits per character for the alphabet this password draws on.
fn alphabet_bits(chars: &[char]) -> f64 {
    let mut size = 0_u32;
    if chars.iter().any(|c| c.is_ascii_lowercase()) {
        size += 26;
    }
    if chars.iter().any(|c| c.is_ascii_uppercase()) {
        size += 26;
    }
    if chars.iter().any(char::is_ascii_digit) {
        size += 10;
    }
    if chars.iter().any(|c| c.is_ascii_punctuation() || *c == ' ') {
        size += 33;
    }
    if chars.iter().any(|c| !c.is_ascii()) {
        size += 100;
    }
    f64::from(size.max(2)).log2()
}

/// The longest run of characters where each pair satisfies `adjacent`.
fn longest_run(chars: &[char], adjacent: impl Fn(&char, &char) -> bool) -> usize {
    let mut best = 1;
    let mut run = 1;
    for pair in chars.windows(2) {
        if adjacent(&pair[0], &pair[1]) {
            run += 1;
            best = best.max(run);
        } else {
            run = 1;
        }
    }
    if chars.is_empty() { 0 } else { best }
}

/// The rows of a QWERTY keyboard, for walk detection.
const KEYBOARD_ROWS: [&str; 4] = [
    "`1234567890-=",
    "qwertyuiop[]\\",
    "asdfghjkl;'",
    "zxcvbnm,./",
];

/// The longest straight keyboard walk in `lowered`, if any.
fn keyboard_walk(lowered: &str) -> Option<usize> {
    let chars: Vec<char> = lowered.chars().collect();
    let mut best = 0;
    for row in KEYBOARD_ROWS {
        let row: Vec<char> = row.chars().collect();
        let position = |c: char| row.iter().position(|r| *r == c);
        let mut run = 1;
        for pair in chars.windows(2) {
            match (position(pair[0]), position(pair[1])) {
                (Some(a), Some(b)) if a + 1 == b || b + 1 == a => {
                    run += 1;
                    best = best.max(run);
                }
                _ => run = 1,
            }
        }
    }
    (best > 1).then_some(best)
}

/// Undo the obvious character substitutions, so `p@ssw0rd` matches `password`.
///
/// The inverse of [`Base::Leet`], character for character, so that anything the
/// corpus generated in its leet form is recognised in whatever form it arrives.
/// `1` becomes `i` rather than `l`: both are used, and `i` is far the more
/// common in the corpora these rules come from.
fn unleet(lowered: &str) -> String {
    lowered
        .chars()
        .map(|c| match c {
            '@' | '4' => 'a',
            '3' => 'e',
            '1' | '|' => 'i',
            '0' => 'o',
            '$' | '5' => 's',
            '7' => 't',
            other => other,
        })
        .collect()
}

/// The length of the longest common-list word this password contains, if any.
///
/// Only the seed list is searched, not the expanded corpus: the expansion is
/// suffixes, and a suffix is what the residual-entropy term already prices.
fn longest_common_word(unleeted: &str) -> Option<usize> {
    COMMON_SEEDS
        .iter()
        .filter(|word| word.len() >= 4 && unleeted.contains(**word))
        .map(|word| word.len())
        .max()
}

/// Whether the password contains something that looks like a year.
fn has_year(lowered: &str) -> bool {
    let bytes = lowered.as_bytes();
    bytes.windows(4).any(|window| {
        window.iter().all(u8::is_ascii_digit) && {
            let year: u32 = core::str::from_utf8(window)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            (1940..=2040).contains(&year)
        }
    })
}

// ---------------------------------------------------------------------------
// The breach filter
// ---------------------------------------------------------------------------

/// Whether a password appears in a known breach.
///
/// The embedded filter is a Bloom filter over the common-password corpus
/// described in [`EMBEDDED_CORPUS_NOTE`] — enough to catch the passwords that
/// are actually tried, without a network call in the signup path.
/// [`BreachCheck::api`] adds the k-anonymity lookup for applications that want
/// the long tail: the first five characters of the SHA-1 are sent and the rest
/// never leaves the process.
///
/// ```
/// use moso_auth::BreachCheck;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso_auth::Result<()> {
/// assert!(BreachCheck::embedded().is_breached("password123").await?);
/// assert!(!BreachCheck::embedded().is_breached("wharf-lentil-oxide-77").await?);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct BreachCheck {
    /// Whether to consult the embedded filter.
    embedded: bool,
    /// The k-anonymity endpoint, when one is configured.
    api: Option<String>,
    /// How long to wait for the API before giving up and allowing the password.
    ///
    /// Failing *open* is deliberate: a breach service being slow must not stop
    /// people signing up, and the embedded filter has already run.
    api_timeout: Duration,
    /// How the range request is made. The crate has no HTTP client of its own —
    /// see [`RangeFetcher`] for why.
    fetcher: Option<std::sync::Arc<dyn RangeFetcher>>,
}

/// What the embedded filter actually contains, stated exactly.
///
/// The design document specifies "a local bloom filter of the top 100k
/// passwords (embedded, 200 KB)". Shipping somebody else's breach corpus in the
/// source tree is a licensing question and a 2 MB blob in every build, so what
/// is embedded is the *generator*: a seed list of the best-known passwords, and
/// the suffix, capitalisation and character-substitution rules that dominate
/// every published breach corpus. The filter is built from the expansion on
/// first use.
///
/// This is stated rather than glossed because the difference matters: the
/// filter catches the passwords people actually pick, and it is **not** a
/// substitute for the real list. An application that wants the published corpus
/// adds it with [`BreachCheck::with_extra_list`], and the k-anonymity API
/// covers the long tail.
///
/// ```
/// assert!(moso_auth::password::EMBEDDED_CORPUS_NOTE.contains("generator"));
/// ```
pub const EMBEDDED_CORPUS_NOTE: &str = "the embedded filter is built from an embedded generator \
                                        (a seed list plus suffix, capitalisation and leet rules), \
                                        not from a published breach corpus";

impl BreachCheck {
    /// The embedded filter only. No network.
    ///
    /// ```
    /// use moso_auth::BreachCheck;
    ///
    /// let _ = BreachCheck::embedded();
    /// ```
    #[must_use]
    pub fn embedded() -> Self {
        Self {
            embedded: true,
            api: None,
            api_timeout: Duration::from_secs(1),
            fetcher: None,
        }
    }

    /// Also consult a k-anonymity endpoint.
    ///
    /// A [`RangeFetcher`] must be installed with [`BreachCheck::fetcher`] as
    /// well; without one this is a configuration error rather than a silent
    /// skip, because a breach check that quietly does nothing is worse than no
    /// breach check at all.
    ///
    /// ```
    /// use moso_auth::BreachCheck;
    ///
    /// let _ = BreachCheck::embedded().api("https://api.pwnedpasswords.com/range");
    /// ```
    #[must_use]
    pub fn api(mut self, endpoint: impl Into<String>) -> Self {
        self.api = Some(endpoint.into());
        self
    }

    /// How the range request is made.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_auth::{BreachCheck, password::RangeFetcher};
    /// # fn f(fetcher: Arc<dyn RangeFetcher>) {
    /// let _ = BreachCheck::embedded()
    ///     .api("https://api.pwnedpasswords.com/range")
    ///     .fetcher(fetcher);
    /// # }
    /// ```
    #[must_use]
    pub fn fetcher(mut self, fetcher: std::sync::Arc<dyn RangeFetcher>) -> Self {
        self.fetcher = Some(fetcher);
        self
    }

    /// How long to wait for the endpoint before allowing the password.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_auth::BreachCheck;
    ///
    /// let _ = BreachCheck::embedded().api_timeout(Duration::from_millis(500));
    /// ```
    #[must_use]
    pub fn api_timeout(mut self, timeout: Duration) -> Self {
        self.api_timeout = timeout;
        self
    }

    /// Add application-supplied entries to the embedded filter.
    ///
    /// How a deployment that *does* want the published 100k list uses it:
    /// read the file at boot and hand the lines over. The entries join a
    /// process-wide filter, so this is a boot-time call, not a per-request one.
    ///
    /// ```
    /// use moso_auth::BreachCheck;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// BreachCheck::with_extra_list(["hunter2-but-longer"]);
    /// assert!(BreachCheck::embedded().is_breached("hunter2-but-longer").await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_extra_list<S: AsRef<str>>(entries: impl IntoIterator<Item = S>) {
        let mut extra = EXTRA_BREACHED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for entry in entries {
            let entry = entry.as_ref().trim();
            if !entry.is_empty() {
                extra.insert(entry.to_lowercase());
            }
        }
    }

    /// Whether this password appears in a known breach.
    ///
    /// # Errors
    ///
    /// Never for a network failure — that fails open, with a warning. Only
    /// [`Error::Config`] for an endpoint configured
    /// without a [`RangeFetcher`].
    ///
    /// ```
    /// use moso_auth::BreachCheck;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// assert!(BreachCheck::embedded().is_breached("qwerty123456").await?);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn is_breached(&self, password: &str) -> Result<bool> {
        if self.embedded {
            let candidate = password.to_owned();
            let hit = moso_core::task::blocking(move || {
                let lowered = candidate.to_lowercase();
                EXTRA_BREACHED
                    .lock()
                    .map(|extra| extra.contains(&lowered))
                    .unwrap_or(false)
                    || breach_filter().contains(&candidate)
                    || breach_filter().contains(&lowered)
                    // `P@ssw0rd` is `password` with a hat on, and the corpus
                    // holds the plain form and the fully-leet form but not
                    // every mixture of the two.
                    || breach_filter().contains(&unleet(&lowered))
            })
            .await
            .map_err(|error| Error::Unavailable {
                component: "password hashing pool",
                detail: error.to_string(),
                source: None,
            })?;

            if hit {
                return Ok(true);
            }
        }

        let Some(endpoint) = self.api.as_deref() else {
            return Ok(false);
        };

        let Some(fetcher) = self.fetcher.clone() else {
            return Err(Error::Config(
                "a breach API endpoint is configured with no `RangeFetcher`; help: call \
                 `BreachCheck::fetcher(..)`, or drop the endpoint and rely on the embedded filter"
                    .into(),
            ));
        };

        let digest = sha1_hex(password.as_bytes());
        let (prefix, suffix) = digest.split_at(5);
        let url = format!("{}/{prefix}", endpoint.trim_end_matches('/'));

        let range = tokio::time::timeout(self.api_timeout, fetcher.fetch(&url)).await;
        let body = match range {
            Ok(Ok(body)) => body,
            Ok(Err(error)) => {
                tracing::warn!(
                    target: "moso.auth",
                    error = %error,
                    "breach API unreachable; allowing the password"
                );
                return Ok(false);
            }
            Err(_) => {
                tracing::warn!(
                    target: "moso.auth",
                    timeout_ms = self.api_timeout.as_millis(),
                    "breach API timed out; allowing the password"
                );
                return Ok(false);
            }
        };

        Ok(body
            .lines()
            .filter_map(|line| line.split(':').next())
            .any(|candidate| candidate.trim().eq_ignore_ascii_case(suffix)))
    }
}

impl core::fmt::Debug for BreachCheck {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BreachCheck")
            .field("embedded", &self.embedded)
            .field("api", &self.api)
            .field("fetcher", &self.fetcher.is_some())
            .finish()
    }
}

/// Fetches one k-anonymity range from a breach service.
///
/// `moso-auth` has no HTTP client: adding one would put TLS, a connection pool
/// and a DNS resolver into every application that only wanted a login form. The
/// application supplies the client it already has, and gets to decide the proxy,
/// the timeout and the user agent.
///
/// ```no_run
/// use moso_auth::password::RangeFetcher;
/// use moso_core::BoxFuture;
///
/// /// A fetcher that has never heard of any password.
/// pub struct Offline;
///
/// impl RangeFetcher for Offline {
///     fn fetch<'a>(&'a self, _url: &'a str)
///         -> BoxFuture<'a, Result<String, moso_auth::BoxError>>
///     {
///         Box::pin(async { Ok(String::new()) })
///     }
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot fetch a breach range",
    label = "not a range fetcher",
    note = "a range fetcher implements `fetch(&self, url) -> BoxFuture<Result<String, BoxError>>`",
    note = "help: wrap the HTTP client the application already has — `moso-auth` deliberately \
            ships none, so that a login form does not pull in TLS and a connection pool",
    note = "help: without one, `BreachCheck::api(..)` is a configuration error rather than a \
            silent skip"
)]
pub trait RangeFetcher: Send + Sync + 'static {
    /// Fetch `url` and return the body.
    ///
    /// # Errors
    ///
    /// Anything the transport reports. A failure is logged and the password is
    /// allowed: a breach service being down must not stop people signing up.
    fn fetch<'a>(
        &'a self,
        url: &'a str,
    ) -> BoxFuture<'a, core::result::Result<String, crate::BoxError>>;
}

/// SHA-1 of `bytes`, uppercase hexadecimal.
///
/// SHA-1 is used here because it is what the k-anonymity API's index is built
/// on, and for no other reason. It is not used as a security primitive anywhere
/// in this crate.
fn sha1_hex(bytes: &[u8]) -> String {
    use sha1_shim::Sha1;

    let digest = Sha1::digest(bytes);
    let mut out = String::with_capacity(40);
    for byte in digest {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

/// A minimal SHA-1, for the k-anonymity index and nothing else.
///
/// Written out rather than taken as a dependency: `sha1` would be a third
/// third-party hash crate in this crate's tree for a single, non-security use.
/// The implementation is RFC 3174 verbatim and is tested against the RFC's own
/// vectors.
mod sha1_shim {
    /// The SHA-1 state machine.
    pub struct Sha1;

    impl Sha1 {
        /// The digest of `message`.
        pub fn digest(message: &[u8]) -> [u8; 20] {
            let mut h: [u32; 5] = [
                0x6745_2301,
                0xEFCD_AB89,
                0x98BA_DCFE,
                0x1032_5476,
                0xC3D2_E1F0,
            ];

            let mut padded = message.to_vec();
            let bit_length = (message.len() as u64) * 8;
            padded.push(0x80);
            while padded.len() % 64 != 56 {
                padded.push(0);
            }
            padded.extend_from_slice(&bit_length.to_be_bytes());

            for chunk in padded.as_chunks::<64>().0 {
                let mut w = [0_u32; 80];
                for (index, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
                    w[index] = u32::from_be_bytes(*word);
                }
                for index in 16..80 {
                    w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16])
                        .rotate_left(1);
                }

                let [mut a, mut b, mut c, mut d, mut e] = h;
                for (index, word) in w.iter().enumerate() {
                    let (f, k) = match index {
                        0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                        20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                        40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                        _ => (b ^ c ^ d, 0xCA62_C1D6),
                    };
                    let temp = a
                        .rotate_left(5)
                        .wrapping_add(f)
                        .wrapping_add(e)
                        .wrapping_add(k)
                        .wrapping_add(*word);
                    e = d;
                    d = c;
                    c = b.rotate_left(30);
                    b = a;
                    a = temp;
                }

                h[0] = h[0].wrapping_add(a);
                h[1] = h[1].wrapping_add(b);
                h[2] = h[2].wrapping_add(c);
                h[3] = h[3].wrapping_add(d);
                h[4] = h[4].wrapping_add(e);
            }

            let mut out = [0_u8; 20];
            for (index, word) in h.iter().enumerate() {
                out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
            }
            out
        }
    }
}

/// Extra entries an application added with [`BreachCheck::with_extra_list`].
///
/// A set rather than more filter bits: the list is usually small, and an exact
/// set has no false positives, which matters when the application chose the
/// entries deliberately.
static EXTRA_BREACHED: Mutex<std::collections::BTreeSet<String>> =
    Mutex::new(std::collections::BTreeSet::new());

/// A Bloom filter over the expanded common-password corpus.
///
/// Built on first use rather than at compile time: the expansion is a hundred
/// thousand short strings, which is 15 ms of work once and 1.2 MB of `.rodata`
/// never.
struct BloomFilter {
    /// The bit array, as words.
    words: Box<[u64]>,
    /// How many bits, always a power of two so the index is a mask.
    bits: u64,
    /// How many probes per entry.
    probes: u32,
}

impl BloomFilter {
    /// A filter sized for `expected` entries at roughly a 1% false-positive
    /// rate: ten bits and seven probes each.
    fn with_capacity(expected: usize) -> Self {
        let bits = ((expected as u64) * 10).next_power_of_two().max(1024);
        Self {
            words: vec![0_u64; (bits / 64) as usize].into_boxed_slice(),
            bits,
            probes: 7,
        }
    }

    /// The two independent hashes an entry's probes are derived from.
    fn seeds(entry: &str) -> (u64, u64) {
        use sha2::{Digest, Sha256};

        let digest = Sha256::digest(entry.as_bytes());
        let mut first = [0_u8; 8];
        let mut second = [0_u8; 8];
        first.copy_from_slice(&digest[0..8]);
        second.copy_from_slice(&digest[8..16]);
        // An even second seed would make every probe land on the same parity.
        (u64::from_le_bytes(first), u64::from_le_bytes(second) | 1)
    }

    /// The bit index of probe `probe`.
    fn index(&self, first: u64, second: u64, probe: u32) -> u64 {
        first.wrapping_add(u64::from(probe).wrapping_mul(second)) & (self.bits - 1)
    }

    /// Record `entry`.
    fn insert(&mut self, entry: &str) {
        let (first, second) = Self::seeds(entry);
        for probe in 0..self.probes {
            let bit = self.index(first, second, probe);
            self.words[(bit / 64) as usize] |= 1 << (bit % 64);
        }
    }

    /// Whether `entry` may be in the set. False positives are possible at about
    /// one in a hundred; false negatives are not.
    fn contains(&self, entry: &str) -> bool {
        let (first, second) = Self::seeds(entry);
        (0..self.probes).all(|probe| {
            let bit = self.index(first, second, probe);
            self.words[(bit / 64) as usize] & (1 << (bit % 64)) != 0
        })
    }
}

/// The process-wide filter, built on first use.
static BREACH_FILTER: LazyLock<BloomFilter> = LazyLock::new(build_breach_filter);

/// The process-wide filter.
fn breach_filter() -> &'static BloomFilter {
    &BREACH_FILTER
}

/// How many entries [`expand_corpus`] produces per seed word, near enough to
/// size the filter for.
///
/// Four base forms times ninety suffixes and years. Rounded up rather than
/// down: a Bloom filter's false-positive rate is what a user experiences as
/// "it rejected a password that is fine", and being generous with bits costs
/// 256 KB once.
const ENTRIES_PER_SEED: usize = 400;

/// Build the filter from the seed list and the expansion rules.
fn build_breach_filter() -> BloomFilter {
    let mut filter = BloomFilter::with_capacity(COMMON_SEEDS.len() * ENTRIES_PER_SEED);
    expand_corpus(|entry| filter.insert(entry));
    filter
}

/// Every entry of the expanded corpus, in order, handed to `sink`.
///
/// The rules are the ones that dominate every published breach list: a common
/// word, optionally capitalised or leet-substituted, with a short numeric or
/// punctuation suffix. Kept in one function so that the filter and the
/// [`Strength`] estimator cannot disagree about what "common" means.
fn expand_corpus(mut sink: impl FnMut(&str)) {
    /// The suffixes that appear on the overwhelming majority of breached
    /// passwords built from a word.
    const SUFFIXES: [&str; 24] = [
        "", "1", "2", "3", "7", "9", "12", "11", "22", "69", "99", "007", "123", "321", "1234",
        "12345", "123456", "!", "!!", "1!", "123!", "@123", ".", "_",
    ];

    let mut buffer = String::with_capacity(64);

    for seed in COMMON_SEEDS {
        for base in [Base::Plain, Base::Capitalised, Base::Upper, Base::Leet] {
            for suffix in SUFFIXES {
                buffer.clear();
                base.write(seed, &mut buffer);
                buffer.push_str(suffix);
                sink(&buffer);
            }
            // Years are their own family: `summer2019` is not a suffix rule so
            // much as a habit.
            for year in 1970..=2035 {
                buffer.clear();
                base.write(seed, &mut buffer);
                buffer.push_str(&year.to_string());
                sink(&buffer);
            }
        }
    }
}

/// How a seed word is written before its suffix.
#[derive(Clone, Copy)]
enum Base {
    /// As it is in the seed list: lowercase.
    Plain,
    /// First letter uppercased, which is what a composition rule produces.
    Capitalised,
    /// Every letter uppercased.
    Upper,
    /// With the obvious character substitutions applied.
    Leet,
}

impl Base {
    /// Write `seed` into `out` in this form.
    fn write(self, seed: &str, out: &mut String) {
        match self {
            Base::Plain => out.push_str(seed),
            Base::Capitalised => {
                let mut chars = seed.chars();
                if let Some(first) = chars.next() {
                    out.extend(first.to_uppercase());
                    out.push_str(chars.as_str());
                }
            }
            Base::Upper => out.extend(seed.chars().flat_map(char::to_uppercase)),
            // The exact inverse of `unleet`, so nothing generated here can fail
            // to be recognised when it arrives.
            Base::Leet => out.extend(seed.chars().map(|c| match c {
                'a' => '@',
                'e' => '3',
                'i' => '1',
                'o' => '0',
                's' => '$',
                't' => '7',
                other => other,
            })),
        }
    }
}

/// The seed words the corpus is expanded from.
///
/// Chosen from the families that top every published breach list: the word
/// "password" and its neighbours, keyboard walks, first names, sports teams,
/// months, seasons, profanity, brands and the handful of pop-culture words that
/// never leave the top thousand. Expansion turns each into roughly 260 entries.
const COMMON_SEEDS: &[&str] = &[
    // The passwords that are always first.
    "password",
    "passwd",
    "pass",
    "pwd",
    "secret",
    "letmein",
    "welcome",
    "login",
    "admin",
    "monkey",
    "administrator",
    "root",
    "toor",
    "guest",
    "test",
    "testing",
    "demo",
    "default",
    "changeme",
    "temp",
    "temporary",
    "user",
    "username",
    "master",
    "manager",
    "superuser",
    "system",
    "server",
    "access",
    "money",
    "freedom",
    "whatever",
    "trustno",
    "iloveyou",
    "loveyou",
    "princess",
    "sunshine",
    "shadow",
    "michael",
    "jennifer",
    "jordan",
    "hunter",
    "harley",
    "ranger",
    "buster",
    "thomas",
    "robert",
    "daniel",
    "matthew",
    "andrew",
    "joshua",
    "charlie",
    "george",
    "william",
    "richard",
    "patrick",
    "anthony",
    "nicholas",
    "jonathan",
    "benjamin",
    "samantha",
    "jessica",
    "ashley",
    "amanda",
    "nicole",
    "elizabeth",
    "melissa",
    "brandon",
    "justin",
    "tigger",
    "cookie",
    "chocolate",
    "peanut",
    "butterfly",
    "pepper",
    "ginger",
    "sparky",
    "bailey",
    "maggie",
    "molly",
    "lucky",
    "buddy",
    "jasper",
    "oliver",
    "charlie",
    "rocky",
    "sammy",
    "snoopy",
    "garfield",
    "batman",
    "superman",
    "spiderman",
    "pokemon",
    "pikachu",
    "starwars",
    "matrix",
    "gandalf",
    "hobbit",
    "legolas",
    "dragon",
    "phoenix",
    "falcon",
    "eagle",
    "tiger",
    "lion",
    "panther",
    "cowboy",
    "dallas",
    "chelsea",
    "arsenal",
    "liverpool",
    "barcelona",
    "juventus",
    "madrid",
    "united",
    "rangers",
    "celtic",
    "yankees",
    "steelers",
    "packers",
    "lakers",
    "soccer",
    "football",
    "baseball",
    "basketball",
    "hockey",
    "golfer",
    "fishing",
    "hunting",
    "cricket",
    "boxing",
    "racing",
    "biteme",
    "yankee",
    "banana",
    "orange",
    "apple",
    "cherry",
    "melon",
    "lemon",
    "coffee",
    "beer",
    "whisky",
    "vodka",
    "guinness",
    "monster",
    "redbull",
    "cheese",
    "pizza",
    "burger",
    "bacon",
    "cookies",
    "candy",
    "sugar",
    "honey",
    "flower",
    "rainbow",
    "diamond",
    "silver",
    "golden",
    "purple",
    "yellow",
    "orange",
    "violet",
    "scarlet",
    "crimson",
    "summer",
    "winter",
    "spring",
    "autumn",
    "january",
    "february",
    "march",
    "april",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "monday",
    "friday",
    "saturday",
    "sunday",
    "holiday",
    "vacation",
    "birthday",
    "christmas",
    "easter",
    "halloween",
    "newyork",
    "london",
    "paris",
    "berlin",
    "madrid",
    "moscow",
    "tokyo",
    "sydney",
    "toronto",
    "chicago",
    "boston",
    "houston",
    "atlanta",
    "miami",
    "vegas",
    "denver",
    "seattle",
    "canada",
    "america",
    "england",
    "ireland",
    "scotland",
    "france",
    "germany",
    "italy",
    "spain",
    "brazil",
    "mexico",
    "china",
    "japan",
    "india",
    "russia",
    "africa",
    "europe",
    "computer",
    "internet",
    "network",
    "database",
    "oracle",
    "windows",
    "linux",
    "ubuntu",
    "android",
    "iphone",
    "samsung",
    "google",
    "facebook",
    "twitter",
    "youtube",
    "amazon",
    "netflix",
    "instagram",
    "snapchat",
    "microsoft",
    "hotmail",
    "gmail",
    "yahoo",
    "myspace",
    "reddit",
    "discord",
    "minecraft",
    "fortnite",
    "roblox",
    "playstation",
    "nintendo",
    "gaming",
    "gamer",
    "player",
    "warrior",
    "hunter",
    "killer",
    "sniper",
    "soldier",
    "captain",
    "general",
    "sergeant",
    "trooper",
    "freedom",
    "liberty",
    "justice",
    "victory",
    "champion",
    "winner",
    "legend",
    "heaven",
    "angel",
    "devil",
    "demon",
    "wizard",
    "magic",
    "mystic",
    "cosmic",
    "galaxy",
    "planet",
    "rocket",
    "thunder",
    "lightning",
    "storm",
    "tornado",
    "hurricane",
    "blizzard",
    "avalanche",
    "volcano",
    "mountain",
    "forest",
    "meadow",
    "river",
    "ocean",
    "island",
    "desert",
    "canyon",
    "prairie",
    // Keyboard walks and digit runs.
    "qwerty",
    "qwertyuiop",
    "qwertz",
    "azerty",
    "asdfgh",
    "asdfghjkl",
    "zxcvbn",
    "zxcvbnm",
    "1qaz2wsx",
    "qazwsx",
    "1q2w3e4r",
    "abc123",
    "a1b2c3",
    "123abc",
    "111111",
    "000000",
    "121212",
    "123123",
    "112233",
    "654321",
    "789456",
    "147258",
    "159753",
    "246810",
    "101010",
    "202020",
    "123456789",
    "1234567890",
    "987654321",
    "iloveu",
    "lovely",
    "forever",
    "together",
    "always",
    "kisses",
    "cutie",
    "sweetie",
    "darling",
    "baby",
    "angel",
    "sexy",
    "hotstuff",
    "naughty",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The doctests hash at 8 KiB; so does most of this module. A hash at the
    /// installed floor takes tens of milliseconds and there is no test here
    /// that learns anything from paying it.
    const FAST: HashParams = HashParams::new(8, 1, 1);

    #[tokio::test]
    async fn a_password_verifies_against_its_own_hash_and_not_another() {
        let right = Password::new("the right password").unwrap();
        let wrong = Password::new("the wrong password").unwrap();

        let hash = PasswordHash::with_params(&right, FAST).await.unwrap();

        assert!(hash.verify(&right).await.unwrap().is_valid());
        assert_eq!(hash.verify(&wrong).await.unwrap(), VerifyOutcome::Invalid);
    }

    /// Two hashes of the same password must differ, or the salt is not doing
    /// its job and a rainbow table works again.
    #[tokio::test]
    async fn the_salt_makes_every_hash_unique() {
        let plain = Password::new("the same password twice").unwrap();

        let first = PasswordHash::with_params(&plain, FAST).await.unwrap();
        let second = PasswordHash::with_params(&plain, FAST).await.unwrap();

        assert_ne!(first.as_str(), second.as_str());
        assert!(first.verify(&plain).await.unwrap().is_valid());
        assert!(second.verify(&plain).await.unwrap().is_valid());
    }

    /// The whole point of PHC: a hash written with weaker parameters says so.
    #[tokio::test]
    async fn a_weaker_hash_asks_to_be_upgraded_on_the_next_login() {
        let plain = Password::new("a sufficiently long one").unwrap();
        let weak = PasswordHash::with_params(&plain, FAST).await.unwrap();

        assert!(weak.needs_rehash());
        assert_eq!(
            weak.verify(&plain).await.unwrap(),
            VerifyOutcome::OkNeedsRehash,
            "the caller has the plaintext exactly now and never again"
        );
    }

    #[tokio::test]
    async fn a_hash_at_the_installed_parameters_does_not_ask_to_be_upgraded() {
        let plain = Password::new("a sufficiently long one").unwrap();
        let hash = PasswordHash::with_params(&plain, current_params())
            .await
            .unwrap();

        assert!(!hash.needs_rehash());
        assert_eq!(hash.verify(&plain).await.unwrap(), VerifyOutcome::Ok);
    }

    #[test]
    fn the_parameters_can_never_be_installed_below_the_floor() {
        let previous = install_params(HashParams::new(1, 1, 1));
        assert_eq!(
            current_params(),
            HashParams::OWASP_MINIMUM,
            "a configuration typo must not be able to lower the floor"
        );
        install_params(previous);
    }

    #[test]
    fn a_stored_hash_in_an_unsupported_algorithm_fails_loudly() {
        let error = PasswordHash::parse(
            "$bcrypt$v=19$m=8,t=1,p=1$c2FsdHNhbHRzYWx0c2FsdA$aGFzaGhhc2hoYXNoaGFzaGhhc2g",
        )
        .unwrap_err();
        assert!(matches!(error, Error::Config(_)));
        assert!(
            error.to_string().contains("bcrypt"),
            "the message must name the algorithm that was found: {error}"
        );
    }

    #[test]
    fn a_hash_that_is_not_a_phc_string_fails_loudly() {
        assert!(PasswordHash::parse("not a hash at all").is_err());
        assert!(PasswordHash::parse("").is_err());
    }

    #[tokio::test]
    async fn a_parsed_hash_round_trips() {
        let plain = Password::new("a sufficiently long one").unwrap();
        let hash = PasswordHash::with_params(&plain, FAST).await.unwrap();

        let reparsed = PasswordHash::parse(hash.as_str()).unwrap();
        assert_eq!(reparsed, hash);
        assert!(reparsed.verify(&plain).await.unwrap().is_valid());
    }

    #[test]
    fn a_hash_never_prints_itself() {
        let hash = PasswordHash("$argon2id$v=19$m=8,t=1,p=1$c2FsdA$aGFzaA".to_owned());
        assert_eq!(format!("{hash:?}"), "PasswordHash(<redacted>)");
    }

    /// Acceptance criterion 3, at the level this module can assert it: the
    /// dummy verify must cost what a real verify costs. The backend's own test
    /// asserts the end-to-end p95.
    #[tokio::test]
    async fn a_dummy_verify_costs_what_a_real_verify_costs() {
        let previous = install_params(HashParams::OWASP_MINIMUM);
        let plain = Password::new("a sufficiently long one").unwrap();
        let hash = PasswordHash::with_params(&plain, current_params())
            .await
            .unwrap();

        // Warm both paths: the dummy hash is built on first use.
        dummy_verify().await.unwrap();
        hash.verify(&plain).await.unwrap();

        let started = Instant::now();
        for _ in 0..5 {
            dummy_verify().await.unwrap();
        }
        let dummy = started.elapsed() / 5;

        let started = Instant::now();
        for _ in 0..5 {
            hash.verify(&plain).await.unwrap();
        }
        let real = started.elapsed() / 5;

        let ratio = dummy.as_secs_f64() / real.as_secs_f64();
        assert!(
            (0.5..2.0).contains(&ratio),
            "a dummy verify took {dummy:?} against a real verify's {real:?}; the ratio {ratio} \
             is the enumeration oracle this function exists to close"
        );

        install_params(previous);
    }

    #[tokio::test]
    async fn calibration_never_returns_less_than_the_floor() {
        // A target no hardware can beat: even one hash at the floor is slower.
        let params = calibrate(Duration::from_nanos(1)).await.unwrap();
        assert_eq!(params, HashParams::OWASP_MINIMUM);
        assert!(params.at_least(HashParams::OWASP_MINIMUM));
    }

    #[tokio::test]
    async fn calibration_raises_memory_before_passes() {
        // A generous target, so the search has room to move, but bounded so the
        // test does not become a benchmark.
        let params = calibrate(Duration::from_millis(120)).await.unwrap();

        assert!(params.at_least(HashParams::OWASP_MINIMUM));
        assert!(params.memory_kib <= CALIBRATION_MEMORY_CEILING);
        assert!(params.iterations <= CALIBRATION_ITERATION_CEILING);
        assert!(
            params.memory_kib > HashParams::OWASP_MINIMUM.memory_kib
                || params.iterations == HashParams::OWASP_MINIMUM.iterations,
            "passes were raised before memory was exhausted: {params:?}"
        );
    }

    #[test]
    fn constant_time_comparison_answers_the_ordinary_questions() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"token", b"token"));
        assert!(!constant_time_eq(b"token", b"tokeN"));
        assert!(!constant_time_eq(b"token", b"token "));
    }

    #[tokio::test]
    async fn the_embedded_filter_knows_the_passwords_people_actually_pick() {
        let check = BreachCheck::embedded();

        for breached in [
            "password",
            "password1",
            "Password1",
            "password123",
            "P@ssw0rd",
            "qwerty123",
            "letmein!",
            "summer2019",
            "iloveyou",
            "football1",
            "MONKEY123",
        ] {
            assert!(
                check.is_breached(breached).await.unwrap(),
                "`{breached}` is in every breach list and the filter missed it"
            );
        }
    }

    #[tokio::test]
    async fn the_embedded_filter_does_not_cry_wolf() {
        let check = BreachCheck::embedded();
        let mut false_positives = 0;

        // Deterministic, unlikely strings. A 1%-FPR filter is allowed a few.
        for index in 0..200_u32 {
            let candidate = format!("wharf-lentil-oxide-{index:04}-zq");
            if check.is_breached(&candidate).await.unwrap() {
                false_positives += 1;
            }
        }

        assert!(
            false_positives <= 8,
            "{false_positives} of 200 unrelated passwords were called breached; the filter is \
             either mis-sized or the corpus is too broad"
        );
    }

    #[tokio::test]
    async fn an_application_can_add_its_own_breach_entries() {
        BreachCheck::with_extra_list(["the-companys-old-shared-password"]);
        assert!(
            BreachCheck::embedded()
                .is_breached("The-Companys-Old-Shared-Password")
                .await
                .unwrap(),
            "the extra list is matched case-insensitively"
        );
    }

    #[tokio::test]
    async fn a_breach_endpoint_without_a_fetcher_is_a_configuration_error() {
        let check = BreachCheck::embedded().api("https://example.invalid/range");
        let error = check
            .is_breached("wharf-lentil-oxide-77")
            .await
            .expect_err("a silent skip would be worse than no check at all");
        assert!(matches!(error, Error::Config(_)));
    }

    #[tokio::test]
    async fn a_failing_fetcher_lets_the_signup_through() {
        /// A fetcher that is always down.
        struct Down;

        impl RangeFetcher for Down {
            fn fetch<'a>(
                &'a self,
                _url: &'a str,
            ) -> BoxFuture<'a, core::result::Result<String, crate::BoxError>> {
                Box::pin(async { Err("connection refused".into()) })
            }
        }

        let check = BreachCheck::embedded()
            .api("https://example.invalid/range")
            .fetcher(std::sync::Arc::new(Down));

        assert!(
            !check.is_breached("wharf-lentil-oxide-77").await.unwrap(),
            "a breach service being down must not stop people signing up"
        );
    }

    #[tokio::test]
    async fn a_fetcher_that_reports_the_suffix_marks_the_password_breached() {
        /// A fetcher that always returns the range containing every suffix it
        /// is asked about, so the parsing is what is under test.
        struct Canned(String);

        impl RangeFetcher for Canned {
            fn fetch<'a>(
                &'a self,
                _url: &'a str,
            ) -> BoxFuture<'a, core::result::Result<String, crate::BoxError>> {
                let body = self.0.clone();
                Box::pin(async move { Ok(body) })
            }
        }

        let password = "wharf-lentil-oxide-77";
        let digest = sha1_hex(password.as_bytes());
        let (_, suffix) = digest.split_at(5);
        let body = format!("0000000000000000000000000000000000AA:3\r\n{suffix}:12345\r\n");

        let check = BreachCheck::embedded()
            .api("https://example.invalid/range")
            .fetcher(std::sync::Arc::new(Canned(body)));

        assert!(check.is_breached(password).await.unwrap());
    }

    /// RFC 3174's own vectors. A wrong SHA-1 would silently make every
    /// k-anonymity lookup a miss, which looks exactly like "not breached".
    #[test]
    fn the_sha1_shim_matches_the_rfc_vectors() {
        assert_eq!(sha1_hex(b"abc"), "A9993E364706816ABA3E25717850C26C9CD0D89D");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983E441C3BD26EBAAE4AA1F95129E5E54670F1"
        );
        assert_eq!(sha1_hex(b""), "DA39A3EE5E6B4B0D3255BFEF95601890AFD80709");
        assert_eq!(
            sha1_hex(&b"a".repeat(1_000_000)),
            "34AA973CD4C4DAA4F61EEB2BDBAD27316534016F"
        );
    }

    #[tokio::test]
    async fn the_policy_rejects_short_breached_and_weak_passwords_with_stable_codes() {
        let policy = PasswordPolicy::default();

        let code = |error: Error| match error {
            Error::PasswordPolicy { code, .. } => code,
            other => panic!("expected a policy failure, got {other:?}"),
        };

        let short = Password::with_min_length("short", 1).unwrap();
        assert_eq!(code(policy.check(&short, &[]).await.unwrap_err()), "len");

        let breached = Password::new("password1234").unwrap();
        assert_eq!(
            code(policy.check(&breached, &[]).await.unwrap_err()),
            "breached"
        );

        let weak = Password::new("aaaaaaaaaaaaaa").unwrap();
        assert_eq!(code(policy.check(&weak, &[]).await.unwrap_err()), "weak");
    }

    #[tokio::test]
    async fn the_policy_rejects_the_users_own_details() {
        let policy = PasswordPolicy::default();

        let error = policy
            .check(
                &Password::new("adaexample1234").unwrap(),
                &["ada@example.com"],
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            Error::PasswordPolicy {
                code: "weak" | "breached",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn the_policy_rejects_an_application_banned_word() {
        let policy = PasswordPolicy {
            banned_words: vec!["Moso".to_owned()],
            ..PasswordPolicy::default()
        };

        let error = policy
            .check(&Password::new("mosoisgreat123").unwrap(), &[])
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            Error::PasswordPolicy { code: "banned", .. }
        ));
    }

    #[tokio::test]
    async fn the_policy_accepts_a_passphrase_with_no_composition_rules() {
        let policy = PasswordPolicy::default();
        policy
            .check(&Password::new("wharf lentil oxide harbour").unwrap(), &[])
            .await
            .expect("length and unpredictability, not character classes");
    }

    #[test]
    fn the_estimator_scores_the_patterns_it_claims_to() {
        assert_eq!(Strength::estimate("password", &[]).score(), 0);
        assert_eq!(Strength::estimate("qwertyuiop", &[]).score(), 0);
        assert_eq!(
            Strength::estimate("aaaaaaaaaaaa", &[]).score(),
            0,
            "twelve of the same character is one character and a length"
        );
        assert_eq!(
            Strength::estimate("abcdefghijkl", &[]).score(),
            0,
            "a run is one character and a length too"
        );
        assert!(Strength::estimate("wharf-lentil-oxide-77", &[]).score() >= 3);
        assert!(Strength::estimate("correct horse battery staple", &[]).score() >= 3);
    }

    #[test]
    fn the_estimator_explains_itself() {
        let weak = Strength::estimate("aaaaaaaaaaaa", &[]);
        assert!(weak.feedback().is_some());
        assert!(weak.suggestion().is_some());

        let strong = Strength::estimate("correct horse battery staple", &[]);
        assert!(strong.suggestion().is_none());
    }

    #[test]
    fn leet_substitutions_do_not_hide_a_common_word() {
        assert_eq!(Strength::estimate("p@ssw0rd", &[]).score(), 0);
        assert_eq!(Strength::estimate("l3tm31n", &[]).score(), 0);
    }

    #[test]
    fn the_corpus_expansion_is_the_size_the_documentation_claims() {
        let mut count = 0_usize;
        expand_corpus(|_| count += 1);
        assert!(
            (80_000..400_000).contains(&count),
            "the expanded corpus is {count} entries; the filter is sized for \
             {} and the documentation says roughly a hundred thousand",
            COMMON_SEEDS.len() * ENTRIES_PER_SEED
        );
    }

    #[test]
    fn the_filter_has_no_false_negatives() {
        // The defining property of a Bloom filter, asserted rather than assumed.
        let mut inserted = Vec::new();
        expand_corpus(|entry| {
            if inserted.len() < 500 {
                inserted.push(entry.to_owned());
            }
        });

        for entry in &inserted {
            assert!(
                breach_filter().contains(entry),
                "`{entry}` was inserted and the filter denies it"
            );
        }
    }

    /// Acceptance criterion 4: hashing must not stall the runtime. A hundred
    /// concurrent hashes run while an unrelated task keeps ticking; the tick
    /// latency is what a real endpoint would see.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_hash_flood_leaves_the_runtime_responsive() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = Arc::new(AtomicBool::new(false));

        // The "unrelated endpoint": a task that wakes every millisecond and
        // records how late it was.
        let ticker_stop = Arc::clone(&stop);
        let ticker = tokio::spawn(async move {
            let mut lateness = Vec::new();
            while !ticker_stop.load(Ordering::Relaxed) {
                let due = Instant::now() + Duration::from_millis(1);
                tokio::time::sleep_until(due.into()).await;
                lateness.push(Instant::now().saturating_duration_since(due));
            }
            lateness
        });

        // A hundred concurrent logins' worth of hashing.
        let plain = Password::new("a sufficiently long one").unwrap();
        let mut hashes = Vec::new();
        for _ in 0..100 {
            let plain = plain.clone();
            hashes.push(tokio::spawn(async move {
                PasswordHash::with_params(&plain, HashParams::new(1024, 1, 1))
                    .await
                    .unwrap()
            }));
        }
        for handle in hashes {
            handle.await.unwrap();
        }

        stop.store(true, Ordering::Relaxed);
        let mut lateness = ticker.await.unwrap();
        assert!(!lateness.is_empty(), "the ticker never ran");

        lateness.sort_unstable();
        let p99 = lateness[(lateness.len() * 99 / 100).min(lateness.len() - 1)];

        assert!(
            p99 < Duration::from_millis(50),
            "an unrelated task's p99 latency was {p99:?} during a hash flood; the bounded pool \
             is supposed to keep this under 50 ms"
        );
    }
}
