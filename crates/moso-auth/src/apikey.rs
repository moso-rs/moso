//! API keys: greppable, scoped, revocable, and stored as a hash.
//!
//! The format is `mso_live_<prefix>_<secret>`, and every part of it earns its
//! place:
//!
//! - **`mso_`** makes a leaked key findable. GitHub's secret scanning matches on
//!   a registered prefix and notifies the owner; a key that looks like base64
//!   noise is a key nobody ever finds in a public repository.
//! - **`live` / `test`** makes it obvious in a log which environment a key came
//!   from, and lets a test key be refused by a production deployment.
//! - **`<prefix>`** is stored in the clear and indexed, so a lookup is one
//!   indexed query rather than a scan comparing hashes.
//! - **`<secret>`** is never stored. Only its SHA-256 is, and the full key is
//!   shown exactly once.
//!
//! ```
//! use moso_auth::{ApiKey, KeyEnvironment};
//!
//! let new = ApiKey::generate("deploy key", "usr_1", KeyEnvironment::Live)?;
//! let presented = new.secret.expose();
//! assert!(presented.starts_with("mso_live_"));
//!
//! // What the server does with what the client sent.
//! let (environment, prefix, secret) = ApiKey::parse(presented)?;
//! assert_eq!(environment, KeyEnvironment::Live);
//! assert_eq!(prefix, new.record.prefix);
//! assert!(new.record.verify_secret(&secret));
//! # Ok::<(), moso_auth::Error>(())
//! ```
//!
//! # Why SHA-256 and not argon2
//!
//! A password is short, chosen by a human, and guessable, so the defence is to
//! make each guess expensive. An API-key secret is 256 bits from the system
//! CSPRNG: there is no dictionary, and an attacker who could brute-force it
//! could brute-force the argon2 version too, in the same number of guesses.
//! What a slow hash *would* buy here is a login path that has to run on the
//! blocking pool — on every request, not just at sign-in. SHA-256 with a
//! constant-time compare is the right trade, and this is where it is written
//! down.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moso_core::config::SecretString;
use moso_schema::Id;
use serde::{Deserialize, Serialize};

use crate::jwks::{b64u, ct_eq, random_bytes, sha256_hex};
use crate::{Error, Result};

/// Which environment a key belongs to.
///
/// ```
/// use moso_auth::KeyEnvironment;
///
/// assert_eq!(KeyEnvironment::Live.as_str(), "live");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyEnvironment {
    /// Production.
    Live,
    /// Anything else. A production deployment refuses these, which is how a
    /// test key pasted into the wrong configuration fails loudly.
    Test,
}

impl KeyEnvironment {
    /// The segment written into the key.
    ///
    /// ```
    /// use moso_auth::KeyEnvironment;
    ///
    /// assert_eq!(KeyEnvironment::Test.as_str(), "test");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Test => "test",
        }
    }

    /// The environment a segment names, or `None` for anything else.
    ///
    /// ```
    /// use moso_auth::KeyEnvironment;
    ///
    /// assert_eq!(KeyEnvironment::parse("live"), Some(KeyEnvironment::Live));
    /// assert_eq!(KeyEnvironment::parse("staging"), None);
    /// ```
    #[must_use]
    pub fn parse(segment: &str) -> Option<Self> {
        match segment {
            "live" => Some(Self::Live),
            "test" => Some(Self::Test),
            _ => None,
        }
    }
}

/// The prefix every key starts with.
///
/// ```
/// assert_eq!(moso_auth::apikey::KEY_PREFIX, "mso");
/// ```
pub const KEY_PREFIX: &str = "mso";

/// How many characters of the public prefix are stored and indexed.
///
/// ```
/// assert_eq!(moso_auth::apikey::PREFIX_LENGTH, 8);
/// ```
pub const PREFIX_LENGTH: usize = 8;

/// How many random bytes the public prefix is drawn from.
///
/// Eight hex characters is four bytes. The prefix is not a secret — it is an
/// index — so its only job is to make a collision rare enough that the unique
/// constraint on the column almost never fires. At four bytes an individual
/// insert collides with probability `n / 2^32`, which for any realistic `n` is
/// a retry, not a problem.
const PREFIX_BYTES: usize = PREFIX_LENGTH / 2;

/// How many random bytes the secret carries.
const SECRET_BYTES: usize = 32;

/// The largest presented key this crate will even look at.
///
/// A key is 70-odd characters. Anything past this is somebody probing, and
/// hashing a megabyte of it on the request path is the point of the probe.
const MAX_PRESENTED_LENGTH: usize = 256;

/// An API key as the application stores it.
///
/// Note what is *not* here: the secret. Only [`ApiKey::hash`] is stored, and the
/// full key exists exactly once, in the [`NewApiKey`] returned by
/// [`ApiKey::generate`].
///
/// ```
/// use moso_auth::{ApiKey, KeyEnvironment};
///
/// let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)?;
/// assert_eq!(new.record.prefix.len(), 8);
/// assert_eq!(new.record.hash.len(), 64);
/// # Ok::<(), moso_auth::Error>(())
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ApiKey {
    /// Which key.
    pub id: Id<ApiKey>,
    /// The public prefix, stored in the clear and indexed.
    pub prefix: String,
    /// The SHA-256 of the secret, hex-encoded. Never the secret.
    pub hash: String,
    /// Which environment.
    pub environment: KeyEnvironment,
    /// A human label, so an operator can tell two keys apart.
    pub name: String,
    /// Whose key it is.
    pub owner: String,
    /// What it may do, as permission wire names.
    ///
    /// Strings and not a typed `PermSet`, because this crate does not depend on
    /// `moso-authz` — `xtask/allow/dep-edges.toml` declares `auth -> [orm, kv]`.
    /// `PermSet::parse_all` turns them back into bits, and the key's set
    /// **intersects** the owner's, so a key can never grant more than its owner
    /// has.
    pub scopes: Vec<String>,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it stops working. `None` for a key with no expiry, which the docs
    /// discourage and the admin marks.
    pub expires_at: Option<DateTime<Utc>>,
    /// When it was last used.
    ///
    /// Written asynchronously and rate-limited to at most once a minute per
    /// key: a write per request would make every authenticated request a write
    /// transaction, which is how this feature usually gets removed again.
    pub last_used_at: Option<DateTime<Utc>>,
    /// When it was revoked. A revoked key is kept, not deleted, so an audit can
    /// still resolve it.
    pub revoked_at: Option<DateTime<Utc>>,
}

