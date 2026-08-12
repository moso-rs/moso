//! One configuration object for the whole battery, and the readiness probe.

use std::time::Duration;

use moso_core::config::{Profile, SecretBytes};
use moso_core::health::HealthStatus;
use moso_schema::{Password, Url};

use crate::{
    CookieConfig, CsrfConfig, Error, HashParams, JwtConfig, PasswordPolicy, Result, SessionConfig,
    SessionStore, ThrottleConfig,
};

// ---------------------------------------------------------------------------
// The configuration object
// ---------------------------------------------------------------------------

/// Everything the auth battery reads from configuration.
///
/// ```
/// use moso_auth::AuthConfig;
/// use moso_core::config::SecretBytes;
///
/// let mut config = AuthConfig::default();
/// config.secret_keys = vec![SecretBytes::new(vec![7; 32])];
/// config.validate()?;
/// # Ok::<(), moso_auth::Error>(())
/// ```
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct AuthConfig {
    /// How sessions behave.
    pub session: SessionConfig,
    /// How CSRF protection behaves.
    pub csrf: CsrfConfig,
    /// What a password must satisfy.
    pub password: PasswordPolicy,
    /// The argon2id parameters, from `moso auth calibrate`.
    ///
    /// `None` means "use [`HashParams::OWASP_MINIMUM`]", and the boot log says
    /// so — running uncalibrated is a defensible choice and a silent one is not.
    pub hash_params: Option<HashParams>,
    /// How tokens are issued and verified.
    pub jwt: JwtConfig,
    /// How aggressive the login throttle is.
    pub throttle: ThrottleConfig,
    /// The keys the session cookie is signed with. The first signs, the rest
    /// only verify, so rotation does not log anybody out.
    pub secret_keys: Vec<SecretBytes>,
    /// Where a `next` parameter may point after login.
    pub redirect_allowlist: Vec<String>,
    /// Whether `Secure` may be off on the session cookie in this profile.
    ///
    /// False everywhere but development. Forcing it true in production is
    /// possible, requires this flag, and logs a warning at boot — because a
    /// session cookie without `Secure` is a session cookie on the wire.
    pub allow_insecure_cookies: bool,
    /// Whether the verification email is required before a session is issued.
    pub require_verified_email: bool,
}

/// The shortest session-cookie signing key that is not a downgrade.
///
/// Thirty-two bytes: the output width of HMAC-SHA256, which is what signs the
/// cookie. A shorter key does not make the signature shorter, only guessable,
/// and the same floor is enforced again by
/// [`SessionLayer::validate`](crate::SessionLayer::validate) for a layer whose
/// keys were set directly rather than through this type.
const MIN_SIGNING_KEY_BYTES: usize = 32;

/// The top of the password-strength scale [`PasswordPolicy::min_strength`] is
/// read against, mirroring [`Strength::score`](crate::Strength::score)'s 0–4.
const MAX_STRENGTH_SCORE: u8 = 4;

impl AuthConfig {
    /// Check for contradictions before the first request.
    ///
    /// **Every** problem is reported, not the first one: a boot report that
    /// stops at the earliest mistake turns one broken deployment into as many
    /// restart cycles as there are typos. Each entry names the field it is
    /// about and the edit that fixes it.
    ///
    /// What is checked, and where the rule lives:
    ///
    /// | Problem | Home of the rule |
    /// | --- | --- |
    /// | No signing key, or one under 32 bytes | here, next to the field |
    /// | Idle timeout past the absolute one | [`SessionConfig::validate`] |
    /// | `SameSite=None` without `Secure` | [`SessionConfig::validate`] |
    /// | `__Host-` prefix with a `Domain` or a sub-path | [`SessionConfig::validate`] |
    /// | A wildcard or non-origin redirect entry | here |
    /// | A symmetric JWT algorithm without the opt-in | here |
    /// | A password policy or hash floor nothing can satisfy | here |
    ///
    /// The three session rules are *folded in*, not restated: this method calls
    /// [`SessionConfig::validate`] and adds its message to the list, so the
    /// wording of a session rule is written once.
    ///
    /// **`allow_insecure_cookies` is deliberately not checked here.** Whether
    /// it is acceptable depends entirely on the profile, and this method has no
    /// profile to read — `AuthConfig` does not carry one, and detecting one
    /// from the environment inside a validator would make the same
    /// configuration valid or invalid depending on an environment variable
    /// nobody passed in. The enforcement point is [`AuthConfig::cookie_for`],
    /// which does take a [`Profile`] and refuses to drop `Secure` outside
    /// development.
    ///
    /// A missing [`hash_params`](AuthConfig::hash_params) is not a problem, it
    /// is a `WARN` on the boot log: uncalibrated hashing is a defensible
    /// choice, and a silent one is not.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] listing every problem found, each naming its field
    /// and its fix.
    ///
    /// ```
    /// use moso_auth::AuthConfig;
    /// use moso_core::config::SecretBytes;
    ///
    /// let mut config = AuthConfig::default();
    ///
    /// // A default configuration has no signing key, and an unsigned session
    /// // cookie is one anybody can mint.
    /// assert!(config.validate().is_err());
    ///
    /// config.secret_keys = vec![SecretBytes::new(vec![7; 32])];
    /// config.validate()?;
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn validate(&self) -> Result<()> {
        let mut problems: Vec<String> = Vec::new();

        self.check_signing_keys(&mut problems);
        self.check_session(&mut problems);
        self.check_redirect_allowlist(&mut problems);
        self.check_jwt(&mut problems);
        self.check_passwords(&mut problems);

        if let [only] = problems.as_slice() {
            return Err(Error::Config(only.clone().into()));
        }
        if problems.is_empty() {
            return Ok(());
        }

        let count = problems.len();
        let listed: String = problems
            .iter()
            .map(|problem| format!("\n  - {problem}"))
            .collect();
        Err(Error::Config(
            format!("{count} problems, and every one of them has to be fixed:{listed}").into(),
        ))
    }