impl ApiKey {
    /// Whether the key may be used right now.
    ///
    /// ```
    /// use moso_auth::{ApiKey, KeyEnvironment};
    ///
    /// let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)?;
    /// assert!(new.record.is_usable(chrono::Utc::now()));
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn is_usable(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|expiry| now < expiry)
    }

    /// Generate a new key, returning the record and the one-time secret.
    ///
    /// ```
    /// use moso_auth::{ApiKey, KeyEnvironment};
    ///
    /// let new = ApiKey::generate("deploy key", "usr_1", KeyEnvironment::Live)?;
    /// assert!(new.secret.expose().starts_with("mso_live_"));
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the system random generator fails.
    pub fn generate(
        name: impl Into<String>,
        owner: impl Into<String>,
        environment: KeyEnvironment,
    ) -> Result<NewApiKey> {
        let prefix = hex(&random_bytes(PREFIX_BYTES)?);
        // base64url so the secret survives a query string, a shell and a YAML
        // file without quoting, and carries 256 bits in 43 characters.
        let secret = b64u(&random_bytes(SECRET_BYTES)?);
        let presented = format!("{KEY_PREFIX}_{}_{prefix}_{secret}", environment.as_str());
        Ok(NewApiKey {
            record: Self {
                id: Id::new_v7(),
                prefix,
                hash: Self::hash_of(&secret),
                environment,
                name: name.into(),
                owner: owner.into(),
                scopes: Vec::new(),
                created_at: Utc::now(),
                expires_at: None,
                last_used_at: None,
                revoked_at: None,
            },
            secret: SecretString::new(presented),
        })
    }

    /// Parse a presented key into its environment, prefix and secret.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidCredentials`] when the
    /// shape is wrong — deliberately the same error a wrong secret gives.
    ///
    /// ```
    /// use moso_auth::{ApiKey, KeyEnvironment};
    ///
    /// let (environment, prefix, secret) =
    ///     ApiKey::parse("mso_test_0123abcd_c2VjcmV0LXZhbHVl")?;
    /// assert_eq!(environment, KeyEnvironment::Test);
    /// assert_eq!(prefix, "0123abcd");
    /// assert_eq!(secret, "c2VjcmV0LXZhbHVl");
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn parse(presented: &str) -> Result<(KeyEnvironment, String, String)> {
        if presented.len() > MAX_PRESENTED_LENGTH {
            return Err(Error::InvalidCredentials);
        }
        // Four parts: the secret is the remainder, so a base64url `_` inside it
        // is not a delimiter.
        let mut parts = presented.splitn(4, '_');
        let (Some(brand), Some(environment), Some(prefix), Some(secret)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Error::InvalidCredentials);
        };
        if brand != KEY_PREFIX {
            return Err(Error::InvalidCredentials);
        }
        let environment = KeyEnvironment::parse(environment).ok_or(Error::InvalidCredentials)?;
        if prefix.len() != PREFIX_LENGTH
            || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
            || prefix.bytes().any(|byte| byte.is_ascii_uppercase())
        {
            return Err(Error::InvalidCredentials);
        }
        if secret.is_empty() || !secret.bytes().all(is_base64url_byte) {
            return Err(Error::InvalidCredentials);
        }
        Ok((environment, prefix.to_owned(), secret.to_owned()))
    }

    /// The stored form of a presented secret.
    ///
    /// ```
    /// use moso_auth::ApiKey;
    ///
    /// assert_eq!(ApiKey::hash_of("anything").len(), 64);
    /// ```
    #[must_use]
    pub fn hash_of(secret: &str) -> String {
        sha256_hex(secret.as_bytes())
    }

    /// Check a presented secret against this record, in constant time.
    ///
    /// ```
    /// use moso_auth::{ApiKey, KeyEnvironment};
    ///
    /// let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)?;
    /// let (_, _, secret) = ApiKey::parse(new.secret.expose())?;
    /// assert!(new.record.verify_secret(&secret));
    /// assert!(!new.record.verify_secret("not it"));
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn verify_secret(&self, secret: &str) -> bool {
        ct_eq(Self::hash_of(secret).as_bytes(), self.hash.as_bytes())
    }

    /// Whether the key carries a scope, by wire name.
    ///
    /// ```
    /// use moso_auth::{ApiKey, KeyEnvironment};
    ///
    /// let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)?
    ///     .with_scopes(["posts:read"]);
    /// assert!(new.record.has_scope("posts:read"));
    /// assert!(!new.record.has_scope("posts:write"));
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|held| held == scope)
    }
}

/// Whether a byte may appear in a base64url secret.
fn is_base64url_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// Lowercase hex.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        text.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    text
}

/// A freshly generated key. The only time the secret exists.
///
/// [`NewApiKey::secret`] is a [`SecretString`], so it does not appear in a
/// `Debug`, a log or a panic message. It is shown to the user once and then
/// there is no way to recover it.
///
/// ```
/// use moso_auth::{ApiKey, KeyEnvironment};
///
/// let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)?
///     .with_scopes(["posts:read", "posts:write"])
///     .expiring_in(std::time::Duration::from_secs(90 * 24 * 3600));
/// assert_eq!(new.record.scopes.len(), 2);
/// assert!(new.record.expires_at.is_some());
/// # Ok::<(), moso_auth::Error>(())
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub struct NewApiKey {
    /// What to store.
    pub record: ApiKey,
    /// What to show the user, once: `mso_live_<prefix>_<secret>`.
    pub secret: SecretString,
}

impl NewApiKey {
    /// Scope the key to these permission wire names.
    ///
    /// ```
    /// # use moso_auth::{ApiKey, KeyEnvironment};
    /// let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)?
    ///     .with_scopes(["posts:read"]);
    /// assert_eq!(new.record.scopes, vec!["posts:read".to_owned()]);
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn with_scopes<S: Into<String>>(mut self, scopes: impl IntoIterator<Item = S>) -> Self {
        self.record.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Give the key an expiry.
    ///
    /// A key with no expiry is a credential nobody ever revokes, which is why
    /// this exists and why the admin marks the keys that lack it.
    ///
    /// ```
    /// # use moso_auth::{ApiKey, KeyEnvironment};
    /// let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)?
    ///     .expiring_in(std::time::Duration::from_secs(3600));
    /// assert!(new.record.expires_at.is_some());
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn expiring_in(mut self, ttl: Duration) -> Self {
        let seconds = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        let delta = chrono::TimeDelta::try_seconds(seconds).unwrap_or(chrono::TimeDelta::MAX);
        self.record.expires_at = Some(
            self.record
                .created_at
                .checked_add_signed(delta)
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
        );
        self
    }
}

/// Where API keys live.
///
/// Dyn-compatible so the application's table shape is its own business.
///
/// ```no_run
/// use moso_auth::{ApiKey, ApiKeyStore};
///
/// async fn find(store: &dyn ApiKeyStore, prefix: &str) -> moso_auth::Result<Option<ApiKey>> {
///     store.find_by_prefix(prefix).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot store API keys",
    label = "not an API-key store",
    note = "an API-key store implements `insert`, `find_by_prefix`, `list_for_owner`, `revoke` \
            and `touch`",
    note = "help: `find_by_prefix` must be one indexed lookup — that is what the public prefix \
            is for, and a `where hash = $1` would be a timing oracle",
    note = "help: `moso_auth::store::TableApiKeyStore` is the shipped table-backed one; \
            pass `moso_auth::store::descriptors()` to `moso db make-migration` for its table"
)]
pub trait ApiKeyStore: Send + Sync + 'static {
    /// Store a new key.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn insert<'a>(&'a self, key: &'a ApiKey) -> moso_core::BoxFuture<'a, Result<()>>;

    /// Find a key by its public prefix. One indexed lookup.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn find_by_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> moso_core::BoxFuture<'a, Result<Option<ApiKey>>>;

    /// Every key an owner has, revoked ones included.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn list_for_owner<'a>(
        &'a self,
        owner: &'a str,
    ) -> moso_core::BoxFuture<'a, Result<Vec<ApiKey>>>;

    /// Revoke a key. `Ok(false)` when it was already revoked.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn revoke<'a>(&'a self, id: Id<ApiKey>) -> moso_core::BoxFuture<'a, Result<bool>>;

    /// Record that a key was used.
    ///
    /// Called on every authenticated request and expected to be **cheap and
    /// lossy**: buffer, rate-limit to at most once a minute per key, and drop
    /// on backpressure. Last-used tracking is worth having and is not worth a
    /// write transaction per request.
    ///
    /// # Errors
    ///
    /// Never. Failures are logged and counted.
    fn touch<'a>(&'a self, id: Id<ApiKey>, at: DateTime<Utc>) -> moso_core::BoxFuture<'a, ()>;
}

/// An API-key store in process memory.
///
/// Complete, not a test double: it is what `moso new --auth` uses before the
/// application has a database, and every rule the trait states — the prefix
/// index, the revocation tombstone, the lossy touch — is implemented here in
/// the shape a table-backed store must reproduce.
///
/// ```no_run
/// use moso_auth::{ApiKey, ApiKeyStore, KeyEnvironment, MemoryApiKeyStore};
///
/// # async fn f() -> moso_auth::Result<()> {
/// let store = MemoryApiKeyStore::new();
/// let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)?;
/// store.insert(&new.record).await?;
/// assert!(store.find_by_prefix(&new.record.prefix).await?.is_some());
/// # Ok(()) }
/// ```
#[derive(Default)]
pub struct MemoryApiKeyStore {
    /// Keys by prefix — the same index a table needs.
    keys: std::sync::Mutex<HashMap<String, ApiKey>>,
}

impl core::fmt::Debug for MemoryApiKeyStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MemoryApiKeyStore")
            .field("keys", &self.len())
            .finish_non_exhaustive()
    }
}

impl MemoryApiKeyStore {
    /// An empty store.
    ///
    /// ```
    /// use moso_auth::MemoryApiKeyStore;
    ///
    /// assert!(MemoryApiKeyStore::new().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many keys are on record.
    ///
    /// ```
    /// use moso_auth::MemoryApiKeyStore;
    ///
    /// assert_eq!(MemoryApiKeyStore::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.lock().map(|keys| keys.len()).unwrap_or_default()
    }

    /// Whether nothing has been stored.
    ///
    /// ```
    /// use moso_auth::MemoryApiKeyStore;
    ///
    /// assert!(MemoryApiKeyStore::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take the lock, mapping poisoning onto an outage rather than a panic.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, ApiKey>>> {
        self.keys.lock().map_err(|_| Error::Unavailable {
            component: "api key store",
            detail: "the in-memory store's lock was poisoned by a panic".to_owned(),
            source: None,
        })
    }
}