    /// Report a missing or undersized session-cookie signing key.
    ///
    /// An empty list short-circuits: telling somebody who set no key at all
    /// that "key 0 is too short" would be noise, because there is no key 0.
    fn check_signing_keys(&self, problems: &mut Vec<String>) {
        if self.secret_keys.is_empty() {
            problems.push(
                "auth.secret_keys is empty, so the session cookie would carry no signature and \
                 anybody could mint one; help: set auth.secret_keys to at least one 32-byte key \
                 (`openssl rand -base64 32`), keeping the previous key in the list so a rotation \
                 does not log everybody out"
                    .to_owned(),
            );
            return;
        }

        for (index, key) in self.secret_keys.iter().enumerate() {
            if key.len() < MIN_SIGNING_KEY_BYTES {
                problems.push(format!(
                    "auth.secret_keys[{index}] is {} bytes and a session-cookie signing key needs \
                     at least {MIN_SIGNING_KEY_BYTES}; help: generate one with \
                     `openssl rand -base64 32`",
                    key.len()
                ));
            }
        }
    }

    /// Fold [`SessionConfig::validate`]'s verdict into the report.
    ///
    /// The session rules are not restated here — the message comes back from
    /// the type that owns them, so there is exactly one wording to maintain.
    fn check_session(&self, problems: &mut Vec<String>) {
        match self.session.validate() {
            Ok(()) => {}
            Err(Error::Config(detail)) => problems.push(detail.into_owned()),
            Err(other) => problems.push(other.to_string()),
        }
    }

    /// Report every redirect-allowlist entry that is not a bare origin.
    fn check_redirect_allowlist(&self, problems: &mut Vec<String>) {
        for (index, entry) in self.redirect_allowlist.iter().enumerate() {
            if let Some(problem) = origin_problem(entry) {
                problems.push(format!(
                    "auth.redirect_allowlist[{index}] ({entry:?}) {problem}"
                ));
            }
        }
    }

    /// Report a symmetric signing algorithm that was never opted into.
    ///
    /// HS256 signs and verifies with the same key, so every service that can
    /// check a token can also forge one — and the key can never be published,
    /// which is why [`Jwt::jwks`](crate::Jwt::jwks) drops HMAC keys from the
    /// document rather than exposing the signing key at
    /// `/.well-known/jwks.json`. [`Jwt::issuer`](crate::Jwt::issuer) refuses
    /// the same combination at construction; catching it here moves the
    /// failure from the first token to boot.
    fn check_jwt(&self, problems: &mut Vec<String>) {
        if self.jwt.algorithm.is_symmetric() && !self.jwt.allow_symmetric {
            problems.push(format!(
                "auth.jwt.algorithm is {}, which is symmetric: every holder of the verification \
                 key can also mint tokens, and the key cannot appear in a JWKS document; help: \
                 use the default EdDSA, or set auth.jwt.allow_symmetric = true if the token \
                 never leaves this process",
                self.jwt.algorithm.as_str()
            ));
        }
    }