impl ApiKeyStore for MemoryApiKeyStore {
    fn insert<'a>(&'a self, key: &'a ApiKey) -> moso_core::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut keys = self.lock()?;
            if keys.contains_key(&key.prefix) {
                // The unique index a table would carry, stated here so the
                // caller's retry loop is exercised by the same code path.
                return Err(Error::Unavailable {
                    component: "api key store",
                    detail: format!("the prefix `{}` is already taken", key.prefix),
                    source: None,
                });
            }
            keys.insert(key.prefix.clone(), key.clone());
            Ok(())
        })
    }

    fn find_by_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> moso_core::BoxFuture<'a, Result<Option<ApiKey>>> {
        Box::pin(async move { Ok(self.lock()?.get(prefix).cloned()) })
    }

    fn list_for_owner<'a>(
        &'a self,
        owner: &'a str,
    ) -> moso_core::BoxFuture<'a, Result<Vec<ApiKey>>> {
        Box::pin(async move {
            let keys = self.lock()?;
            let mut found: Vec<ApiKey> = keys
                .values()
                .filter(|key| key.owner == owner)
                .cloned()
                .collect();
            found.sort_by_key(|key| key.created_at);
            Ok(found)
        })
    }

    fn revoke<'a>(&'a self, id: Id<ApiKey>) -> moso_core::BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let mut keys = self.lock()?;
            let Some(key) = keys.values_mut().find(|key| key.id == id) else {
                return Ok(false);
            };
            if key.revoked_at.is_some() {
                return Ok(false);
            }
            // Kept, not deleted: an audit trail that cannot resolve a key id is
            // not an audit trail.
            key.revoked_at = Some(Utc::now());
            Ok(true)
        })
    }

    fn touch<'a>(&'a self, id: Id<ApiKey>, at: DateTime<Utc>) -> moso_core::BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Ok(mut keys) = self.keys.lock()
                && let Some(key) = keys.values_mut().find(|key| key.id == id)
            {
                key.last_used_at = Some(at);
            }
        })
    }
}

/// How often at most a key's `last_used_at` is written.
///
/// ```
/// assert_eq!(moso_auth::apikey::TOUCH_INTERVAL.as_secs(), 60);
/// ```
pub const TOUCH_INTERVAL: Duration = Duration::from_secs(60);

/// How many keys the touch rate-limiter remembers.
///
/// Bounded so that a stream of requests with distinct keys cannot grow the map
/// without limit — which is what turns a rate limiter into a memory leak.
const TOUCH_TABLE_CAPACITY: usize = 4096;

/// Authenticates a request that presents an API key.
///
/// Reads `Authorization: Bearer mso_…` — and, for clients that cannot set that
/// header, a configurable one. Parses, looks the prefix up, verifies the secret
/// in constant time, checks expiry and revocation, and records the use.
///
/// ```no_run
/// use std::sync::Arc;
///
/// use moso_auth::{ApiKeyAuthenticator, ApiKeyStore};
///
/// fn build(store: Arc<dyn ApiKeyStore>) -> ApiKeyAuthenticator {
///     ApiKeyAuthenticator::new(store)
/// }
/// ```
#[derive(Clone)]
pub struct ApiKeyAuthenticator {
    /// Where keys live.
    store: Arc<dyn ApiKeyStore>,
    /// An additional header to read a key from, for clients that cannot set
    /// `Authorization`.
    header: Option<String>,
    /// Which environments this process accepts.
    accept: Vec<KeyEnvironment>,
    /// How often at most a key's last-used timestamp is written.
    touch_interval: Duration,
    /// When each key was last touched, so the writes can be rate-limited.
    touched: Arc<std::sync::Mutex<HashMap<Id<ApiKey>, Instant>>>,
}