    /// Report a password policy or hash floor that nothing could satisfy, and
    /// warn when hashing is running uncalibrated.
    fn check_passwords(&self, problems: &mut Vec<String>) {
        if self.password.min_strength > MAX_STRENGTH_SCORE {
            problems.push(format!(
                "auth.password.min_strength is {}, and the scale stops at {MAX_STRENGTH_SCORE}, \
                 so no password could ever be accepted; help: use 0-4 (the default is 2)",
                self.password.min_strength
            ));
        }

        if self.password.min_length < Password::MIN_LENGTH {
            problems.push(format!(
                "auth.password.min_length is {}, below the {} `Password` itself already enforces, \
                 so the lower value can never take effect and the configuration is a lie; help: \
                 raise it to at least {}",
                self.password.min_length,
                Password::MIN_LENGTH,
                Password::MIN_LENGTH
            ));
        } else if self.password.min_length > Password::MAX_LENGTH {
            problems.push(format!(
                "auth.password.min_length is {}, above the {} `Password` accepts, so every \
                 password would be refused; help: lower it to at most {}",
                self.password.min_length,
                Password::MAX_LENGTH,
                Password::MAX_LENGTH
            ));
        }

        match self.hash_params {
            Some(params) if !params.at_least(HashParams::OWASP_MINIMUM) => problems.push(format!(
                "auth.hash_params ({} KiB, t={}, p={}) is below OWASP's minimum ({} KiB, t={}, \
                 p={}) in at least one dimension, and being slow hardware is not a reason to be \
                 weak; help: raise it, or drop the setting to run on the floor",
                params.memory_kib,
                params.iterations,
                params.parallelism,
                HashParams::OWASP_MINIMUM.memory_kib,
                HashParams::OWASP_MINIMUM.iterations,
                HashParams::OWASP_MINIMUM.parallelism,
            )),
            Some(_) => {}
            None => tracing::warn!(
                target: "moso_auth::config",
                memory_kib = HashParams::OWASP_MINIMUM.memory_kib,
                iterations = HashParams::OWASP_MINIMUM.iterations,
                parallelism = HashParams::OWASP_MINIMUM.parallelism,
                "auth.hash_params is unset, so password hashing runs uncalibrated on \
                 HashParams::OWASP_MINIMUM; call `moso_auth::calibrate(TARGET_HASH_TIME)` on the \
                 deployment hardware and write the result to auth.hash_params"
            ),
        }
    }

    /// Read a configuration from the process environment, then validate it.
    ///
    /// The twelve-factor loader, for a binary that has no configuration layer of
    /// its own. It mirrors [`KvConfig::from_env`](moso_kv::KvConfig::from_env):
    /// `moso-auth` sits below `moso-macros`, so it cannot carry a
    /// `#[derive(Config)]` — the derive resolves against the `moso` facade above
    /// it — and an application that *does* have a configuration layer should
    /// build an [`AuthConfig`] from its own `#[derive(Config)]` struct instead,
    /// so `moso config` can see the settings, and then call
    /// [`validate`](AuthConfig::validate).
    ///
    /// Unlike the key-value loader, this one **validates before it returns**:
    /// the ledger's complaint was that nothing ran [`validate`](Self::validate)
    /// at boot, and a loader that hands back a configuration a first request
    /// would reject is the same bug wearing a hat. The whole boot report comes
    /// back at once, every field named with its fix.
    ///
    /// | Variable | Meaning | Default |
    /// | --- | --- | --- |
    /// | `AUTH_SECRET_KEYS` | Comma-separated standard-base64 signing keys; the first signs, the rest only verify | none — and an empty set is a boot error |
    /// | `AUTH_REDIRECT_ALLOWLIST` | Comma-separated `http(s)://host[:port]` origins a `next` may point at | empty |
    /// | `AUTH_ALLOW_INSECURE_COOKIES` | Drop `Secure` on the session cookie — development only | `false` |
    /// | `AUTH_REQUIRE_VERIFIED_EMAIL` | Refuse a session until the address is verified | `false` |
    ///
    /// A signing key is read straight into a [`SecretBytes`], which redacts in
    /// `Debug` and zeroes on drop, so the value never reaches a log.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when a variable is set to something unparseable — a key
    /// that is not base64, a boolean that is not a boolean — or when the loaded
    /// configuration does not [`validate`](Self::validate).
    ///
    /// ```no_run
    /// // `no_run`: this reads the process environment, which the doc-test
    /// // harness does not set up. See the unit tests for the deterministic path.
    /// let config = moso_auth::AuthConfig::from_env()?;
    /// assert!(!config.secret_keys.is_empty());
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn from_env() -> Result<Self> {
        Self::load(|name| std::env::var(name).ok())
    }

    /// The pure core of [`from_env`](Self::from_env), over an arbitrary lookup.
    ///
    /// Taking the environment as a closure rather than reading it directly is
    /// what lets the loader be tested without mutating process-wide state —
    /// which on edition 2024 is an `unsafe` operation and a race between
    /// parallel tests besides.
    fn load(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut config = Self::default();

        if let Some(raw) = present(&lookup, "AUTH_SECRET_KEYS") {
            config.secret_keys = parse_secret_keys(&raw)?;
        }
        if let Some(raw) = present(&lookup, "AUTH_REDIRECT_ALLOWLIST") {
            config.redirect_allowlist = raw
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_owned)
                .collect();
        }
        if let Some(raw) = present(&lookup, "AUTH_ALLOW_INSECURE_COOKIES") {
            config.allow_insecure_cookies = parse_bool("AUTH_ALLOW_INSECURE_COOKIES", &raw)?;
        }
        if let Some(raw) = present(&lookup, "AUTH_REQUIRE_VERIFIED_EMAIL") {
            config.require_verified_email = parse_bool("AUTH_REQUIRE_VERIFIED_EMAIL", &raw)?;
        }