impl ApiKeyAuthenticator {
    /// An authenticator over `store`, accepting live keys only.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_auth::{ApiKeyAuthenticator, ApiKeyStore};
    /// # fn f(s: Arc<dyn ApiKeyStore>) { let _ = ApiKeyAuthenticator::new(s); }
    /// ```
    #[must_use]
    pub fn new(store: Arc<dyn ApiKeyStore>) -> Self {
        Self {
            store,
            header: None,
            accept: vec![KeyEnvironment::Live],
            touch_interval: TOUCH_INTERVAL,
            touched: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Also read keys from this header.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_auth::{ApiKeyAuthenticator, ApiKeyStore};
    /// # fn f(s: Arc<dyn ApiKeyStore>) { let _ = ApiKeyAuthenticator::new(s).header("x-api-key"); }
    /// ```
    #[must_use]
    pub fn header(mut self, name: impl Into<String>) -> Self {
        self.header = Some(name.into().to_ascii_lowercase());
        self
    }

    /// Which environments to accept. `Live` only by default.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_auth::{ApiKeyAuthenticator, ApiKeyStore, KeyEnvironment};
    /// # fn f(s: Arc<dyn ApiKeyStore>) {
    /// let _ = ApiKeyAuthenticator::new(s).accept([KeyEnvironment::Test]);
    /// # }
    /// ```
    #[must_use]
    pub fn accept(mut self, environments: impl IntoIterator<Item = KeyEnvironment>) -> Self {
        self.accept = environments.into_iter().collect();
        self
    }

    /// How often at most to write a key's last-used timestamp.
    ///
    /// [`Duration::ZERO`] writes on every request, which is what the default
    /// exists to avoid.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_auth::{ApiKeyAuthenticator, ApiKeyStore};
    /// # fn f(s: Arc<dyn ApiKeyStore>) {
    /// let _ = ApiKeyAuthenticator::new(s).touch_at_most_every(std::time::Duration::ZERO);
    /// # }
    /// ```
    #[must_use]
    pub fn touch_at_most_every(mut self, interval: Duration) -> Self {
        self.touch_interval = interval;
        self
    }

    /// The extra header keys may be read from, if one was configured.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_auth::{ApiKeyAuthenticator, ApiKeyStore};
    /// # fn f(s: Arc<dyn ApiKeyStore>) { let _: Option<&str> = ApiKeyAuthenticator::new(s).extra_header(); }
    /// ```
    #[must_use]
    pub fn extra_header(&self) -> Option<&str> {
        self.header.as_deref()
    }

    /// Pull a presented key out of the request headers.
    ///
    /// Reads `Authorization: Bearer …` first, then the configured extra header.
    /// Returns `None` when neither is present, which is "no credentials
    /// presented" and not "wrong credentials" — a distinction the caller needs
    /// to decide between a 401 challenge and an anonymous request.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_auth::{ApiKeyAuthenticator, MemoryApiKeyStore};
    ///
    /// let authenticator = ApiKeyAuthenticator::new(Arc::new(MemoryApiKeyStore::new()));
    /// let mut headers = http::HeaderMap::new();
    /// headers.insert(http::header::AUTHORIZATION, "Bearer mso_live_x".parse().unwrap());
    /// assert_eq!(authenticator.presented_in(&headers), Some("mso_live_x".to_owned()));
    /// ```
    #[must_use]
    pub fn presented_in(&self, headers: &http::HeaderMap) -> Option<String> {
        if let Some(value) = headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
        {
            // Case-insensitive scheme, per RFC 9110 § 11.1.
            let mut parts = value.splitn(2, ' ');
            if let (Some(scheme), Some(token)) = (parts.next(), parts.next())
                && scheme.eq_ignore_ascii_case("bearer")
                && !token.trim().is_empty()
            {
                return Some(token.trim().to_owned());
            }
        }
        let name = self.header.as_deref()?;
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    /// Authenticate a presented key string.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidCredentials`] for any
    /// failure — bad shape, unknown prefix, wrong secret, wrong environment —
    /// so none of them is distinguishable;
    /// [`Error::Revoked`] and [`Error::Expired`] for a key that *was* valid,
    /// because those are actionable by whoever holds it.
    ///
    /// ```no_run
    /// # use moso_auth::{ApiKey, ApiKeyAuthenticator};
    /// # async fn f(a: &ApiKeyAuthenticator, k: &str) -> moso_auth::Result<ApiKey> {
    /// a.authenticate(k).await
    /// # }
    /// ```
    pub async fn authenticate(&self, presented: &str) -> Result<ApiKey> {
        let (environment, prefix, secret) = ApiKey::parse(presented)?;
        if !self.accept.contains(&environment) {
            // A test key in production is indistinguishable from a wrong key,
            // on purpose: the prefix already told the client which it holds.
            tracing::debug!(
                target: "moso_auth::apikey",
                environment = environment.as_str(),
                "an API key from a refused environment was presented"
            );
            return Err(Error::InvalidCredentials);
        }

        let found = self.store.find_by_prefix(&prefix).await?;
        let Some(key) = found else {
            return Err(Error::InvalidCredentials);
        };

        // The secret check happens before the state checks so that "this key was
        // revoked" is only ever said to somebody who actually holds the key —
        // otherwise the prefix alone is an oracle for which keys once existed.
        if !key.verify_secret(&secret) {
            return Err(Error::InvalidCredentials);
        }
        if key.environment != environment {
            return Err(Error::InvalidCredentials);
        }
        if key.revoked_at.is_some() {
            return Err(Error::Revoked { kind: "api key" });
        }
        let now = Utc::now();
        if key.expires_at.is_some_and(|expiry| now >= expiry) {
            return Err(Error::Expired { kind: "api key" });
        }

        self.record_use(key.id, now);
        Ok(key)
    }

    /// Note the use, at most once per key per interval, off the request path.
    ///
    /// Lossy by design: the write is spawned and never awaited, and a full or
    /// unavailable store loses the timestamp rather than the request.
    fn record_use(&self, id: Id<ApiKey>, at: DateTime<Utc>) {
        // `tokio::spawn` needs a runtime. Inside a handler there always is one;
        // in a unit test calling `authenticate` directly there may not be, and
        // losing a last-used timestamp is not worth a panic. Checked *before*
        // the rate limiter, so a call with no runtime does not consume the
        // interval a later call would have used.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if !self.should_touch(id) {
            return;
        }
        let store = Arc::clone(&self.store);
        tokio::spawn(async move {
            store.touch(id, at).await;
        });
    }

    /// Whether enough time has passed to write this key's timestamp again.
    fn should_touch(&self, id: Id<ApiKey>) -> bool {
        let Ok(mut touched) = self.touched.lock() else {
            return false;
        };
        let now = Instant::now();
        if let Some(last) = touched.get(&id)
            && now.duration_since(*last) < self.touch_interval
        {
            return false;
        }
        if touched.len() >= TOUCH_TABLE_CAPACITY {
            // Bounded: drop everything older than the interval, and if that is
            // not enough, clear. Both are safe — the worst case is one extra
            // write per key.
            touched.retain(|_, last| now.duration_since(*last) < self.touch_interval);
            if touched.len() >= TOUCH_TABLE_CAPACITY {
                touched.clear();
            }
        }
        touched.insert(id, now);
        true
    }
}

impl core::fmt::Debug for ApiKeyAuthenticator {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ApiKeyAuthenticator")
            .field("accept", &self.accept)
            .field("header", &self.header)
            .field("touch_interval", &self.touch_interval)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store with one live key, and the secret to present.
    async fn one_key() -> (Arc<MemoryApiKeyStore>, ApiKey, String) {
        let store = Arc::new(MemoryApiKeyStore::new());
        let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live).unwrap();
        store.insert(&new.record).await.unwrap();
        let presented = new.secret.expose().to_owned();
        (store, new.record, presented)
    }

    /// The format is the format. A change here breaks every deployed key and
    /// every secret scanner that was told about the prefix.
    #[test]
    fn the_format_is_the_documented_one() {
        let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live).unwrap();
        let presented = new.secret.expose();
        let parts: Vec<&str> = presented.split('_').collect();
        assert!(parts.len() >= 4, "{presented}");
        assert_eq!(parts[0], "mso");
        assert_eq!(parts[1], "live");
        assert_eq!(parts[2].len(), PREFIX_LENGTH);
        assert!(parts[2].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(parts[2], new.record.prefix);
        // 32 bytes of base64url without padding.
        assert_eq!(parts[3..].join("_").len(), 43);
    }

    /// The secret is never stored, in any form a lookup could reverse.
    #[test]
    fn the_record_does_not_contain_the_secret() {
        let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live).unwrap();
        let (_, _, secret) = ApiKey::parse(new.secret.expose()).unwrap();
        let stored = serde_json::to_string(&new.record).unwrap();
        assert!(!stored.contains(&secret), "the secret reached the record");
        assert_eq!(new.record.hash, ApiKey::hash_of(&secret));
        assert_eq!(new.record.hash.len(), 64);
    }