        config.validate()?;
        Ok(config)
    }

    /// The parameters password hashing will actually use.
    ///
    /// ```
    /// use moso_auth::{AuthConfig, HashParams};
    ///
    /// let config = AuthConfig::default();
    /// assert_eq!(config.effective_hash_params(), HashParams::OWASP_MINIMUM);
    /// ```
    #[must_use]
    pub fn effective_hash_params(&self) -> HashParams {
        self.hash_params.unwrap_or(HashParams::OWASP_MINIMUM)
    }

    /// The cookie configuration, adjusted for the profile.
    ///
    /// In development, `Secure` comes off when
    /// [`allow_insecure_cookies`](AuthConfig::allow_insecure_cookies) asks for
    /// it — otherwise nothing works on `http://localhost` and every tutorial
    /// starts with a workaround. **In every other profile `Secure` stays on
    /// whatever the flag says**, and setting it there logs a warning naming
    /// what it would have cost: a session cookie without `Secure` is a session
    /// cookie on the wire, and the profile is exactly the thing that decides
    /// whether "the wire" is a loopback socket or the internet.
    ///
    /// Dropping `Secure` also drops the `__Host-` prefix from
    /// [`CookieConfig::full_name`], because the prefix is only honoured on a
    /// secure cookie — no extra bookkeeping is needed for that, it falls out of
    /// [`CookieConfig::host_prefix_applies`].
    ///
    /// ```
    /// use moso_auth::AuthConfig;
    /// use moso_core::config::Profile;
    ///
    /// let mut config = AuthConfig::default();
    /// config.allow_insecure_cookies = true;
    ///
    /// assert!(!config.cookie_for(Profile::Dev).secure);
    /// assert!(config.cookie_for(Profile::Production).secure);
    /// assert!(config.cookie_for(Profile::Test).secure);
    /// ```
    #[must_use]
    pub fn cookie_for(&self, profile: Profile) -> CookieConfig {
        let mut cookie = self.session.cookie.clone();
        if !self.allow_insecure_cookies {
            return cookie;
        }

        match profile {
            Profile::Dev => {
                tracing::warn!(
                    target: "moso_auth::config",
                    profile = profile.as_str(),
                    "auth.allow_insecure_cookies is on: the session cookie is issued without \
                     `Secure`, so any plain-HTTP request to this host puts a live session on the \
                     wire, and the `__Host-` prefix no longer applies. Development only"
                );
                cookie.secure = false;
                cookie
            }
            Profile::Test | Profile::Production => {
                tracing::warn!(
                    target: "moso_auth::config",
                    profile = profile.as_str(),
                    "auth.allow_insecure_cookies is set but the profile is not `dev`, so `Secure` \
                     stays on the session cookie; help: remove the setting from this profile's \
                     configuration so it does not read as protection that is switched off"
                );
                cookie
            }
        }
    }
}

/// Why `entry` is not usable as a redirect-allowlist origin, if it is not.
///
/// The list is compared origin by origin — scheme, host and port, all three or
/// none — so an entry has to *be* an origin. Three shapes are refused:
///
/// * anything containing `*`. This list is not a pattern language, and reading
///   `*.example.com` as one is how `evil-example.com` gets accepted;
/// * anything that is not an absolute `http(s)://host[:port]` URL. A relative
///   value silently matches nothing, which reads as "the allowlist is broken"
///   long after the deploy;
/// * anything carrying a path, query, fragment or userinfo. `https://a@b` and
///   `https://b/../a` are the two spellings that make an origin comparison look
///   like it succeeded against a host nobody meant to allow.
fn origin_problem(entry: &str) -> Option<String> {
    if entry.contains('*') {
        return Some(
            "contains a wildcard, and this list is compared origin by origin rather than as a \
             pattern; help: list each origin in full, e.g. \"https://app.example.com\""
                .to_owned(),
        );
    }

    let parsed = match Url::parse_http(entry) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Some(format!(
                "is not an absolute `http(s)://host[:port]` origin ({error}); help: write it out \
                 in full, e.g. \"https://app.example.com\""
            ));
        }
    };

    let url = parsed.as_url();
    if !url.username().is_empty() || url.password().is_some() {
        return Some(
            "carries credentials in its authority, which is not part of an origin and would \
             compare against a host nobody intended; help: drop everything before the `@`"
                .to_owned(),
        );
    }
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        return Some(
            "has a path, query or fragment, and only the origin is compared, so the extra part \
             would be silently ignored; help: keep the scheme, host and port only"
                .to_owned(),
        );
    }

    None
}

/// The value of `name` from `lookup`, unless it is absent or blank.
///
/// An environment variable set to the empty string is treated as unset: a
/// deployment that writes `AUTH_SECRET_KEYS=` meant "I have not set this", not
/// "the empty list", and the second reading turns a typo into a silent
/// downgrade.
fn present(lookup: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    lookup(name).filter(|value| !value.trim().is_empty())
}

/// Parse a comma-separated list of standard-base64 signing keys.
///
/// Standard base64 because that is what `openssl rand -base64 32` emits — the
/// very command [`AuthConfig::validate`] tells an operator to run. The length
/// floor is left to `validate`, so a short key is reported next to every other
/// boot problem rather than aborting this parse in isolation.
fn parse_secret_keys(raw: &str) -> Result<Vec<SecretBytes>> {
    use base64::Engine as _;

    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            base64::engine::general_purpose::STANDARD
                .decode(entry)
                .map(SecretBytes::new)
                .map_err(|error| {
                    Error::Config(
                        format!(
                            "AUTH_SECRET_KEYS has an entry that is not standard base64 ({error}); \
                             help: generate each key with `openssl rand -base64 32`"
                        )
                        .into(),
                    )
                })
        })
        .collect()
}

/// Parse a boolean the way a person writes one in an environment file.
fn parse_bool(name: &str, raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(Error::Config(
            format!(
                "{name} is set to `{other}`, which is not a boolean; help: use `true` or `false`"
            )
            .into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Readiness
// ---------------------------------------------------------------------------

/// How long the session store has to answer a readiness probe.
///
/// One second, which is half of
/// [`READINESS_BUDGET`](moso_core::health::READINESS_BUDGET). The outer budget
/// already stops a hung check, but it stops *every* check at once and reports
/// them all as "timed out"; bounding this probe at half of it means the store
/// is named as the slow component while the rest of the report still arrives.
/// A session store that needs longer than a second to answer "are you there"
/// is a store that will not survive a login flood either, so a slow answer and
/// no answer are deliberately the same verdict.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// The `/readyz` probe for the session store.
///
/// Critical: a process that cannot read sessions cannot authenticate anybody,
/// and staying in rotation means serving 401s to every signed-in user.
///
/// ```
/// use std::sync::Arc;
///
/// use moso_auth::{AuthHealthCheck, MemorySessionStore, SessionStore};
///
/// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
/// let check = AuthHealthCheck::new(store);
/// ```
#[derive(Clone)]
pub struct AuthHealthCheck {
    /// What to probe.
    store: std::sync::Arc<dyn SessionStore>,
    /// Whether a failure makes the instance unready.
    critical: bool,
}

impl AuthHealthCheck {
    /// A critical probe of `store`.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_auth::{AuthHealthCheck, MemorySessionStore, SessionStore};
    ///
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// let check = AuthHealthCheck::new(store);
    /// ```
    #[must_use]
    pub fn new(store: std::sync::Arc<dyn SessionStore>) -> Self {
        Self {
            store,
            critical: true,
        }
    }

    /// Whether a failure takes the instance out of rotation.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_auth::{AuthHealthCheck, MemorySessionStore, SessionStore};
    ///
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// let check = AuthHealthCheck::new(store).critical(false);
    /// ```
    #[must_use]
    pub fn critical(mut self, critical: bool) -> Self {
        self.critical = critical;
        self
    }
}

impl core::fmt::Debug for AuthHealthCheck {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthHealthCheck")
            .field("critical", &self.critical)
            .finish()
    }
}

impl moso_core::HealthCheck for AuthHealthCheck {
    /// Reach the session store and come back inside [`PROBE_TIMEOUT`].
    ///
    /// [`SessionStore::probe`] is the round trip: it is the one method on the
    /// trait that proves reachability without touching a record — a `load` of
    /// an invented identifier would also work, but it would put a miss through
    /// whatever cache the backend keeps, and nothing that runs on every
    /// readiness poll should be able to disturb a real session.
    ///
    /// The resolver is unused: the store is held by value, so the probe does
    /// not depend on the provider map being complete.
    fn check<'a>(
        &'a self,
        resolver: &'a moso_core::Resolver,
    ) -> moso_core::BoxFuture<'a, HealthStatus> {
        let _ = resolver;
        Box::pin(async move {
            match tokio::time::timeout(PROBE_TIMEOUT, self.store.probe()).await {
                Ok(Ok(())) => HealthStatus::Up,
                Ok(Err(error)) => HealthStatus::Down(format!("session store: {error}")),
                Err(_) => HealthStatus::Down(format!(
                    "session store did not answer within {PROBE_TIMEOUT:?}"
                )),
            }
        })
    }

    fn critical(&self) -> bool {
        self.critical
    }
}

// ---------------------------------------------------------------------------
// Token lifetimes
// ---------------------------------------------------------------------------

/// How long a verification or reset token lives.
///
/// One hour: long enough that a slow mail provider does not break the flow,
/// short enough that a token sitting in an abandoned inbox is not a standing
/// credential.
///
/// ```
/// use std::time::Duration;
///
/// assert_eq!(moso_auth::config::TOKEN_TTL, Duration::from_secs(3600));
/// ```
pub const TOKEN_TTL: Duration = Duration::from_secs(3600);