    /// `Debug` on the one-time key must not print the secret either.
    #[test]
    fn the_debug_impl_does_not_print_the_secret() {
        let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live).unwrap();
        let rendered = format!("{new:?}");
        assert!(!rendered.contains(new.secret.expose()), "{rendered}");
    }

    /// Two keys are never the same, and neither are two prefixes in practice.
    #[test]
    fn generated_keys_are_unique() {
        let mut secrets = std::collections::HashSet::new();
        let mut prefixes = std::collections::HashSet::new();
        for _ in 0..512 {
            let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live).unwrap();
            assert!(secrets.insert(new.secret.expose().to_owned()));
            prefixes.insert(new.record.prefix);
        }
        assert!(
            prefixes.len() > 500,
            "512 draws produced {} distinct prefixes",
            prefixes.len()
        );
    }

    /// Everything malformed is the same error, so the shape of a key is not an
    /// oracle either.
    #[test]
    fn a_malformed_key_is_invalid_credentials() {
        for presented in [
            "",
            "mso",
            "mso_live",
            "mso_live_",
            "mso_live__secret",
            "sk_live_0123abcd_secret",
            "mso_staging_0123abcd_secret",
            "mso_live_0123ABCD_secret",
            "mso_live_0123abc_secret",
            "mso_live_0123abcde_secret",
            "mso_live_zzzzzzzz_secret",
            "mso_live_0123abcd_sec ret",
            &format!("mso_live_0123abcd_{}", "x".repeat(1024)),
        ] {
            assert!(
                matches!(ApiKey::parse(presented), Err(Error::InvalidCredentials)),
                "accepted {presented:?}"
            );
        }
    }

    /// The secret may contain the base64url `_`, so the split must be bounded.
    #[test]
    fn a_secret_containing_an_underscore_survives_the_split() {
        let (_, prefix, secret) = ApiKey::parse("mso_live_0123abcd_aa_bb-cc_dd").unwrap();
        assert_eq!(prefix, "0123abcd");
        assert_eq!(secret, "aa_bb-cc_dd");
    }

    /// A generated key round-trips through the parser, whatever the draw.
    #[test]
    fn every_generated_key_parses() {
        for environment in [KeyEnvironment::Live, KeyEnvironment::Test] {
            for _ in 0..64 {
                let new = ApiKey::generate("ci", "usr_1", environment).unwrap();
                let (parsed, prefix, secret) = ApiKey::parse(new.secret.expose()).unwrap();
                assert_eq!(parsed, environment);
                assert_eq!(prefix, new.record.prefix);
                assert!(new.record.verify_secret(&secret));
            }
        }
    }

    /// A near-miss secret must not verify.
    #[test]
    fn a_wrong_secret_does_not_verify() {
        let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live).unwrap();
        let (_, _, secret) = ApiKey::parse(new.secret.expose()).unwrap();
        let mut wrong = secret.clone();
        wrong.pop();
        wrong.push(if secret.ends_with('a') { 'b' } else { 'a' });
        assert!(new.record.verify_secret(&secret));
        assert!(!new.record.verify_secret(&wrong));
        assert!(!new.record.verify_secret(""));
        assert!(!new.record.verify_secret(&new.record.hash));
    }

    /// Expiry and revocation, as `is_usable` sees them.
    #[test]
    fn usability_covers_expiry_and_revocation() {
        let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live).unwrap();
        let now = Utc::now();
        assert!(new.record.is_usable(now), "a fresh key with no expiry");

        let expiring = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)
            .unwrap()
            .expiring_in(Duration::from_secs(3600));
        assert!(expiring.record.is_usable(now));
        assert!(
            !expiring
                .record
                .is_usable(now + chrono::TimeDelta::try_seconds(7200).unwrap())
        );

        let mut revoked = new.record;
        revoked.revoked_at = Some(now);
        assert!(!revoked.is_usable(now));
    }

    /// The happy path, through the authenticator.
    #[tokio::test]
    async fn a_valid_key_authenticates() {
        let (store, record, presented) = one_key().await;
        let authenticator = ApiKeyAuthenticator::new(store);
        let authenticated = authenticator.authenticate(&presented).await.unwrap();
        assert_eq!(authenticated.id, record.id);
        assert_eq!(authenticated.owner, "usr_1");
    }

    /// One indexed lookup, not a scan. Measured, because "it is indexed" is a
    /// claim about a query plan that a memory store can still get wrong.
    #[tokio::test]
    async fn authentication_is_one_lookup() {
        /// A store that counts what it was asked to do.
        #[derive(Default)]
        struct Counting {
            /// The real store.
            inner: MemoryApiKeyStore,
            /// How many prefix lookups happened.
            lookups: std::sync::atomic::AtomicUsize,
            /// How many owner scans happened. Must stay zero.
            scans: std::sync::atomic::AtomicUsize,
        }

        impl ApiKeyStore for Counting {
            fn insert<'a>(&'a self, key: &'a ApiKey) -> moso_core::BoxFuture<'a, Result<()>> {
                self.inner.insert(key)
            }

            fn find_by_prefix<'a>(
                &'a self,
                prefix: &'a str,
            ) -> moso_core::BoxFuture<'a, Result<Option<ApiKey>>> {
                self.lookups
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.inner.find_by_prefix(prefix)
            }

            fn list_for_owner<'a>(
                &'a self,
                owner: &'a str,
            ) -> moso_core::BoxFuture<'a, Result<Vec<ApiKey>>> {
                self.scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.inner.list_for_owner(owner)
            }

            fn revoke<'a>(&'a self, id: Id<ApiKey>) -> moso_core::BoxFuture<'a, Result<bool>> {
                self.inner.revoke(id)
            }

            fn touch<'a>(
                &'a self,
                id: Id<ApiKey>,
                at: DateTime<Utc>,
            ) -> moso_core::BoxFuture<'a, ()> {
                self.inner.touch(id, at)
            }
        }

        let store = Arc::new(Counting::default());
        let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live).unwrap();
        store.insert(&new.record).await.unwrap();
        let authenticator = ApiKeyAuthenticator::new(Arc::clone(&store) as Arc<dyn ApiKeyStore>);
        authenticator
            .authenticate(new.secret.expose())
            .await
            .unwrap();
        assert_eq!(store.lookups.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(store.scans.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// A revoked key says so — but only to somebody who proved they hold it.
    #[tokio::test]
    async fn a_revoked_key_is_reported_as_revoked() {
        let (store, record, presented) = one_key().await;
        assert!(store.revoke(record.id).await.unwrap());
        assert!(
            !store.revoke(record.id).await.unwrap(),
            "revoking twice is not a second revocation"
        );
        let authenticator = ApiKeyAuthenticator::new(Arc::clone(&store) as Arc<dyn ApiKeyStore>);
        assert!(matches!(
            authenticator.authenticate(&presented).await,
            Err(Error::Revoked { kind: "api key" })
        ));
        assert_eq!(store.len(), 1, "a revoked key is kept for the audit trail");
    }

    /// An expired key says so too.
    #[tokio::test]
    async fn an_expired_key_is_reported_as_expired() {
        let store = Arc::new(MemoryApiKeyStore::new());
        let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)
            .unwrap()
            .expiring_in(Duration::ZERO);
        store.insert(&new.record).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let authenticator = ApiKeyAuthenticator::new(store);
        assert!(matches!(
            authenticator.authenticate(new.secret.expose()).await,
            Err(Error::Expired { kind: "api key" })
        ));
    }

    /// An unknown prefix and a wrong secret are the same error, so neither is
    /// an oracle for which keys exist.
    #[tokio::test]
    async fn an_unknown_prefix_and_a_wrong_secret_are_the_same_error() {
        let (store, _, presented) = one_key().await;
        let authenticator = ApiKeyAuthenticator::new(store);

        let unknown = ApiKey::generate("other", "usr_2", KeyEnvironment::Live).unwrap();
        let a = authenticator.authenticate(unknown.secret.expose()).await;

        // Flip the last character to something it demonstrably is not, so the
        // "tampered" key cannot accidentally be the original.
        let mut tampered = presented;
        let last = tampered.pop().expect("a non-empty key");
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        let b = authenticator.authenticate(&tampered).await;

        assert!(matches!(a, Err(Error::InvalidCredentials)));
        assert!(matches!(b, Err(Error::InvalidCredentials)));
    }

    /// A test key in a production deployment fails, which is what the
    /// environment segment is for.
    #[tokio::test]
    async fn a_test_key_is_refused_by_default() {
        let store = Arc::new(MemoryApiKeyStore::new());
        let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Test).unwrap();
        store.insert(&new.record).await.unwrap();

        let production = ApiKeyAuthenticator::new(Arc::clone(&store) as Arc<dyn ApiKeyStore>);
        assert!(matches!(
            production.authenticate(new.secret.expose()).await,
            Err(Error::InvalidCredentials)
        ));

        let staging = ApiKeyAuthenticator::new(store).accept([KeyEnvironment::Test]);
        assert!(staging.authenticate(new.secret.expose()).await.is_ok());
    }

    /// A key whose stored environment disagrees with the one it presents is a
    /// forgery attempt against the environment gate.
    #[tokio::test]
    async fn a_relabelled_key_is_refused() {
        let store = Arc::new(MemoryApiKeyStore::new());
        let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Test).unwrap();
        let mut record = new.record.clone();
        record.environment = KeyEnvironment::Live;
        store.insert(&record).await.unwrap();

        // The client presents `mso_test_…`, the record says `live`.
        let authenticator =
            ApiKeyAuthenticator::new(store).accept([KeyEnvironment::Live, KeyEnvironment::Test]);
        assert!(matches!(
            authenticator.authenticate(new.secret.expose()).await,
            Err(Error::InvalidCredentials)
        ));
    }

    /// Last-used is written, and rate-limited. Both halves matter: a feature
    /// that never writes is not a feature, and one that writes per request is a
    /// performance bug.
    #[tokio::test]
    async fn last_used_is_written_and_rate_limited() {
        let (store, record, presented) = one_key().await;
        let authenticator = ApiKeyAuthenticator::new(Arc::clone(&store) as Arc<dyn ApiKeyStore>)
            .touch_at_most_every(Duration::ZERO);
        authenticator.authenticate(&presented).await.unwrap();
        // The write is spawned; give it a turn of the scheduler.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let stored = store
            .find_by_prefix(&record.prefix)
            .await
            .unwrap()
            .expect("still there");
        assert!(stored.last_used_at.is_some(), "the timestamp was written");

        // With the default interval, a second authentication inside the minute
        // does not schedule another write.
        let limited = ApiKeyAuthenticator::new(store);
        assert!(limited.should_touch(record.id), "the first use writes");
        assert!(!limited.should_touch(record.id), "the second does not");
    }

    /// The rate-limiter's table is bounded, or it is a memory leak with a
    /// stream of distinct keys.
    #[test]
    fn the_touch_table_is_bounded() {
        let authenticator = ApiKeyAuthenticator::new(Arc::new(MemoryApiKeyStore::new()));
        for _ in 0..(TOUCH_TABLE_CAPACITY + 100) {
            authenticator.should_touch(Id::new_v7());
        }
        let size = authenticator.touched.lock().unwrap().len();
        assert!(size <= TOUCH_TABLE_CAPACITY, "the table grew to {size}");
    }

    /// Where a key is read from, in both spellings.
    #[test]
    fn a_key_is_read_from_the_authorization_header_or_a_named_one() {
        let store = Arc::new(MemoryApiKeyStore::new());
        let plain = ApiKeyAuthenticator::new(Arc::clone(&store) as Arc<dyn ApiKeyStore>);
        let with_header = ApiKeyAuthenticator::new(store).header("X-Api-Key");

        let mut headers = http::HeaderMap::new();
        assert_eq!(plain.presented_in(&headers), None, "nothing presented");

        headers.insert("x-api-key", "mso_live_key".parse().unwrap());
        assert_eq!(
            plain.presented_in(&headers),
            None,
            "not configured to read it"
        );
        assert_eq!(
            with_header.presented_in(&headers),
            Some("mso_live_key".to_owned())
        );

        headers.insert(
            http::header::AUTHORIZATION,
            "bearer  mso_live_other ".parse().unwrap(),
        );
        assert_eq!(
            with_header.presented_in(&headers),
            Some("mso_live_other".to_owned()),
            "Authorization wins, and the scheme is case-insensitive"
        );

        let mut basic = http::HeaderMap::new();
        basic.insert(http::header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(plain.presented_in(&basic), None, "a different scheme");
    }

    /// Scopes are carried and readable; the intersection with the owner's set
    /// is `moso-authz`'s job, and this is the half that lives here.
    #[test]
    fn scopes_are_carried_on_the_record() {
        let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)
            .unwrap()
            .with_scopes(["posts:read", "posts:write"]);
        assert!(new.record.has_scope("posts:read"));
        assert!(!new.record.has_scope("posts:delete"));
        assert_eq!(new.record.scopes.len(), 2);
    }

    /// The store honours the unique index the prefix needs.
    #[tokio::test]
    async fn two_keys_may_not_share_a_prefix() {
        let store = MemoryApiKeyStore::new();
        let first = ApiKey::generate("a", "usr_1", KeyEnvironment::Live).unwrap();
        let mut second = ApiKey::generate("b", "usr_1", KeyEnvironment::Live)
            .unwrap()
            .record;
        second.prefix = first.record.prefix.clone();
        store.insert(&first.record).await.unwrap();
        assert!(store.insert(&second).await.is_err());
    }

    /// Listing is by owner and stable, so an admin page does not reshuffle.
    #[tokio::test]
    async fn keys_list_by_owner_in_creation_order() {
        let store = MemoryApiKeyStore::new();
        for index in 0..3 {
            let new =
                ApiKey::generate(format!("key {index}"), "usr_1", KeyEnvironment::Live).unwrap();
            store.insert(&new.record).await.unwrap();
        }
        let other = ApiKey::generate("theirs", "usr_2", KeyEnvironment::Live).unwrap();
        store.insert(&other.record).await.unwrap();

        let mine = store.list_for_owner("usr_1").await.unwrap();
        assert_eq!(mine.len(), 3);
        assert!(
            mine.windows(2)
                .all(|pair| pair[0].created_at <= pair[1].created_at)
        );
        assert_eq!(store.list_for_owner("nobody").await.unwrap().len(), 0);
    }

    /// Revoking a key that does not exist is `false`, not an error.
    #[tokio::test]
    async fn revoking_an_unknown_key_is_not_an_error() {
        let store = MemoryApiKeyStore::new();
        assert!(!store.revoke(Id::new_v7()).await.unwrap());
    }

    /// Hex is hex.
    #[test]
    fn hex_encodes_every_byte_as_two_lowercase_digits() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x10; 4]).len(), 8);
    }
}