/// How long a magic-link token lives.
///
/// Shorter than [`TOKEN_TTL`], because a magic link *is* a login and a link
/// that logs somebody in should not be forwardable an hour later.
///
/// ```
/// use std::time::Duration;
///
/// assert_eq!(moso_auth::config::MAGIC_LINK_TTL, Duration::from_secs(900));
/// ```
pub const MAGIC_LINK_TTL: Duration = Duration::from_secs(900);

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moso_core::{HealthCheck, ProviderMap, Resolver};

    use super::*;
    use crate::{JwtAlgorithm, SameSite, SessionId, SessionRecord};

    /// A configuration that validates, so a test can break exactly one thing.
    fn workable() -> AuthConfig {
        AuthConfig {
            secret_keys: vec![SecretBytes::new(vec![7; MIN_SIGNING_KEY_BYTES])],
            ..AuthConfig::default()
        }
    }

    /// The message of a `Config` failure, or a panic naming what came instead.
    fn config_failure(config: &AuthConfig) -> String {
        match config.validate() {
            Ok(()) => panic!("expected the configuration to be refused"),
            Err(Error::Config(detail)) => detail.into_owned(),
            Err(other) => panic!("expected Error::Config, got {other:?}"),
        }
    }

    /// A store that stores nothing and answers a probe the way it was told to.
    ///
    /// Everything but `probe` is a working no-op store: a session saved into
    /// nowhere is not found again, which is exactly what these tests need and
    /// is a real answer rather than a stub.
    struct ProbeStore {
        /// Whether the probe succeeds.
        reachable: bool,
        /// How long the probe takes to say so.
        delay: Duration,
    }

    impl ProbeStore {
        /// A store that answers immediately.
        fn answering(reachable: bool) -> Arc<dyn SessionStore> {
            Arc::new(Self {
                reachable,
                delay: Duration::ZERO,
            })
        }
    }

    impl SessionStore for ProbeStore {
        fn load<'a>(
            &'a self,
            id: &'a SessionId,
        ) -> moso_core::BoxFuture<'a, Result<Option<SessionRecord>>> {
            let _ = id;
            Box::pin(async { Ok(None) })
        }

        fn save<'a>(
            &'a self,
            record: &'a SessionRecord,
            ttl: Duration,
        ) -> moso_core::BoxFuture<'a, Result<()>> {
            let _ = (record, ttl);
            Box::pin(async { Ok(()) })
        }

        fn delete<'a>(&'a self, id: &'a SessionId) -> moso_core::BoxFuture<'a, Result<bool>> {
            let _ = id;
            Box::pin(async { Ok(false) })
        }

        fn rename<'a>(
            &'a self,
            from: &'a SessionId,
            to: &'a SessionId,
        ) -> moso_core::BoxFuture<'a, Result<()>> {
            let _ = (from, to);
            Box::pin(async { Ok(()) })
        }

        fn list_for_user<'a>(
            &'a self,
            user_id: &'a str,
        ) -> moso_core::BoxFuture<'a, Result<Vec<SessionRecord>>> {
            let _ = user_id;
            Box::pin(async { Ok(Vec::new()) })
        }

        fn delete_for_user<'a>(
            &'a self,
            user_id: &'a str,
            except: Option<&'a SessionId>,
        ) -> moso_core::BoxFuture<'a, Result<u64>> {
            let _ = (user_id, except);
            Box::pin(async { Ok(0) })
        }

        fn probe(&self) -> moso_core::BoxFuture<'_, Result<()>> {
            Box::pin(async move {
                tokio::time::sleep(self.delay).await;
                if self.reachable {
                    return Ok(());
                }
                Err(Error::Unavailable {
                    component: "session store",
                    detail: "connection refused".to_owned(),
                    source: None,
                })
            })
        }
    }

    /// An empty provider map, because the probe reads nothing from one.
    fn resolver() -> Resolver {
        Resolver::new(Arc::new(ProviderMap::default()))
    }

    // ── the boot report ───────────────────────────────────────────────────

    #[test]
    fn a_configuration_with_one_long_enough_key_is_accepted() {
        workable().validate().unwrap();
    }

    #[test]
    fn an_empty_key_list_is_a_boot_error_naming_the_field() {
        let message = config_failure(&AuthConfig::default());

        assert!(message.contains("auth.secret_keys"), "{message}");
        assert!(message.contains("help:"), "{message}");
    }

    #[test]
    fn a_signing_key_shorter_than_the_hmac_output_is_refused() {
        let mut config = workable();
        config.secret_keys = vec![SecretBytes::new(vec![1; 16])];

        let message = config_failure(&config);

        assert!(message.contains("auth.secret_keys[0]"), "{message}");
        assert!(message.contains("16 bytes"), "{message}");
    }

    #[test]
    fn every_problem_is_reported_in_one_error_rather_than_the_first() {
        let config = AuthConfig {
            redirect_allowlist: vec!["https://*.example.com".to_owned()],
            ..AuthConfig::default()
        };

        let message = config_failure(&config);

        assert!(message.contains("2 problems"), "{message}");
        assert!(message.contains("auth.secret_keys"), "{message}");
        assert!(message.contains("auth.redirect_allowlist[0]"), "{message}");
    }

    #[test]
    fn a_session_rule_is_folded_in_rather_than_restated() {
        let mut config = workable();
        config.session.cookie.secure = false;
        config.session.cookie.same_site = SameSite::None;

        let message = config_failure(&config);
        let from_session = match config.session.validate() {
            Err(Error::Config(detail)) => detail.into_owned(),
            other => panic!("expected the session config to be refused, got {other:?}"),
        };

        assert_eq!(message, from_session);
    }

    // ── the redirect allowlist ────────────────────────────────────────────

    #[test]
    fn a_wildcard_in_the_redirect_allowlist_is_refused() {
        let mut config = workable();
        config.redirect_allowlist = vec!["https://*.example.com".to_owned()];

        let message = config_failure(&config);

        assert!(message.contains("wildcard"), "{message}");
        assert!(message.contains("auth.redirect_allowlist[0]"), "{message}");
    }

    #[test]
    fn a_bare_origin_with_a_port_is_accepted() {
        let mut config = workable();
        config.redirect_allowlist = vec![
            "https://app.example.com".to_owned(),
            "http://localhost:3000/".to_owned(),
        ];

        config.validate().unwrap();
    }

    #[test]
    fn an_allowlist_entry_with_a_path_or_a_relative_shape_is_refused() {
        let mut config = workable();
        config.redirect_allowlist = vec![
            "https://app.example.com/dashboard".to_owned(),
            "/after-login".to_owned(),
            "https://user:pass@app.example.com".to_owned(),
        ];

        let message = config_failure(&config);

        assert!(message.contains("3 problems"), "{message}");
        assert!(message.contains("path, query or fragment"), "{message}");
        assert!(message.contains("absolute"), "{message}");
        assert!(message.contains("credentials"), "{message}");
    }

    // ── tokens, passwords and hashing ─────────────────────────────────────

    #[test]
    fn a_symmetric_jwt_algorithm_without_the_opt_in_is_refused() {
        let mut config = workable();
        config.jwt.algorithm = JwtAlgorithm::HS256;

        let message = config_failure(&config);

        assert!(message.contains("auth.jwt.allow_symmetric"), "{message}");
        assert!(message.contains("HS256"), "{message}");

        config.jwt.allow_symmetric = true;
        config.validate().unwrap();
    }

    #[test]
    fn hash_parameters_below_the_owasp_floor_are_refused() {
        let mut config = workable();
        config.hash_params = Some(HashParams::new(1024, 1, 1));

        let message = config_failure(&config);

        assert!(message.contains("auth.hash_params"), "{message}");
        assert!(message.contains("OWASP"), "{message}");
    }

    #[test]
    fn a_password_policy_nothing_could_satisfy_is_refused() {
        let mut config = workable();
        config.password.min_strength = 9;
        config.password.min_length = 4;

        let message = config_failure(&config);

        assert!(message.contains("auth.password.min_strength"), "{message}");
        assert!(message.contains("auth.password.min_length"), "{message}");
    }

    // ── the cookie, per profile ───────────────────────────────────────────

    #[test]
    fn secure_survives_a_production_profile_even_with_the_flag_set() {
        let mut config = workable();
        config.allow_insecure_cookies = true;

        assert!(config.cookie_for(Profile::Production).secure);
        assert!(config.cookie_for(Profile::Test).secure);
    }

    #[test]
    fn secure_is_relaxed_in_development_only_when_the_flag_is_set() {
        let mut config = workable();

        assert!(config.cookie_for(Profile::Dev).secure);

        config.allow_insecure_cookies = true;
        let relaxed = config.cookie_for(Profile::Dev);

        assert!(!relaxed.secure);
        assert!(!relaxed.host_prefix_applies());
        assert_eq!(relaxed.full_name(), relaxed.name);
    }

    // ── loading from the environment ──────────────────────────────────────

    /// A base64 encoding of a 32-byte key, for the loader tests.
    fn a_key() -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode([7u8; MIN_SIGNING_KEY_BYTES])
    }

    /// Look a name up in a fixed table, standing in for the environment.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |name: &str| {
            owned
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn a_configured_key_loads_and_validates() {
        let key = a_key();
        let config = AuthConfig::load(env_of(&[("AUTH_SECRET_KEYS", &key)])).expect("valid");

        assert_eq!(config.secret_keys.len(), 1);
        assert_eq!(config.secret_keys[0].len(), MIN_SIGNING_KEY_BYTES);
    }

    #[test]
    fn an_environment_without_a_signing_key_is_a_boot_error() {
        let error = AuthConfig::load(env_of(&[])).expect_err("no key");
        assert!(matches!(error, Error::Config(_)), "{error:?}");
        assert!(error.to_string().contains("auth.secret_keys"), "{error}");
    }

    #[test]
    fn a_blank_value_reads_as_unset_rather_than_an_empty_list() {
        // `AUTH_SECRET_KEYS=` is a typo, not an instruction to sign with nothing.
        let error = AuthConfig::load(env_of(&[("AUTH_SECRET_KEYS", "   ")])).expect_err("blank");
        assert!(error.to_string().contains("auth.secret_keys"), "{error}");
    }

    #[test]
    fn a_key_that_is_not_base64_names_the_variable() {
        let error = AuthConfig::load(env_of(&[("AUTH_SECRET_KEYS", "not base64!!")]))
            .expect_err("bad base64");
        assert!(error.to_string().contains("AUTH_SECRET_KEYS"), "{error}");
        assert!(error.to_string().contains("base64"), "{error}");
    }

    #[test]
    fn several_keys_load_in_order_so_the_first_signs() {
        let first = a_key();
        use base64::Engine as _;
        let second = base64::engine::general_purpose::STANDARD.encode([9u8; MIN_SIGNING_KEY_BYTES]);
        let joined = format!("{first}, {second}");

        let config = AuthConfig::load(env_of(&[("AUTH_SECRET_KEYS", &joined)])).expect("valid");

        assert_eq!(config.secret_keys.len(), 2);
        assert_eq!(config.secret_keys[0].expose(), [7u8; MIN_SIGNING_KEY_BYTES]);
        assert_eq!(config.secret_keys[1].expose(), [9u8; MIN_SIGNING_KEY_BYTES]);
    }

    #[test]
    fn the_flag_fields_load_and_a_non_boolean_is_refused() {
        let key = a_key();
        let config = AuthConfig::load(env_of(&[
            ("AUTH_SECRET_KEYS", &key),
            ("AUTH_ALLOW_INSECURE_COOKIES", "true"),
            ("AUTH_REQUIRE_VERIFIED_EMAIL", "1"),
        ]))
        .expect("valid");
        assert!(config.allow_insecure_cookies);
        assert!(config.require_verified_email);

        let error = AuthConfig::load(env_of(&[
            ("AUTH_SECRET_KEYS", &key),
            ("AUTH_REQUIRE_VERIFIED_EMAIL", "maybe"),
        ]))
        .expect_err("not a boolean");
        assert!(
            error.to_string().contains("AUTH_REQUIRE_VERIFIED_EMAIL"),
            "{error}"
        );
    }

    #[test]
    fn the_redirect_allowlist_loads_and_is_validated() {
        let key = a_key();
        let config = AuthConfig::load(env_of(&[
            ("AUTH_SECRET_KEYS", &key),
            (
                "AUTH_REDIRECT_ALLOWLIST",
                "https://app.example.com, https://admin.example.com",
            ),
        ]))
        .expect("valid");
        assert_eq!(config.redirect_allowlist.len(), 2);

        // A bad origin is a boot error, from the same `validate` the struct uses.
        let error = AuthConfig::load(env_of(&[
            ("AUTH_SECRET_KEYS", &key),
            ("AUTH_REDIRECT_ALLOWLIST", "https://*.example.com"),
        ]))
        .expect_err("wildcard");
        assert!(
            error.to_string().contains("auth.redirect_allowlist"),
            "{error}"
        );
    }

    #[test]
    fn a_loaded_signing_key_does_not_print() {
        let key = a_key();
        let config = AuthConfig::load(env_of(&[("AUTH_SECRET_KEYS", &key)])).expect("valid");
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains(&key),
            "the base64 key leaked: {rendered}"
        );
        assert!(!rendered.contains("07070707"), "{rendered}");
    }

    // ── the readiness probe ───────────────────────────────────────────────

    #[tokio::test]
    async fn the_probe_is_up_when_the_store_answers() {
        let check = AuthHealthCheck::new(ProbeStore::answering(true));

        assert_eq!(check.check(&resolver()).await, HealthStatus::Up);
    }

    #[tokio::test]
    async fn the_probe_is_down_and_names_the_store_when_it_cannot_be_reached() {
        let check = AuthHealthCheck::new(ProbeStore::answering(false));

        let status = check.check(&resolver()).await;

        assert!(status.is_down(), "{status:?}");
        assert!(status.render().contains("session store"), "{status:?}");
        assert!(status.render().contains("connection refused"), "{status:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_store_is_down_rather_than_a_slow_readyz() {
        let check = AuthHealthCheck::new(Arc::new(ProbeStore {
            reachable: true,
            delay: PROBE_TIMEOUT * 10,
        }));

        let status = check.check(&resolver()).await;

        assert!(status.is_down(), "{status:?}");
        assert!(status.render().contains("did not answer"), "{status:?}");
    }
}
