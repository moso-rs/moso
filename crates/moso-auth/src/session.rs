//! Sessions: the record, the store, the cookie, and the five defaults that
//! make them safe.
//!
//! 1. **The store is not touched unless the handler reads or writes.** A static
//!    endpoint behind a session cookie costs zero round trips.
//! 2. **The id is cycled on login and on any privilege change**, by the
//!    framework. Session fixation is not something a user should have to
//!    remember to defend against.
//! 3. **The cookie is `HttpOnly`, `Secure`, `SameSite=Lax` and `__Host-`**
//!    prefixed wherever the path allows, signed with a rotating key set so a
//!    rotation does not log everybody out.
//! 4. **Expiry is rolling with an absolute cap** — a 14-day idle timeout inside
//!    a 90-day ceiling — so an abandoned session dies and a live one still
//!    eventually ends.
//! 5. **The store fails loudly.** `FailureMode::Fail`, never `Degrade`: an
//!    unreachable store must be a 503, because silently logging everybody out
//!    is worse and much harder to diagnose.
//!
//! # Where the laziness actually is
//!
//! [`SessionLayer`] builds a [`Session`] from the cookie and touches nothing.
//! The first round trip happens when something calls [`Session::load`] — which
//! is what `Depends<AuthSession>`, `Depends<CurrentUser>` and
//! `Depends<MaybeUser>` do, and what an endpoint that names none of them never
//! does. The synchronous [`get`](Session::get) and [`insert`](Session::insert)
//! then operate on the record that is already in hand, which is why they can be
//! synchronous at all.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use moso_core::config::SecretBytes;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Error, Result};

/// The base64url alphabet, without padding, that a session id is written in.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A session's identifier.
///
/// 256 bits from the operating system's generator, base64url-encoded. Not a
/// UUID: a session id is a bearer token, and 122 bits of a v4 UUID is less
/// margin than a token that lives for ninety days deserves.
///
/// ```
/// use moso_auth::SessionId;
///
/// let id = SessionId::generate();
/// assert_eq!(id.as_str().len(), 43);
/// assert_eq!(SessionId::parse(id.as_str()).unwrap(), id);
/// ```
#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// How many bytes of entropy a session id carries.
    ///
    /// ```
    /// assert_eq!(moso_auth::session::SESSION_ID_BYTES, 32);
    /// ```
    pub const BYTES: usize = SESSION_ID_BYTES;

    /// How many characters the base64url encoding of [`SessionId::BYTES`] is.
    ///
    /// ```
    /// use moso_auth::SessionId;
    ///
    /// assert_eq!(SessionId::LEN, 43);
    /// ```
    pub const LEN: usize = SESSION_ID_BYTES.div_ceil(3) * 4 - 1;

    /// A fresh identifier from the system generator.
    ///
    /// # Panics
    ///
    /// If the operating system cannot produce random bytes. There is no safe
    /// fallback: a session id from a predictable source is a session anybody
    /// can guess, so failing to start is the only correct behaviour.
    ///
    /// ```
    /// use moso_auth::SessionId;
    ///
    /// assert_ne!(SessionId::generate(), SessionId::generate());
    /// ```
    #[must_use]
    pub fn generate() -> Self {
        Self(B64.encode(random_bytes::<SESSION_ID_BYTES>()))
    }

    /// Parse one back from a cookie.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidCredentials`] when the
    /// string is not the right shape — deliberately the same error a wrong
    /// password gives, so a malformed cookie is not distinguishable from an
    /// expired one.
    ///
    /// ```
    /// use moso_auth::SessionId;
    ///
    /// assert!(SessionId::parse("too short").is_err());
    /// assert!(SessionId::parse(&"!".repeat(43)).is_err());
    /// ```
    pub fn parse(value: &str) -> Result<Self> {
        let looks_right = value.len() == Self::LEN
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');

        if !looks_right {
            return Err(Error::InvalidCredentials);
        }

        Ok(Self(value.to_owned()))
    }

    /// The identifier.
    ///
    /// ```
    /// use moso_auth::SessionId;
    ///
    /// assert!(!SessionId::generate().as_str().is_empty());
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Debug for SessionId {
    /// Redacted. A session id in a log line is a session id in a log
    /// aggregator, which is a credential in a place nobody is auditing.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SessionId(<redacted>)")
    }
}

/// `N` bytes from the operating system's generator.
///
/// # Panics
///
/// If the system generator fails. See [`SessionId::generate`].
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).expect(
        "the operating system's random generator is unavailable; refusing to issue a \
                 guessable credential",
    );
    bytes
}

/// How many bytes of entropy a session id carries.
///
/// ```
/// assert_eq!(moso_auth::session::SESSION_ID_BYTES, 32);
/// ```
pub const SESSION_ID_BYTES: usize = 32;

/// What a session's owner sees in "your devices".
///
/// Session listing is a feature users expect and almost nobody builds, because
/// it needs an index on the user id. The shipped stores keep a
/// `user:{id}:sessions` set so it is one lookup.
///
/// ```
/// use moso_auth::DeviceInfo;
///
/// let device = DeviceInfo::from_request(
///     Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Firefox/128.0"),
///     Some("203.0.113.7"),
/// );
/// assert_eq!(device.label.as_deref(), Some("Firefox on macOS"));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct DeviceInfo {
    /// The user agent, truncated to 256 characters.
    pub user_agent: Option<String>,
    /// The address the session was created from.
    pub ip: Option<String>,
    /// A coarse label — `"Firefox on macOS"` — derived from the user agent, so
    /// the listing is readable without shipping a full parser to the client.
    pub label: Option<String>,
}

/// The longest user agent that is kept.
///
/// A user agent is attacker-controlled and unbounded; a session record is
/// written to a store with a size limit. 256 characters keeps every real one
/// and truncates the ones that are trying something.
const MAX_USER_AGENT: usize = 256;

impl DeviceInfo {
    /// Derive the device information from a request's headers.
    ///
    /// The label is deliberately coarse — a browser family and an operating
    /// system family — because the listing exists so that a user can recognise
    /// their own devices, not so the application can fingerprint them.
    ///
    /// ```
    /// use moso_auth::DeviceInfo;
    ///
    /// let device = DeviceInfo::from_request(Some("curl/8.4.0"), None);
    /// assert_eq!(device.label.as_deref(), Some("curl"));
    /// ```
    #[must_use]
    pub fn from_request(user_agent: Option<&str>, ip: Option<&str>) -> Self {
        let user_agent =
            user_agent.map(|agent| agent.chars().take(MAX_USER_AGENT).collect::<String>());
        let label = user_agent.as_deref().map(device_label);

        Self {
            user_agent,
            ip: ip.map(str::to_owned),
            label,
        }
    }
}

/// A coarse `"Browser on Platform"` label for a user agent.
fn device_label(agent: &str) -> String {
    let browser = if agent.contains("Edg/") {
        "Edge"
    } else if agent.contains("OPR/") || agent.contains("Opera") {
        "Opera"
    } else if agent.contains("Firefox/") {
        "Firefox"
    } else if agent.contains("Chrome/") {
        "Chrome"
    } else if agent.contains("Safari/") {
        "Safari"
    } else if agent.starts_with("curl/") {
        return "curl".to_owned();
    } else if agent.contains("Moso") || agent.contains("reqwest") {
        return "an API client".to_owned();
    } else {
        "a browser"
    };

    let platform = if agent.contains("iPhone") || agent.contains("iPad") {
        "iOS"
    } else if agent.contains("Android") {
        "Android"
    } else if agent.contains("Mac OS X") || agent.contains("Macintosh") {
        "macOS"
    } else if agent.contains("Windows") {
        "Windows"
    } else if agent.contains("Linux") || agent.contains("X11") {
        "Linux"
    } else {
        return browser.to_owned();
    };

    format!("{browser} on {platform}")
}

/// One session, as the store holds it.
///
/// ```
/// use moso_auth::{SessionId, SessionRecord};
///
/// let record = SessionRecord::new(SessionId::generate());
/// assert!(record.user_id.is_none(), "a pre-login session belongs to nobody");
/// assert_eq!(record.created_at, record.last_seen_at);
/// ```
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct SessionRecord {
    /// Which session.
    pub id: SessionId,
    /// Whose, once authenticated. `None` for a pre-login session holding an
    /// OAuth `state` or a CSRF token.
    ///
    /// The identifier is stored as text: a JSON string identifier is written
    /// verbatim, anything else as its JSON encoding, so an integer key stays
    /// `42` and a UUID stays a UUID. [`Session::log_in`] writes it and
    /// `CurrentUser` reads it back.
    pub user_id: Option<String>,
    /// The [`AuthUser::auth_hash`](crate::AuthUser::auth_hash) as of creation.
    /// A mismatch on load drops the session, which is what makes "log out
    /// everywhere" free.
    pub auth_hash: Vec<u8>,
    /// Arbitrary data the application put in it.
    pub data: serde_json::Value,
    /// When it was created. The absolute timeout counts from here.
    pub created_at: DateTime<Utc>,
    /// When it was last used. The idle timeout counts from here.
    pub last_seen_at: DateTime<Utc>,
    /// Where from.
    pub device: DeviceInfo,
}

impl SessionRecord {
    /// A fresh, empty, unauthenticated record.
    ///
    /// ```
    /// use moso_auth::{SessionId, SessionRecord};
    ///
    /// let record = SessionRecord::new(SessionId::generate());
    /// assert!(record.auth_hash.is_empty());
    /// ```
    #[must_use]
    pub fn new(id: SessionId) -> Self {
        let now = Utc::now();
        Self {
            id,
            user_id: None,
            auth_hash: Vec::new(),
            data: serde_json::Value::Object(serde_json::Map::new()),
            created_at: now,
            last_seen_at: now,
            device: DeviceInfo::default(),
        }
    }

    /// Whether this record has passed either timeout, as of `now`.
    ///
    /// Checked on load as well as by the store's own expiry: a store TTL covers
    /// the idle timeout, and nothing but this covers the absolute one.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use chrono::Utc;
    /// use moso_auth::{SessionConfig, SessionId, SessionRecord};
    ///
    /// let mut record = SessionRecord::new(SessionId::generate());
    /// record.created_at = Utc::now() - chrono::Duration::days(120);
    /// assert!(record.is_expired(&SessionConfig::default(), Utc::now()));
    /// ```
    #[must_use]
    pub fn is_expired(&self, config: &SessionConfig, now: DateTime<Utc>) -> bool {
        let idle = chrono::Duration::from_std(config.idle_timeout).unwrap_or(chrono::Duration::MAX);
        let absolute =
            chrono::Duration::from_std(config.absolute_timeout).unwrap_or(chrono::Duration::MAX);

        now - self.last_seen_at > idle || now - self.created_at > absolute
    }

    /// How long this record should live in the store, as of `now`.
    ///
    /// The smaller of what is left of the idle window and what is left of the
    /// absolute one, so a store that honours the TTL never holds a session the
    /// absolute cap has already ended.
    ///
    /// ```
    /// use moso_auth::{SessionConfig, SessionId, SessionRecord};
    ///
    /// let record = SessionRecord::new(SessionId::generate());
    /// let ttl = record.ttl(&SessionConfig::default(), chrono::Utc::now());
    /// assert!(ttl <= SessionConfig::default().idle_timeout);
    /// ```
    #[must_use]
    pub fn ttl(&self, config: &SessionConfig, now: DateTime<Utc>) -> Duration {
        let since_creation = (now - self.created_at).to_std().unwrap_or(Duration::ZERO);
        let absolute_left = config.absolute_timeout.saturating_sub(since_creation);

        config
            .idle_timeout
            .min(absolute_left)
            .max(Duration::from_secs(1))
    }
}

/// What a [`Session`]'s clones share.
struct SessionInner {
    /// The identifier: from the cookie at first, then whatever cycling made it.
    id: std::sync::RwLock<SessionId>,
    /// The record, once loaded. A standard lock, not an async one, because
    /// every access is a map lookup and the synchronous accessors cannot await.
    record: std::sync::RwLock<Option<SessionRecord>>,
    /// Serialises concurrent first loads, so two extractors asking at once
    /// produce one round trip.
    loading: tokio::sync::Mutex<()>,
    /// Whether the id was presented by the client, so the store may know it.
    presented: AtomicBool,
    /// Whether anything changed and the store needs writing.
    dirty: AtomicBool,
    /// Whether the session ended and the cookie must be cleared.
    destroyed: AtomicBool,
    /// Where it is stored.
    store: Arc<dyn SessionStore>,
    /// How it behaves.
    config: SessionConfig,
    /// What to record about the device, when this session is first written.
    device: DeviceInfo,
}

/// A request's session.
///
/// Lazy: nothing is read from the store until [`load`](Session::load) is
/// called — which the auth extractors do and a static endpoint does not — and
/// nothing is written until the response is being sent. The methods take
/// `&self` because the session lives behind the request context and several
/// extractors may touch it.
///
/// ```
/// use moso_auth::{Session, SessionConfig};
/// use moso_auth::store::MemorySessionStore;
/// use std::sync::Arc;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso_auth::Result<()> {
/// let store = Arc::new(MemorySessionStore::new());
/// let session = Session::detached(store, SessionConfig::default());
///
/// session.load().await?;
/// session.insert("locale", "it-IT")?;
/// assert_eq!(session.get::<String>("locale")?.as_deref(), Some("it-IT"));
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Session {
    /// The shared state: the record, whether it was loaded, whether it changed.
    inner: Arc<SessionInner>,
}

impl Session {
    /// A session that no request produced, for a test or a background task.
    ///
    /// It has a fresh identifier, an empty record and nothing in the store, so
    /// [`load`](Session::load) is a no-op on it.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_auth::{Session, SessionConfig};
    /// use moso_auth::store::MemorySessionStore;
    ///
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// assert!(!session.was_loaded());
    /// ```
    #[must_use]
    pub fn detached(store: Arc<dyn SessionStore>, config: SessionConfig) -> Self {
        Self::build(
            SessionId::generate(),
            false,
            store,
            config,
            DeviceInfo::default(),
        )
    }

    /// The constructor [`SessionLayer`] uses.
    fn build(
        id: SessionId,
        presented: bool,
        store: Arc<dyn SessionStore>,
        config: SessionConfig,
        device: DeviceInfo,
    ) -> Self {
        Self {
            inner: Arc::new(SessionInner {
                id: std::sync::RwLock::new(id),
                record: std::sync::RwLock::new(None),
                loading: tokio::sync::Mutex::new(()),
                presented: AtomicBool::new(presented),
                dirty: AtomicBool::new(false),
                destroyed: AtomicBool::new(false),
                store,
                config,
                device,
            }),
        }
    }

    /// Read the record from the store, if it has not been read already.
    ///
    /// The one round trip. Called by `Depends<AuthSession>`,
    /// `Depends<CurrentUser>` and `Depends<MaybeUser>`; an endpoint that names
    /// none of those never reaches it, which is the whole of the laziness
    /// promise. Calling it twice costs one load, and two tasks calling it at
    /// once cost one load between them.
    ///
    /// A record past either timeout is treated as absent and deleted.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the store cannot
    /// be reached. Never a silent "not logged in".
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// session.load().await?;
    /// assert!(session.was_loaded());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn load(&self) -> Result<()> {
        if self.was_loaded() {
            return Ok(());
        }

        let _guard = self.inner.loading.lock().await;
        if self.was_loaded() {
            return Ok(());
        }

        let id = self.id();
        let loaded = if self.inner.presented.load(Ordering::Acquire) {
            self.inner.store.load(&id).await?
        } else {
            None
        };

        let record = match loaded {
            Some(record) if record.is_expired(&self.inner.config, Utc::now()) => {
                // Expired is indistinguishable from absent to the client, and
                // the row is deleted so the listing does not show a ghost.
                self.inner.store.delete(&record.id).await?;
                self.inner.presented.store(false, Ordering::Release);
                self.fresh_record()
            }
            Some(record) => record,
            None => {
                self.inner.presented.store(false, Ordering::Release);
                self.fresh_record()
            }
        };

        *self
            .inner
            .record
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(record);
        Ok(())
    }

    /// A brand-new record under the current identifier.
    fn fresh_record(&self) -> SessionRecord {
        let mut record = SessionRecord::new(self.id());
        if self.inner.config.track_devices {
            record.device = self.inner.device.clone();
        }
        record
    }

    /// Read a value.
    ///
    /// Operates on the record [`load`](Session::load) put in hand, which is why
    /// it can be synchronous.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the session has not been
    /// loaded — take `Depends<AuthSession>`, or call [`load`](Session::load)
    /// first. A deserialisation failure when the stored value no longer matches
    /// `T`, which a deploy that changed a session value's shape will produce,
    /// and which is why session values should be as small and as stable as
    /// possible.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// session.load().await?;
    /// assert_eq!(session.get::<String>("absent")?, None);
    /// # Ok(())
    /// # }
    /// ```
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.with_record(|record| match record.data.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|error| {
                    Error::Config(
                        format!(
                            "session key `{key}` no longer deserialises as \
                         `{}`: {error}; help: bump the value's shape behind a new key, or clear \
                         it on read",
                            core::any::type_name::<T>()
                        )
                        .into(),
                    )
                }),
        })
    }

    /// Write a value. Marks the session dirty; the store is written once, at
    /// the end of the request.
    ///
    /// # Errors
    ///
    /// A serialisation failure, or [`Error::Config`] when
    /// the session has not been loaded.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// session.load().await?;
    /// session.insert("locale", "it-IT")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn insert<T: Serialize>(&self, key: &str, value: T) -> Result<()> {
        let value = serde_json::to_value(value).map_err(|error| {
            Error::Config(format!("session value for `{key}` does not serialise: {error}").into())
        })?;

        self.with_record_mut(|record| {
            if let serde_json::Value::Object(map) = &mut record.data {
                map.insert(key.to_owned(), value);
            } else {
                let mut map = serde_json::Map::new();
                map.insert(key.to_owned(), value);
                record.data = serde_json::Value::Object(map);
            }
            Ok(())
        })
    }

    /// Remove a value.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the session has not been
    /// loaded.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// session.load().await?;
    /// session.insert("locale", "it-IT")?;
    /// session.remove("locale")?;
    /// assert_eq!(session.get::<String>("locale")?, None);
    /// # Ok(())
    /// # }
    /// ```
    pub fn remove(&self, key: &str) -> Result<()> {
        self.with_record_mut(|record| {
            if let serde_json::Value::Object(map) = &mut record.data {
                map.remove(key);
            }
            Ok(())
        })
    }

    /// Read and remove in one step, for a flash message.
    ///
    /// # Errors
    ///
    /// As [`get`](Session::get).
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// session.load().await?;
    /// session.insert("flash", "saved")?;
    /// assert_eq!(session.take::<String>("flash")?.as_deref(), Some("saved"));
    /// assert_eq!(session.get::<String>("flash")?, None);
    /// # Ok(())
    /// # }
    /// ```
    pub fn take<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let value = self.get(key)?;
        if value.is_some() {
            self.remove(key)?;
        }
        Ok(value)
    }

    /// Issue a new identifier, keeping the contents.
    ///
    /// **Must** be called on login and on any privilege change. The framework
    /// does it for its own login route; an application that authenticates by
    /// another path has to call it, and the documentation says why: without it,
    /// an attacker who can set a victim's cookie before login owns the session
    /// after it.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// session.load().await?;
    /// let before = session.id();
    /// session.cycle_id().await?;
    /// assert_ne!(session.id(), before);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn cycle_id(&self) -> Result<()> {
        self.load().await?;

        let old = self.id();
        let new = SessionId::generate();

        if self.inner.presented.load(Ordering::Acquire) {
            self.inner.store.rename(&old, &new).await?;
        }

        *self
            .inner
            .id
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = new.clone();
        self.with_record_mut(|record| {
            record.id = new;
            Ok(())
        })?;

        Ok(())
    }

    /// End the session and clear the cookie.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// session.load().await?;
    /// session.destroy().await?;
    /// assert!(session.is_destroyed());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn destroy(&self) -> Result<()> {
        let id = self.id();
        if self.inner.presented.load(Ordering::Acquire) {
            self.inner.store.delete(&id).await?;
        }

        *self
            .inner
            .record
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.inner.presented.store(false, Ordering::Release);
        self.inner.destroyed.store(true, Ordering::Release);
        self.inner.dirty.store(false, Ordering::Release);
        Ok(())
    }

    /// Bind the session to a principal. Cycles the id as part of the same step.
    ///
    /// The absolute window restarts here: ninety days from the login, not from
    /// whenever the anonymous session that held the CSRF token happened to be
    /// created.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{DefaultUser, Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// let user = DefaultUser::new("usr_1", b"epoch-0".to_vec());
    /// session.log_in(&user).await?;
    /// assert_eq!(session.user_id().as_deref(), Some("usr_1"));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn log_in<U: crate::AuthUser>(&self, user: &U) -> Result<()> {
        self.load().await?;
        self.cycle_id().await?;

        let subject = encode_subject(&user.auth_id())?;
        let auth_hash = user.auth_hash();
        let now = Utc::now();

        self.with_record_mut(|record| {
            record.user_id = Some(subject);
            record.auth_hash = auth_hash;
            record.created_at = now;
            record.last_seen_at = now;
            Ok(())
        })
    }

    /// The current identifier.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig, SessionId};
    /// # use moso_auth::store::MemorySessionStore;
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// let _: SessionId = session.id();
    /// ```
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.inner
            .id
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Whose session this is, once authenticated.
    ///
    /// Does **not** load the record: if nothing has been read yet this returns
    /// `None`, because the identifier alone does not say who owns it.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// assert_eq!(session.user_id(), None);
    /// ```
    #[must_use]
    pub fn user_id(&self) -> Option<String> {
        self.inner
            .record
            .read()
            .ok()
            .and_then(|record| record.as_ref().and_then(|r| r.user_id.clone()))
    }

    /// The `auth_hash` this session was created with, once loaded.
    ///
    /// What `CurrentUser` compares against the principal it just loaded. A
    /// mismatch drops the session, and that is what makes "log out everywhere"
    /// a single `UPDATE` rather than a scan of the session store.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{DefaultUser, Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// session.log_in(&DefaultUser::new("usr_1", b"epoch-0".to_vec())).await?;
    /// assert_eq!(session.auth_hash().as_deref(), Some(&b"epoch-0"[..]));
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn auth_hash(&self) -> Option<Vec<u8>> {
        self.inner
            .record
            .read()
            .ok()
            .and_then(|record| record.as_ref().map(|r| r.auth_hash.clone()))
    }

    /// Whether the store has been touched at all this request.
    ///
    /// The observable half of the laziness promise, and what the test
    /// asserting "a static endpoint costs zero round trips" reads.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// assert!(!session.was_loaded());
    /// ```
    #[must_use]
    pub fn was_loaded(&self) -> bool {
        self.inner
            .record
            .read()
            .map(|record| record.is_some())
            .unwrap_or(false)
    }

    /// Whether anything changed and the store will be written.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// session.load().await?;
    /// assert!(!session.is_dirty());
    /// session.insert("locale", "it-IT")?;
    /// assert!(session.is_dirty());
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.inner.dirty.load(Ordering::Acquire)
    }

    /// Whether [`destroy`](Session::destroy) ended this session.
    ///
    /// What the layer reads to decide between writing a cookie and clearing
    /// one.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// assert!(!session.is_destroyed());
    /// ```
    #[must_use]
    pub fn is_destroyed(&self) -> bool {
        self.inner.destroyed.load(Ordering::Acquire)
    }

    /// Write the session out, if anything changed. Called by the layer.
    ///
    /// Returns whether anything was written, which is what the "a static
    /// endpoint costs zero round trips" test reads.
    ///
    /// A session that was only read still gets written when its `last_seen_at`
    /// is older than [`SessionConfig::touch_interval`] — that is the rolling
    /// half of the expiry, and skipping it would make every session idle out
    /// fourteen days after it was created rather than after it was last used.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let session = Session::detached(Arc::new(MemorySessionStore::new()), SessionConfig::default());
    /// assert!(!session.save().await?, "an untouched session writes nothing");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn save(&self) -> Result<bool> {
        if self.inner.destroyed.load(Ordering::Acquire) {
            return Ok(false);
        }

        let now = Utc::now();
        let record = {
            let guard = self
                .inner
                .record
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(record) = guard.as_ref() else {
                return Ok(false);
            };

            let stale = (now - record.last_seen_at)
                .to_std()
                .unwrap_or(Duration::ZERO)
                >= self.inner.config.touch_interval;

            if !self.is_dirty() && !stale {
                return Ok(false);
            }

            let mut record = record.clone();
            record.last_seen_at = now;
            record
        };

        let ttl = record.ttl(&self.inner.config, now);
        self.inner.store.save(&record, ttl).await?;

        *self
            .inner
            .record
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(record);
        self.inner.presented.store(true, Ordering::Release);
        self.inner.dirty.store(false, Ordering::Release);
        Ok(true)
    }

    /// Read the loaded record, or explain that nothing loaded it.
    fn with_record<T>(&self, read: impl FnOnce(&SessionRecord) -> Result<T>) -> Result<T> {
        let guard = self
            .inner
            .record
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(record) => read(record),
            None => Err(not_loaded()),
        }
    }

    /// Mutate the loaded record and mark the session dirty.
    fn with_record_mut<T>(&self, write: impl FnOnce(&mut SessionRecord) -> Result<T>) -> Result<T> {
        let mut guard = self
            .inner
            .record
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_mut() {
            Some(record) => {
                let outcome = write(record)?;
                self.inner.dirty.store(true, Ordering::Release);
                Ok(outcome)
            }
            None => Err(not_loaded()),
        }
    }
}

/// The error a synchronous accessor gives when nothing loaded the session.
fn not_loaded() -> Error {
    Error::Config(
        "the session was read before it was loaded; help: take \
         `Depends<AuthSession>` in the handler, or call `Session::load().await` first — the store \
         is deliberately not touched until something asks for it"
            .into(),
    )
}

/// Write an identifier the way [`SessionRecord::user_id`] holds it.
///
/// A JSON string is written verbatim so that `usr_1` stays `usr_1` in a store
/// dump; anything else keeps its JSON encoding so an integer or a UUID round
/// trips.
///
/// # Errors
///
/// [`Error::Config`] when the identifier does not
/// serialise, which is a bug in the application's `AuthUser::Id`.
pub(crate) fn encode_subject<I: Serialize>(id: &I) -> Result<String> {
    let value = serde_json::to_value(id).map_err(|error| {
        Error::Config(format!("`AuthUser::Id` does not serialise: {error}").into())
    })?;

    Ok(match value {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    })
}

/// Read an identifier back out of [`SessionRecord::user_id`].
///
/// # Errors
///
/// [`Error::InvalidCredentials`] when the
/// stored text is not this identifier type — which is what a deploy that
/// changed the user key's type produces, and which must log everybody out
/// rather than authenticate the wrong account.
pub(crate) fn decode_subject<I: DeserializeOwned>(text: &str) -> Result<I> {
    serde_json::from_value::<I>(serde_json::Value::String(text.to_owned()))
        .or_else(|_| serde_json::from_str::<I>(text))
        .map_err(|_| Error::InvalidCredentials)
}

impl core::fmt::Debug for Session {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Session")
            .field("loaded", &self.was_loaded())
            .field("dirty", &self.is_dirty())
            .finish_non_exhaustive()
    }
}

/// Where sessions live.
///
/// Dyn-compatible: an application picks in configuration between a
/// [`KvStore`](moso_kv::KvStore)-backed store and a table.
///
/// ```
/// use moso_auth::{SessionId, SessionStore};
///
/// async fn read(store: &dyn SessionStore, id: &SessionId)
///     -> moso_auth::Result<Option<moso_auth::SessionRecord>>
/// {
///     store.load(id).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a session store",
    label = "not a session store",
    note = "a session store implements `load`, `save`, `delete`, `rename`, `list_for_user` and \
            `delete_for_user`",
    note = "help: use `KvSessionStore` over any `moso_kv::Kv`, or the table-backed store the \
            migration generator creates from `moso_auth_sessions`",
    note = "help: whatever it is, it must fail loudly — a store that degrades to \"no session\" \
            logs every user out on a blip"
)]
pub trait SessionStore: Send + Sync + 'static {
    /// Read a session.
    ///
    /// `Ok(None)` for an unknown or expired id. Never an error: an expired
    /// session is a normal outcome and must not be distinguishable from a
    /// forged one.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] — and *only* that.
    fn load<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<Option<SessionRecord>>>;

    /// Write a session with a time-to-live.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn save<'a>(&'a self, record: &'a SessionRecord, ttl: Duration) -> BoxFuture<'a, Result<()>>;

    /// Delete a session. `Ok(false)` when there was nothing to delete.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn delete<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<bool>>;

    /// Move a session to a new identifier, atomically.
    ///
    /// What [`Session::cycle_id`] needs. Atomic because a copy-then-delete that
    /// fails between the two either duplicates the session or loses it, and
    /// both happen during login, which is the worst time.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn rename<'a>(&'a self, from: &'a SessionId, to: &'a SessionId) -> BoxFuture<'a, Result<()>>;

    /// Every live session for a user, for the "your devices" listing.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn list_for_user<'a>(&'a self, user_id: &'a str) -> BoxFuture<'a, Result<Vec<SessionRecord>>>;

    /// Delete every session for a user, optionally keeping one.
    ///
    /// What "log out my other devices" calls. Returns how many were deleted.
    /// Note that `auth_hash` already invalidates them lazily; this is the eager
    /// version, so the listing empties immediately.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn delete_for_user<'a>(
        &'a self,
        user_id: &'a str,
        except: Option<&'a SessionId>,
    ) -> BoxFuture<'a, Result<u64>>;

    /// A readiness probe.
    ///
    /// The default is `Ok(())`, which is right for a store with nothing remote
    /// to reach. Anything with a connection overrides it.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

moso_kv::namespace! {
    /// One session record. `on_failure = fail`: losing a session must be a 503,
    /// never a silent logout.
    pub(crate) SessionData: str => SessionRecord, on_failure = fail;

    /// The identifiers of one user's live sessions, when the backend has no
    /// set type of its own. `on_failure = fail` for the same reason.
    pub(crate) SessionIndex: str => Vec<String>, on_failure = fail;
}

/// A key-value failure, as a session failure.
///
/// Always [`Error::Unavailable`], never a silent
/// logout — which is why the session namespaces declare `on_failure = fail`.
/// Written out rather than left to `?` so that it does not depend on the
/// `From<moso_kv::Error>` conversion in `error.rs`.
fn kv_failed(error: moso_kv::Error) -> Error {
    Error::Unavailable {
        component: "session store",
        detail: error.to_string(),
        source: Some(Box::new(error)),
    }
}

/// Sessions in a [`moso_kv::Kv`].
///
/// The default. Works over the memory backend in tests, Redis in production and
/// PostgreSQL where an application would rather not run Redis — the same code
/// in all three, which is the whole argument for `moso-kv` existing.
///
/// ```
/// use moso_auth::{KvSessionStore, SessionId, SessionStore};
/// use moso_kv::Kv;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso_auth::Result<()> {
/// let store = KvSessionStore::new(Kv::in_memory("shop").unwrap());
/// assert!(store.load(&SessionId::generate()).await?.is_none());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct KvSessionStore {
    /// Where sessions go.
    kv: moso_kv::Kv,
}

impl KvSessionStore {
    /// A store over `kv`.
    ///
    /// The namespace it uses declares `on_failure = fail`: losing a session is
    /// worse than a 503, so this is the one place in `moso-kv` where degrading
    /// is wrong.
    ///
    /// ```
    /// use moso_auth::KvSessionStore;
    /// use moso_kv::Kv;
    ///
    /// let _ = KvSessionStore::new(Kv::in_memory("shop").unwrap());
    /// ```
    #[must_use]
    pub fn new(kv: moso_kv::Kv) -> Self {
        Self { kv }
    }

    /// The store this reads and writes.
    ///
    /// ```
    /// use moso_auth::KvSessionStore;
    /// use moso_kv::Kv;
    ///
    /// let store = KvSessionStore::new(Kv::in_memory("shop").unwrap());
    /// assert_eq!(store.kv().app(), "shop");
    /// ```
    #[must_use]
    pub fn kv(&self) -> &moso_kv::Kv {
        &self.kv
    }

    /// Add `id` to `user`'s index.
    async fn index_add(&self, user: &str, id: &SessionId, ttl: Duration) -> Result<()> {
        if self.kv.capabilities().structures {
            let key = self.kv.key::<SessionIndex>(user).map_err(kv_failed)?;
            let store = self.kv.store();
            store
                .set_add(&key, &[bytes::Bytes::from(id.as_str().to_owned())])
                .await
                .map_err(kv_failed)?;
            store
                .expire(&key, ttl.max(Duration::from_secs(60)))
                .await
                .map_err(kv_failed)?;
            return Ok(());
        }

        let mut ids = self
            .kv
            .get::<SessionIndex>(user)
            .await
            .map_err(kv_failed)?
            .unwrap_or_default();
        if !ids.iter().any(|held| held == id.as_str()) {
            ids.push(id.as_str().to_owned());
            self.kv
                .set_ttl::<SessionIndex>(user, &ids, ttl)
                .await
                .map_err(kv_failed)?;
        }
        Ok(())
    }

    /// Remove `id` from `user`'s index.
    async fn index_remove(&self, user: &str, id: &SessionId) -> Result<()> {
        if self.kv.capabilities().structures {
            let key = self.kv.key::<SessionIndex>(user).map_err(kv_failed)?;
            self.kv
                .store()
                .set_remove(&key, &[bytes::Bytes::from(id.as_str().to_owned())])
                .await
                .map_err(kv_failed)?;
            return Ok(());
        }

        if let Some(mut ids) = self.kv.get::<SessionIndex>(user).await.map_err(kv_failed)? {
            ids.retain(|held| held != id.as_str());
            self.kv
                .set_ttl::<SessionIndex>(user, &ids, Duration::from_secs(90 * 24 * 3600))
                .await
                .map_err(kv_failed)?;
        }
        Ok(())
    }

    /// Every identifier in `user`'s index.
    async fn index_read(&self, user: &str) -> Result<Vec<SessionId>> {
        let raw = if self.kv.capabilities().structures {
            let key = self.kv.key::<SessionIndex>(user).map_err(kv_failed)?;
            self.kv
                .store()
                .set_members(&key)
                .await
                .map_err(kv_failed)?
                .into_iter()
                .map(|member| String::from_utf8_lossy(&member).into_owned())
                .collect()
        } else {
            self.kv
                .get::<SessionIndex>(user)
                .await
                .map_err(kv_failed)?
                .unwrap_or_default()
        };

        Ok(raw
            .iter()
            .filter_map(|text| SessionId::parse(text).ok())
            .collect())
    }
}

impl SessionStore for KvSessionStore {
    fn load<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<Option<SessionRecord>>> {
        Box::pin(async move {
            self.kv
                .get::<SessionData>(id.as_str())
                .await
                .map_err(kv_failed)
        })
    }

    fn save<'a>(&'a self, record: &'a SessionRecord, ttl: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.kv
                .set_ttl::<SessionData>(record.id.as_str(), record, ttl)
                .await
                .map_err(kv_failed)?;

            if let Some(user) = record.user_id.as_deref() {
                self.index_add(user, &record.id, ttl).await?;
            }
            Ok(())
        })
    }

    fn delete<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            // Read first, so the user's index can be cleaned up: an index that
            // still names a deleted session shows a ghost device in the
            // listing, and users report those as security incidents.
            let existing = self
                .kv
                .get::<SessionData>(id.as_str())
                .await
                .map_err(kv_failed)?;
            let removed = self
                .kv
                .delete::<SessionData>(id.as_str())
                .await
                .map_err(kv_failed)?;

            if let Some(user) = existing.as_ref().and_then(|r| r.user_id.as_deref()) {
                self.index_remove(user, id).await?;
            }
            Ok(removed)
        })
    }

    fn rename<'a>(&'a self, from: &'a SessionId, to: &'a SessionId) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(mut record) = self
                .kv
                .get::<SessionData>(from.as_str())
                .await
                .map_err(kv_failed)?
            else {
                return Ok(());
            };

            let ttl = self
                .kv
                .ttl::<SessionData>(from.as_str())
                .await
                .map_err(kv_failed)?
                .unwrap_or(Duration::from_secs(14 * 24 * 3600));

            record.id = to.clone();

            // Write the new key before dropping the old one: a failure between
            // the two must leave a session that still works, not one that
            // vanished mid-login.
            self.kv
                .set_ttl::<SessionData>(to.as_str(), &record, ttl)
                .await
                .map_err(kv_failed)?;
            self.kv
                .delete::<SessionData>(from.as_str())
                .await
                .map_err(kv_failed)?;

            if let Some(user) = record.user_id.as_deref() {
                self.index_add(user, to, ttl).await?;
                self.index_remove(user, from).await?;
            }
            Ok(())
        })
    }

    fn list_for_user<'a>(&'a self, user_id: &'a str) -> BoxFuture<'a, Result<Vec<SessionRecord>>> {
        Box::pin(async move {
            let mut records = Vec::new();
            let mut stale = Vec::new();

            for id in self.index_read(user_id).await? {
                match self
                    .kv
                    .get::<SessionData>(id.as_str())
                    .await
                    .map_err(kv_failed)?
                {
                    Some(record) => records.push(record),
                    None => stale.push(id),
                }
            }

            // An index entry whose record expired is a ghost. Clean it here
            // rather than in a sweeper: the listing is the only place anybody
            // looks, and it is not a hot path.
            for id in stale {
                self.index_remove(user_id, &id).await?;
            }

            records.sort_by_key(|record| core::cmp::Reverse(record.last_seen_at));
            Ok(records)
        })
    }

    fn delete_for_user<'a>(
        &'a self,
        user_id: &'a str,
        except: Option<&'a SessionId>,
    ) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let mut deleted = 0;
            for id in self.index_read(user_id).await? {
                if except.is_some_and(|keep| *keep == id) {
                    continue;
                }
                if self
                    .kv
                    .delete::<SessionData>(id.as_str())
                    .await
                    .map_err(kv_failed)?
                {
                    deleted += 1;
                }
                self.index_remove(user_id, &id).await?;
            }
            Ok(deleted)
        })
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            match self.kv.health().await {
                moso_core::health::HealthStatus::Up => Ok(()),
                other => Err(Error::Unavailable {
                    component: "session store",
                    detail: format!("{other:?}"),
                    source: None,
                }),
            }
        })
    }
}

/// How the session cookie is written.
///
/// ```
/// use moso_auth::CookieConfig;
///
/// let config = CookieConfig::default();
/// assert_eq!(config.name, "id");
/// assert!(config.http_only);
/// assert!(config.secure);
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CookieConfig {
    /// The cookie's name, before any prefix.
    ///
    /// `"id"` and not `"session"`: with the `__Host-` prefix the full name is
    /// `__Host-id`, and a short name is a few bytes off every request.
    pub name: String,
    /// The path. `"/"` unless an application is mounted under a prefix.
    pub path: String,
    /// The domain. `None` — a host-only cookie — unless subdomains need it,
    /// because a domain cookie is readable by every subdomain including the one
    /// somebody's marketing site is on.
    pub domain: Option<String>,
    /// Never readable from JavaScript. Always true; the field exists so the
    /// value can be asserted, not so it can be changed.
    pub http_only: bool,
    /// Only sent over TLS.
    ///
    /// Always true in production. Auto-detected in development, where a plain
    /// `http://localhost` would otherwise never receive the cookie. Forcing it
    /// off in production requires an explicit flag and logs a warning.
    pub secure: bool,
    /// The `SameSite` attribute.
    pub same_site: SameSite,
    /// Whether to use the `__Host-` prefix.
    ///
    /// It requires `Secure`, `Path=/` and no `Domain`, and in exchange the
    /// browser guarantees no subdomain can have set the cookie. On whenever
    /// those three hold.
    pub host_prefix: bool,
}

impl Default for CookieConfig {
    fn default() -> Self {
        Self {
            name: "id".to_owned(),
            path: "/".to_owned(),
            domain: None,
            http_only: true,
            secure: true,
            same_site: SameSite::Lax,
            host_prefix: true,
        }
    }
}

impl CookieConfig {
    /// Whether the three conditions the `__Host-` prefix requires all hold.
    ///
    /// `Secure`, `Path=/` and no `Domain`. A browser silently ignores a
    /// `__Host-` cookie that breaks any of them, which presents as "login does
    /// not work" with nothing in any log.
    ///
    /// ```
    /// use moso_auth::CookieConfig;
    ///
    /// assert!(CookieConfig::default().host_prefix_applies());
    ///
    /// let mut scoped = CookieConfig::default();
    /// scoped.domain = Some("example.com".to_owned());
    /// assert!(!scoped.host_prefix_applies());
    /// ```
    #[must_use]
    pub fn host_prefix_applies(&self) -> bool {
        self.host_prefix && self.secure && self.path == "/" && self.domain.is_none()
    }

    /// The full cookie name, including the `__Host-` prefix when it applies.
    ///
    /// ```
    /// use moso_auth::CookieConfig;
    ///
    /// assert_eq!(CookieConfig::default().full_name(), "__Host-id");
    ///
    /// let mut dev = CookieConfig::default();
    /// dev.secure = false;
    /// assert_eq!(dev.full_name(), "id");
    /// ```
    #[must_use]
    pub fn full_name(&self) -> String {
        if self.host_prefix_applies() {
            format!("__Host-{}", self.name)
        } else {
            self.name.clone()
        }
    }
}

/// The `SameSite` attribute.
///
/// ```
/// use moso_auth::SameSite;
///
/// assert_eq!(SameSite::default(), SameSite::Lax);
/// assert_eq!(SameSite::Strict.as_str(), "Strict");
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SameSite {
    /// Sent on top-level navigations only. The default: it stops most CSRF
    /// without breaking the "click a link in an email" flow that `Strict`
    /// breaks.
    #[default]
    Lax,
    /// Never sent cross-site. Correct for an application with no inbound links,
    /// and confusing for everyone else.
    Strict,
    /// Always sent. Requires `Secure`, and needs a real reason.
    None,
}

impl SameSite {
    /// The attribute value, as it is written into a `Set-Cookie` header.
    ///
    /// ```
    /// use moso_auth::SameSite;
    ///
    /// assert_eq!(SameSite::Lax.as_str(), "Lax");
    /// assert_eq!(SameSite::None.as_str(), "None");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lax => "Lax",
            Self::Strict => "Strict",
            Self::None => "None",
        }
    }

    /// The matching [`cookie::SameSite`], for rendering a `Set-Cookie` header
    /// through the [`cookie`] crate rather than by hand.
    ///
    /// The three variants map one for one; this is the bridge that lets
    /// [`SessionLayer`] hand its own typed `SameSite` to a [`cookie::Cookie`].
    #[must_use]
    pub(crate) const fn to_cookie(self) -> cookie::SameSite {
        match self {
            Self::Lax => cookie::SameSite::Lax,
            Self::Strict => cookie::SameSite::Strict,
            Self::None => cookie::SameSite::None,
        }
    }
}

/// How sessions behave.
///
/// ```
/// use moso_auth::SessionConfig;
/// use std::time::Duration;
///
/// let config = SessionConfig::default();
/// assert_eq!(config.idle_timeout, Duration::from_secs(14 * 24 * 3600));
/// assert_eq!(config.absolute_timeout, Duration::from_secs(90 * 24 * 3600));
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SessionConfig {
    /// How the cookie is written.
    pub cookie: CookieConfig,
    /// How long a session survives without being used.
    pub idle_timeout: Duration,
    /// How long a session survives at all, however active.
    ///
    /// The cap that stops a session from becoming permanent. Ninety days is the
    /// default because a shorter one is a login prompt users learn to expect
    /// and a longer one is a credential nobody remembers issuing.
    pub absolute_timeout: Duration,
    /// How stale `last_seen_at` may get before a read becomes a write.
    ///
    /// Without this, every authenticated request writes to the store just to
    /// move a timestamp. One minute makes the rolling expiry accurate enough
    /// and the write rate a fraction of the read rate.
    pub touch_interval: Duration,
    /// Whether to record the address and user agent, for "your devices".
    ///
    /// On by default. It is personal data with a retention policy attached, and
    /// an application that would rather not hold it turns this off and loses
    /// only the listing's detail.
    pub track_devices: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            cookie: CookieConfig::default(),
            idle_timeout: Duration::from_secs(14 * 24 * 3600),
            absolute_timeout: Duration::from_secs(90 * 24 * 3600),
            touch_interval: Duration::from_secs(60),
            track_devices: true,
        }
    }
}

impl SessionConfig {
    /// Check for contradictions before the first request.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the idle timeout exceeds
    /// the absolute one, when `SameSite=None` is set without `Secure`, or when
    /// `host_prefix` is on with a `Domain` or a non-root `Path`.
    ///
    /// ```
    /// use moso_auth::{SameSite, SessionConfig};
    ///
    /// SessionConfig::default().validate().unwrap();
    ///
    /// let mut broken = SessionConfig::default();
    /// broken.cookie.secure = false;
    /// broken.cookie.same_site = SameSite::None;
    /// assert!(broken.validate().is_err());
    /// ```
    pub fn validate(&self) -> Result<()> {
        if self.idle_timeout > self.absolute_timeout {
            return Err(Error::Config(
                format!(
                    "session.idle_timeout ({:?}) is longer than session.absolute_timeout ({:?}), \
                     so the absolute cap would never fire; help: lower idle_timeout, or raise \
                     absolute_timeout",
                    self.idle_timeout, self.absolute_timeout
                )
                .into(),
            ));
        }

        if self.cookie.same_site == SameSite::None && !self.cookie.secure {
            return Err(Error::Config(
                "session.cookie.same_site = \"none\" requires session.cookie.secure = true; every \
                 browser rejects the combination; help: set secure = true, or use \"lax\""
                    .into(),
            ));
        }

        if self.cookie.host_prefix && !self.cookie.host_prefix_applies() {
            return Err(Error::Config(
                format!(
                    "session.cookie.host_prefix needs secure = true, path = \"/\" and no domain, \
                     and this configuration has secure = {}, path = {:?}, domain = {:?}; help: \
                     fix one of the three, or set host_prefix = false",
                    self.cookie.secure, self.cookie.path, self.cookie.domain
                )
                .into(),
            ));
        }

        Ok(())
    }
}

/// The header a signed session cookie is separated by: id, a dot, signature.
const SIGNATURE_SEPARATOR: char = '.';

/// The middleware that loads and saves the session.
///
/// Installed into [`Slot::Session`](moso_core::middleware::Slot::Session),
/// which is what makes the laziness work: the layer creates the [`Session`]
/// handle from the cookie without touching the store, and writes it back after
/// the handler only if something changed.
///
/// ```
/// use std::sync::Arc;
///
/// use moso_auth::{SessionConfig, SessionLayer, SessionStore};
/// use moso_auth::store::MemorySessionStore;
/// use moso_core::config::SecretBytes;
///
/// let layer = SessionLayer::new(
///     Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>,
///     SessionConfig::default(),
/// )
/// .keys(vec![SecretBytes::new(vec![7; 32])]);
///
/// layer.validate().unwrap();
/// ```
#[derive(Clone)]
pub struct SessionLayer {
    /// Where sessions live.
    store: Arc<dyn SessionStore>,
    /// How they behave.
    config: SessionConfig,
    /// The keys the cookie is signed with. The first signs; the rest verify, so
    /// a rotation does not log everybody out.
    keys: Vec<SecretBytes>,
}

impl SessionLayer {
    /// A layer over `store`.
    ///
    /// The key set starts empty; [`keys`](SessionLayer::keys) fills it and
    /// [`validate`](SessionLayer::validate) is what turns an empty one into a
    /// boot error rather than an unsigned cookie nobody noticed.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{SessionConfig, SessionLayer, SessionStore};
    /// # use moso_auth::store::MemorySessionStore;
    /// let layer = SessionLayer::new(
    ///     Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>,
    ///     SessionConfig::default(),
    /// );
    /// assert!(layer.validate().is_err(), "an unsigned session cookie is a boot error");
    /// ```
    #[must_use]
    pub fn new(store: Arc<dyn SessionStore>, config: SessionConfig) -> Self {
        Self {
            store,
            config,
            keys: Vec::new(),
        }
    }

    /// The signing keys: the first signs, the rest only verify.
    ///
    /// Rotation is: prepend the new key, deploy, wait longer than the absolute
    /// timeout, drop the old one. Nobody is logged out at any point, which is
    /// the only kind of rotation that actually gets done.
    ///
    /// An empty set is refused: it is left as it was and an error is logged,
    /// because silently turning signing off is the worst possible reading of
    /// `keys(vec![])`. [`validate`](SessionLayer::validate) then reports it.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{SessionConfig, SessionLayer, SessionStore};
    /// # use moso_auth::store::MemorySessionStore;
    /// # use moso_core::config::SecretBytes;
    /// let layer = SessionLayer::new(
    ///     Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>,
    ///     SessionConfig::default(),
    /// )
    /// .keys(vec![SecretBytes::new(vec![1; 32])])
    /// .keys(vec![]);
    ///
    /// assert_eq!(layer.key_count(), 1, "an empty set does not silently disable signing");
    /// ```
    #[must_use]
    pub fn keys(mut self, keys: Vec<SecretBytes>) -> Self {
        if keys.is_empty() {
            tracing::error!(
                target: "moso.auth",
                "SessionLayer::keys was given an empty key set; the previous set is kept and \
                 `SessionLayer::validate` will refuse to boot"
            );
            return self;
        }

        self.keys = keys;
        self
    }

    /// How many keys are installed.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{SessionConfig, SessionLayer, SessionStore};
    /// # use moso_auth::store::MemorySessionStore;
    /// let layer = SessionLayer::new(
    ///     Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>,
    ///     SessionConfig::default(),
    /// );
    /// assert_eq!(layer.key_count(), 0);
    /// ```
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    /// How sessions behave under this layer.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{SessionConfig, SessionLayer, SessionStore};
    /// # use moso_auth::store::MemorySessionStore;
    /// let layer = SessionLayer::new(
    ///     Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>,
    ///     SessionConfig::default(),
    /// );
    /// assert!(layer.config().cookie.http_only);
    /// ```
    #[must_use]
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Check the layer before the first request.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when there is no signing key, or
    /// when [`SessionConfig::validate`] fails.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{SessionConfig, SessionLayer, SessionStore};
    /// # use moso_auth::store::MemorySessionStore;
    /// # use moso_core::config::SecretBytes;
    /// SessionLayer::new(
    ///     Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>,
    ///     SessionConfig::default(),
    /// )
    /// .keys(vec![SecretBytes::new(vec![9; 32])])
    /// .validate()
    /// .unwrap();
    /// ```
    pub fn validate(&self) -> Result<()> {
        self.config.validate()?;

        if self.keys.is_empty() {
            return Err(Error::Config(
                "the session cookie has no signing key; help: set `auth.secret_keys` (32 random \
                 bytes, base64 or hex) and pass it to `SessionLayer::keys`, keeping the previous \
                 key in the list so a rotation does not log everybody out"
                    .into(),
            ));
        }

        if self.keys.iter().any(|key| key.len() < 32) {
            return Err(Error::Config(
                "a session signing key is shorter than 32 bytes; help: generate one with \
                 `openssl rand -base64 32`"
                    .into(),
            ));
        }

        Ok(())
    }

    /// The cookie value for `id`: the identifier, a dot, and its signature.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{SessionConfig, SessionId, SessionLayer, SessionStore};
    /// # use moso_auth::store::MemorySessionStore;
    /// # use moso_core::config::SecretBytes;
    /// let layer = SessionLayer::new(
    ///     Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>,
    ///     SessionConfig::default(),
    /// )
    /// .keys(vec![SecretBytes::new(vec![3; 32])]);
    ///
    /// let id = SessionId::generate();
    /// let signed = layer.sign(&id);
    /// assert_eq!(layer.verify(&signed).as_ref(), Some(&id));
    /// ```
    #[must_use]
    pub fn sign(&self, id: &SessionId) -> String {
        match self.keys.first() {
            Some(key) => format!(
                "{}{SIGNATURE_SEPARATOR}{}",
                id.as_str(),
                B64.encode(hmac_sha256(key.expose(), id.as_str().as_bytes()))
            ),
            None => id.as_str().to_owned(),
        }
    }

    /// The identifier a cookie value carries, if its signature checks out
    /// against **any** installed key.
    ///
    /// Trying every key is the whole of key rotation: the new key signs, the old
    /// ones still verify, and nobody is logged out while the old key is retired.
    /// The comparison is constant-time.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{SessionConfig, SessionId, SessionLayer, SessionStore};
    /// # use moso_auth::store::MemorySessionStore;
    /// # use moso_core::config::SecretBytes;
    /// # let store = || Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// let old = SecretBytes::new(vec![1; 32]);
    /// let new = SecretBytes::new(vec![2; 32]);
    ///
    /// let before = SessionLayer::new(store(), SessionConfig::default()).keys(vec![old.clone()]);
    /// let after = SessionLayer::new(store(), SessionConfig::default()).keys(vec![new, old]);
    ///
    /// let id = SessionId::generate();
    /// assert_eq!(after.verify(&before.sign(&id)).as_ref(), Some(&id));
    /// ```
    #[must_use]
    pub fn verify(&self, cookie: &str) -> Option<SessionId> {
        if self.keys.is_empty() {
            return SessionId::parse(cookie).ok();
        }

        let (id, signature) = cookie.split_once(SIGNATURE_SEPARATOR)?;
        let id = SessionId::parse(id).ok()?;
        let presented = B64.decode(signature).ok()?;

        self.keys
            .iter()
            .any(|key| {
                crate::password::constant_time_eq(
                    &hmac_sha256(key.expose(), id.as_str().as_bytes()),
                    &presented,
                )
            })
            .then_some(id)
    }

    /// Build the request's session from its headers, touching nothing.
    ///
    /// The lazy half. A cookie whose signature does not check out is treated as
    /// absent — the same outcome as no cookie at all, because a client cannot be
    /// told the difference without learning whether its forgery was close.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{SessionConfig, SessionLayer, SessionStore};
    /// # use moso_auth::store::MemorySessionStore;
    /// # use moso_core::config::SecretBytes;
    /// let layer = SessionLayer::new(
    ///     Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>,
    ///     SessionConfig::default(),
    /// )
    /// .keys(vec![SecretBytes::new(vec![5; 32])]);
    ///
    /// let session = layer.begin(&http::HeaderMap::new());
    /// assert!(!session.was_loaded(), "no cookie, and still no round trip");
    /// ```
    #[must_use]
    pub fn begin(&self, headers: &http::HeaderMap) -> Session {
        let name = self.config.cookie.full_name();
        let presented = cookie_value(headers, &name).and_then(|value| self.verify(&value));

        let device = if self.config.track_devices {
            DeviceInfo::from_request(
                headers
                    .get(http::header::USER_AGENT)
                    .and_then(|value| value.to_str().ok()),
                headers
                    .get("x-forwarded-for")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.split(',').next())
                    .map(str::trim),
            )
        } else {
            DeviceInfo::default()
        };

        match presented {
            Some(id) => Session::build(
                id,
                true,
                Arc::clone(&self.store),
                self.config.clone(),
                device,
            ),
            None => Session::build(
                SessionId::generate(),
                false,
                Arc::clone(&self.store),
                self.config.clone(),
                device,
            ),
        }
    }

    /// Save the session and produce the `Set-Cookie` value, if one is needed.
    ///
    /// `None` when nothing changed: re-sending an identical cookie on every
    /// response is bytes on the wire and a cache-key hazard for nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the store cannot
    /// be reached.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{SessionConfig, SessionLayer, SessionStore};
    /// # use moso_auth::store::MemorySessionStore;
    /// # use moso_core::config::SecretBytes;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let layer = SessionLayer::new(
    ///     Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>,
    ///     SessionConfig::default(),
    /// )
    /// .keys(vec![SecretBytes::new(vec![5; 32])]);
    ///
    /// let session = layer.begin(&http::HeaderMap::new());
    /// assert_eq!(layer.finish(&session).await?, None, "an untouched session sets no cookie");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn finish(&self, session: &Session) -> Result<Option<String>> {
        if session.is_destroyed() {
            return Ok(Some(self.clearing_cookie()));
        }

        if !session.save().await? {
            return Ok(None);
        }

        Ok(Some(self.cookie_for(&session.id())))
    }

    /// The `Set-Cookie` value that installs `id`.
    ///
    /// The header is assembled by [`cookie::Cookie`] rather than string
    /// concatenation, so the signed value cannot smuggle a `;` or a control
    /// byte into a second attribute — the escaping is the crate's job, and a
    /// hand-rolled `format!` is exactly where an unescaped value becomes a
    /// second `Set-Cookie` directive. Every attribute is the same one the
    /// [`CookieConfig`] carries: the `__Host-`/plain name from
    /// [`CookieConfig::full_name`], `Path`, the `Domain` only when the
    /// `__Host-` prefix is not in force, `Max-Age` from the idle timeout,
    /// `HttpOnly`, `Secure` and `SameSite`.
    fn cookie_for(&self, id: &SessionId) -> String {
        let cookie = &self.config.cookie;
        let mut builder = cookie::Cookie::build((cookie.full_name(), self.sign(id)))
            .path(cookie.path.clone())
            .http_only(cookie.http_only)
            .secure(cookie.secure)
            .same_site(cookie.same_site.to_cookie())
            .max_age(cookie::time::Duration::seconds(seconds_i64(
                self.config.idle_timeout,
            )));
        if let Some(domain) = cookie.domain.as_deref()
            && !cookie.host_prefix_applies()
        {
            builder = builder.domain(domain.to_owned());
        }
        builder.build().to_string()
    }

    /// The `Set-Cookie` value that removes the session cookie.
    ///
    /// A `Max-Age=0` cookie with the same name, `Path`, `HttpOnly`, `Secure`
    /// and `SameSite` as [`cookie_for`](Self::cookie_for), and no `Domain` —
    /// the browser matches the deletion to the cookie by name and path, and a
    /// `Domain` here would only widen the scope the deletion claims to cover.
    fn clearing_cookie(&self) -> String {
        let cookie = &self.config.cookie;
        cookie::Cookie::build((cookie.full_name(), ""))
            .path(cookie.path.clone())
            .http_only(cookie.http_only)
            .secure(cookie.secure)
            .same_site(cookie.same_site.to_cookie())
            .max_age(cookie::time::Duration::ZERO)
            .build()
            .to_string()
    }
}

/// A [`Duration`] as whole seconds, saturating into an `i64` for
/// [`cookie::time::Duration`].
///
/// A session idle timeout is days, not the tens of thousands of years it would
/// take to overflow, but saturating rather than wrapping keeps a preposterous
/// configuration from minting a negative `Max-Age`.
fn seconds_i64(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

/// The value of the cookie called `name`, if the request sent one.
fn cookie_value(headers: &http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(cookie::Cookie::split_parse_encoded)
        .filter_map(core::result::Result::ok)
        .find(|cookie| cookie.name() == name)
        .map(|cookie| cookie.value().to_owned())
}

/// HMAC-SHA256 of `message` under `key`.
fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

impl core::fmt::Debug for SessionLayer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SessionLayer")
            .field("keys", &self.keys.len())
            .finish_non_exhaustive()
    }
}

impl moso_core::middleware::CustomLayer for SessionLayer {
    fn name(&self) -> &'static str {
        "session"
    }

    fn apply(&self, service: moso_core::router::Route) -> moso_core::router::Route {
        moso_core::router::Route::new(SessionService {
            inner: service,
            layer: self.clone(),
        })
    }

    fn summary(&self) -> String {
        format!(
            "{}, idle {}, absolute {}, {} signing key(s)",
            self.config.cookie.full_name(),
            humantime_secs(self.config.idle_timeout),
            humantime_secs(self.config.absolute_timeout),
            self.keys.len()
        )
    }
}

/// A duration in whole days or hours, for the middleware summary.
fn humantime_secs(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else {
        format!("{secs}s")
    }
}

/// The service [`SessionLayer`] wraps a route in.
#[derive(Clone)]
struct SessionService {
    /// What runs after the session is in the extensions.
    inner: moso_core::router::Route,
    /// The layer's configuration and keys.
    layer: SessionLayer,
}

impl tower::Service<moso_core::Request> for SessionService {
    type Response = moso_core::Response;
    type Error = core::convert::Infallible;
    type Future = BoxFuture<'static, core::result::Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<core::result::Result<(), Self::Error>> {
        tower::Service::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, mut request: moso_core::Request) -> Self::Future {
        // The clone-and-swap dance: `self.inner` is the instance that was
        // polled ready, so it is the one that must be called.
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);
        let layer = self.layer.clone();

        Box::pin(async move {
            let session = layer.begin(request.headers());
            request.extensions_mut().insert(session.clone());

            let mut response = inner.call(request).await?;

            match layer.finish(&session).await {
                Ok(Some(cookie)) => match http::HeaderValue::from_str(&cookie) {
                    Ok(value) => {
                        response
                            .headers_mut()
                            .append(http::header::SET_COOKIE, value);
                    }
                    Err(error) => tracing::error!(
                        target: "moso.auth",
                        %error,
                        "the session cookie is not a valid header value"
                    ),
                },
                Ok(None) => {}
                Err(error) => {
                    // A store that cannot be written must not silently drop the
                    // session: the response becomes a 503, because a user whose
                    // login vanished has no way to know it did.
                    tracing::error!(target: "moso.auth", %error, "the session could not be saved");
                    return Ok(moso_core::IntoResponse::into_response(
                        moso_core::Error::unavailable("the session store could not be written"),
                    ));
                }
            }

            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemorySessionStore;

    /// A store and a layer with one key, which is what every test here wants.
    fn layer() -> (Arc<MemorySessionStore>, SessionLayer) {
        let store = Arc::new(MemorySessionStore::new());
        let layer = SessionLayer::new(
            Arc::clone(&store) as Arc<dyn SessionStore>,
            SessionConfig::default(),
        )
        .keys(vec![SecretBytes::new(vec![42; 32])]);
        (store, layer)
    }

    #[test]
    fn an_identifier_is_256_bits_of_base64url() {
        let id = SessionId::generate();
        assert_eq!(id.as_str().len(), SessionId::LEN);
        assert_eq!(B64.decode(id.as_str()).unwrap().len(), SESSION_ID_BYTES);
        assert_eq!(SessionId::parse(id.as_str()).unwrap(), id);
    }

    #[test]
    fn a_thousand_identifiers_never_repeat() {
        let ids: std::collections::HashSet<_> = (0..1000).map(|_| SessionId::generate()).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn a_malformed_identifier_is_the_same_error_a_wrong_password_is() {
        for bad in [
            "",
            "short",
            &"!".repeat(43),
            &"a".repeat(44),
            &"a".repeat(42),
        ] {
            assert!(
                matches!(SessionId::parse(bad), Err(Error::InvalidCredentials)),
                "`{bad}` should have been refused as invalid credentials"
            );
        }
    }

    #[test]
    fn an_identifier_never_prints_itself() {
        let id = SessionId::generate();
        let printed = format!("{id:?}");
        assert_eq!(printed, "SessionId(<redacted>)");
        assert!(!printed.contains(id.as_str()));
    }

    #[test]
    fn the_host_prefix_is_only_used_when_the_browser_would_accept_it() {
        assert_eq!(CookieConfig::default().full_name(), "__Host-id");

        let with_domain = CookieConfig {
            domain: Some("example.com".to_owned()),
            ..CookieConfig::default()
        };
        assert_eq!(with_domain.full_name(), "id");

        let scoped = CookieConfig {
            path: "/app".to_owned(),
            ..CookieConfig::default()
        };
        assert_eq!(scoped.full_name(), "id");

        let insecure = CookieConfig {
            secure: false,
            ..CookieConfig::default()
        };
        assert_eq!(insecure.full_name(), "id");
    }

    #[test]
    fn the_configuration_refuses_the_combinations_a_browser_would() {
        let mut none_without_secure = SessionConfig::default();
        none_without_secure.cookie.secure = false;
        none_without_secure.cookie.host_prefix = false;
        none_without_secure.cookie.same_site = SameSite::None;
        assert!(none_without_secure.validate().is_err());

        let idle_past_absolute = SessionConfig {
            idle_timeout: Duration::from_secs(200 * 24 * 3600),
            ..SessionConfig::default()
        };
        assert!(idle_past_absolute.validate().is_err());

        let mut prefix_with_domain = SessionConfig::default();
        prefix_with_domain.cookie.domain = Some("example.com".to_owned());
        assert!(prefix_with_domain.validate().is_err());

        SessionConfig::default().validate().unwrap();
    }

    #[test]
    fn a_layer_without_a_key_refuses_to_boot() {
        let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
        let bare = SessionLayer::new(store, SessionConfig::default());
        let error = bare.validate().unwrap_err();
        assert!(error.to_string().contains("signing key"));
    }

    #[test]
    fn a_short_key_refuses_to_boot() {
        let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
        let weak = SessionLayer::new(store, SessionConfig::default())
            .keys(vec![SecretBytes::new(vec![1; 16])]);
        assert!(weak.validate().is_err());
    }

    #[test]
    fn a_tampered_cookie_is_indistinguishable_from_no_cookie() {
        let (_, layer) = layer();
        let id = SessionId::generate();
        let signed = layer.sign(&id);

        assert_eq!(layer.verify(&signed).as_ref(), Some(&id));

        // Flip one character of the signature.
        let (body, signature) = signed.split_once('.').unwrap();
        let mut broken = signature.to_owned();
        broken.replace_range(0..1, if signature.starts_with('A') { "B" } else { "A" });
        assert_eq!(layer.verify(&format!("{body}.{broken}")), None);

        // Swap the identifier for another, keeping the signature.
        let other = SessionId::generate();
        assert_eq!(
            layer.verify(&format!("{}.{signature}", other.as_str())),
            None
        );

        // No signature at all.
        assert_eq!(layer.verify(id.as_str()), None);
    }

    #[test]
    fn a_key_rotation_does_not_log_anybody_out() {
        let store = || Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
        let old = SecretBytes::new(vec![1; 32]);
        let new = SecretBytes::new(vec![2; 32]);

        let before = SessionLayer::new(store(), SessionConfig::default()).keys(vec![old.clone()]);
        let during = SessionLayer::new(store(), SessionConfig::default())
            .keys(vec![new.clone(), old.clone()]);
        let after = SessionLayer::new(store(), SessionConfig::default()).keys(vec![new]);

        let id = SessionId::generate();
        let signed_by_old = before.sign(&id);

        assert_eq!(
            during.verify(&signed_by_old).as_ref(),
            Some(&id),
            "an old cookie must still verify while the old key is retained"
        );
        assert_eq!(
            after.verify(&signed_by_old),
            None,
            "and must stop verifying once the old key is dropped"
        );
        assert_eq!(
            during.verify(&during.sign(&id)).as_ref(),
            Some(&id),
            "new cookies are signed by the new key"
        );
    }

    #[test]
    fn the_set_cookie_header_carries_every_documented_attribute() {
        let (_, layer) = layer();
        let cookie = layer.cookie_for(&SessionId::generate());

        assert!(cookie.starts_with("__Host-id="));
        assert!(cookie.contains("; Path=/"));
        assert!(cookie.contains("; HttpOnly"));
        assert!(cookie.contains("; Secure"));
        assert!(cookie.contains("; SameSite=Lax"));
        assert!(!cookie.contains("Domain="), "__Host- forbids a Domain");
    }

    #[test]
    fn the_clearing_cookie_expires_immediately() {
        let (_, layer) = layer();
        let cookie = layer.clearing_cookie();
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.starts_with("__Host-id=;"));
    }

    #[test]
    fn the_set_cookie_header_parses_back_to_its_security_attributes() {
        // Rendered through the `cookie` crate, the header must re-parse to the
        // exact attributes the layer set — that is what proves the switch away
        // from `format!` changed no byte of the security envelope.
        let (_, layer) = layer();
        let id = SessionId::generate();

        let parsed = cookie::Cookie::parse(layer.cookie_for(&id)).expect("a well-formed cookie");

        assert_eq!(parsed.name(), "__Host-id");
        assert_eq!(parsed.http_only(), Some(true));
        assert_eq!(
            parsed.secure(),
            Some(true),
            "Secure survives in the prod default"
        );
        assert_eq!(parsed.same_site(), Some(cookie::SameSite::Lax));
        assert_eq!(parsed.path(), Some("/"));
        assert_eq!(parsed.domain(), None, "__Host- forbids a Domain");
        // The value is the signed identifier, untouched: `cookie` renders it
        // verbatim, so it reads back byte for byte and still verifies.
        assert_eq!(parsed.value(), layer.sign(&id));
        assert_eq!(layer.verify(parsed.value()).as_ref(), Some(&id));
    }

    #[test]
    fn dropping_secure_drops_it_from_the_rendered_header() {
        // A dev profile without `Secure` must render *without* it — the cookie
        // crate only auto-adds `Secure` for `SameSite=None`, and Lax stays bare.
        let store = Arc::new(MemorySessionStore::new());
        let mut config = SessionConfig::default();
        config.cookie.secure = false;
        config.cookie.host_prefix = false;
        let layer = SessionLayer::new(Arc::clone(&store) as Arc<dyn SessionStore>, config)
            .keys(vec![SecretBytes::new(vec![9; 32])]);

        let rendered = layer.cookie_for(&SessionId::generate());
        assert!(!rendered.contains("Secure"), "{rendered}");
        assert!(rendered.contains("HttpOnly"), "{rendered}");
        assert!(rendered.contains("SameSite=Lax"), "{rendered}");
        assert!(
            rendered.starts_with("id="),
            "no __Host- without Secure: {rendered}"
        );
    }

    #[test]
    fn a_configured_domain_is_rendered_when_the_host_prefix_is_off() {
        let store = Arc::new(MemorySessionStore::new());
        let mut config = SessionConfig::default();
        config.cookie.host_prefix = false;
        config.cookie.domain = Some("app.example.com".to_owned());
        let layer = SessionLayer::new(Arc::clone(&store) as Arc<dyn SessionStore>, config)
            .keys(vec![SecretBytes::new(vec![9; 32])]);

        let parsed =
            cookie::Cookie::parse(layer.cookie_for(&SessionId::generate())).expect("well-formed");
        assert_eq!(parsed.domain(), Some("app.example.com"));
    }

    #[tokio::test]
    async fn a_request_with_no_session_interest_costs_zero_round_trips() {
        let (store, layer) = layer();

        let session = layer.begin(&http::HeaderMap::new());
        assert_eq!(store.round_trips(), 0);

        // The handler never looks at it, so `finish` writes nothing either.
        assert_eq!(layer.finish(&session).await.unwrap(), None);
        assert_eq!(
            store.round_trips(),
            0,
            "a static endpoint behind a session cookie must not touch the store"
        );
    }

    #[tokio::test]
    async fn a_request_that_reads_the_session_costs_exactly_one_load() {
        let (store, layer) = layer();
        let id = SessionId::generate();
        let mut record = SessionRecord::new(id.clone());
        record.user_id = Some("usr_1".to_owned());
        store.save(&record, Duration::from_secs(60)).await.unwrap();
        store.reset_round_trips();

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            format!("__Host-id={}", layer.sign(&id)).parse().unwrap(),
        );

        let session = layer.begin(&headers);
        session.load().await.unwrap();
        session.load().await.unwrap();
        session.load().await.unwrap();

        assert_eq!(store.round_trips(), 1, "three loads, one round trip");
        assert_eq!(session.user_id().as_deref(), Some("usr_1"));
    }

    #[tokio::test]
    async fn two_tasks_loading_at_once_share_one_round_trip() {
        let (store, layer) = layer();
        let id = SessionId::generate();
        store
            .save(&SessionRecord::new(id.clone()), Duration::from_secs(60))
            .await
            .unwrap();
        store.set_load_delay(Duration::from_millis(30));
        store.reset_round_trips();

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            format!("__Host-id={}", layer.sign(&id)).parse().unwrap(),
        );
        let session = layer.begin(&headers);

        let (first, second) = tokio::join!(session.load(), session.load());
        first.unwrap();
        second.unwrap();

        assert_eq!(store.round_trips(), 1);
    }

    #[tokio::test]
    async fn writing_marks_the_session_dirty_and_saving_writes_once() {
        let (store, layer) = layer();
        let session = layer.begin(&http::HeaderMap::new());

        session.load().await.unwrap();
        assert!(!session.is_dirty());

        session.insert("locale", "it-IT").unwrap();
        assert!(session.is_dirty());

        assert!(layer.finish(&session).await.unwrap().is_some());
        assert!(!session.is_dirty());
        assert_eq!(store.writes(), 1);

        // Nothing changed since, so nothing is written.
        assert_eq!(layer.finish(&session).await.unwrap(), None);
        assert_eq!(store.writes(), 1);
    }

    #[tokio::test]
    async fn a_synchronous_read_before_a_load_names_the_problem() {
        let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
        let session = Session::detached(store, SessionConfig::default());

        let error = session.get::<String>("locale").unwrap_err();
        assert!(matches!(error, Error::Config(_)));
        assert!(
            error.to_string().contains("Depends<AuthSession>"),
            "the message must name the fix: {error}"
        );
    }

    #[tokio::test]
    async fn session_values_round_trip_through_the_store() {
        let (_, layer) = layer();
        let session = layer.begin(&http::HeaderMap::new());
        session.load().await.unwrap();

        session.insert("locale", "it-IT").unwrap();
        session.insert("count", 7_u32).unwrap();
        layer.finish(&session).await.unwrap();

        let id = session.id();
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            format!("__Host-id={}", layer.sign(&id)).parse().unwrap(),
        );

        let next = layer.begin(&headers);
        next.load().await.unwrap();
        assert_eq!(
            next.get::<String>("locale").unwrap().as_deref(),
            Some("it-IT")
        );
        assert_eq!(next.get::<u32>("count").unwrap(), Some(7));
        assert_eq!(
            next.take::<String>("locale").unwrap().as_deref(),
            Some("it-IT")
        );
        assert_eq!(next.get::<String>("locale").unwrap(), None);
    }

    #[tokio::test]
    async fn a_value_whose_shape_changed_names_the_key() {
        let (_, layer) = layer();
        let session = layer.begin(&http::HeaderMap::new());
        session.load().await.unwrap();
        session.insert("locale", "it-IT").unwrap();

        let error = session.get::<u32>("locale").unwrap_err();
        assert!(error.to_string().contains("locale"));
    }

    /// Session fixation: the id an attacker planted must not survive login.
    #[tokio::test]
    async fn logging_in_cycles_the_identifier_and_moves_the_record() {
        let (store, layer) = layer();
        let planted = SessionId::generate();
        let mut record = SessionRecord::new(planted.clone());
        record.data = serde_json::json!({ "cart": 3 });
        store.save(&record, Duration::from_secs(60)).await.unwrap();

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            format!("__Host-id={}", layer.sign(&planted))
                .parse()
                .unwrap(),
        );

        let session = layer.begin(&headers);
        session
            .log_in(&crate::DefaultUser::new("usr_1", b"epoch-0".to_vec()))
            .await
            .unwrap();
        layer.finish(&session).await.unwrap();

        assert_ne!(session.id(), planted, "the planted id must not survive");
        assert_eq!(
            store.load(&planted).await.unwrap().map(|r| r.id),
            None,
            "the old key must be gone, not merely unused"
        );

        let moved = store.load(&session.id()).await.unwrap().unwrap();
        assert_eq!(
            moved.data,
            serde_json::json!({ "cart": 3 }),
            "contents survive"
        );
        assert_eq!(moved.user_id.as_deref(), Some("usr_1"));
        assert_eq!(moved.auth_hash, b"epoch-0".to_vec());
    }

    #[tokio::test]
    async fn logging_in_restarts_the_absolute_window() {
        let (store, layer) = layer();
        let id = SessionId::generate();
        let mut record = SessionRecord::new(id.clone());
        record.created_at = Utc::now() - chrono::Duration::days(80);
        record.last_seen_at = Utc::now();
        store.save(&record, Duration::from_secs(60)).await.unwrap();

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            format!("__Host-id={}", layer.sign(&id)).parse().unwrap(),
        );
        let session = layer.begin(&headers);
        session
            .log_in(&crate::DefaultUser::new("usr_1", b"epoch".to_vec()))
            .await
            .unwrap();
        layer.finish(&session).await.unwrap();

        let after = store.load(&session.id()).await.unwrap().unwrap();
        assert!(
            (Utc::now() - after.created_at).num_seconds() < 5,
            "ninety days should count from the login, not from the anonymous session"
        );
    }

    #[tokio::test]
    async fn destroying_a_session_deletes_it_and_clears_the_cookie() {
        let (store, layer) = layer();
        let session = layer.begin(&http::HeaderMap::new());
        session.load().await.unwrap();
        session.insert("locale", "it-IT").unwrap();
        layer.finish(&session).await.unwrap();

        let id = session.id();
        assert!(store.load(&id).await.unwrap().is_some());

        session.destroy().await.unwrap();
        let cookie = layer.finish(&session).await.unwrap().unwrap();

        assert!(store.load(&id).await.unwrap().is_none());
        assert!(cookie.contains("Max-Age=0"));
    }

    #[tokio::test]
    async fn a_session_past_its_absolute_cap_is_treated_as_absent_and_deleted() {
        let (store, layer) = layer();
        let id = SessionId::generate();
        let mut record = SessionRecord::new(id.clone());
        record.created_at = Utc::now() - chrono::Duration::days(120);
        record.last_seen_at = Utc::now();
        record.user_id = Some("usr_1".to_owned());
        store.save(&record, Duration::from_secs(600)).await.unwrap();

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            format!("__Host-id={}", layer.sign(&id)).parse().unwrap(),
        );

        let session = layer.begin(&headers);
        session.load().await.unwrap();

        assert_eq!(
            session.user_id(),
            None,
            "an expired session belongs to nobody"
        );
        assert!(
            store.load(&id).await.unwrap().is_none(),
            "and is cleaned up"
        );
    }

    #[tokio::test]
    async fn a_session_past_its_idle_timeout_is_treated_as_absent() {
        let (store, layer) = layer();
        let id = SessionId::generate();
        let mut record = SessionRecord::new(id.clone());
        record.last_seen_at = Utc::now() - chrono::Duration::days(20);
        record.user_id = Some("usr_1".to_owned());
        store.save(&record, Duration::from_secs(600)).await.unwrap();

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            format!("__Host-id={}", layer.sign(&id)).parse().unwrap(),
        );

        let session = layer.begin(&headers);
        session.load().await.unwrap();
        assert_eq!(session.user_id(), None);
    }

    #[tokio::test]
    async fn a_read_only_request_still_rolls_the_expiry_forward() {
        let (store, layer) = layer();
        let id = SessionId::generate();
        let mut record = SessionRecord::new(id.clone());
        record.last_seen_at = Utc::now() - chrono::Duration::minutes(30);
        store.save(&record, Duration::from_secs(600)).await.unwrap();
        store.reset_round_trips();

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            format!("__Host-id={}", layer.sign(&id)).parse().unwrap(),
        );

        let session = layer.begin(&headers);
        session.load().await.unwrap();
        assert!(
            layer.finish(&session).await.unwrap().is_some(),
            "a session last seen half an hour ago is touched"
        );

        let after = store.load(&id).await.unwrap().unwrap();
        assert!((Utc::now() - after.last_seen_at).num_seconds() < 5);
    }

    #[tokio::test]
    async fn a_freshly_read_session_is_not_written_again() {
        let (store, layer) = layer();
        let id = SessionId::generate();
        store
            .save(&SessionRecord::new(id.clone()), Duration::from_secs(600))
            .await
            .unwrap();
        store.reset_round_trips();

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            format!("__Host-id={}", layer.sign(&id)).parse().unwrap(),
        );

        let session = layer.begin(&headers);
        session.load().await.unwrap();
        assert_eq!(
            layer.finish(&session).await.unwrap(),
            None,
            "`touch_interval` is what keeps a read from becoming a write"
        );
        assert_eq!(store.writes(), 0);
    }

    #[test]
    fn the_ttl_is_the_smaller_of_the_two_windows() {
        let config = SessionConfig::default();
        let mut record = SessionRecord::new(SessionId::generate());

        // A young session: the idle window is the binding one.
        assert_eq!(record.ttl(&config, Utc::now()), config.idle_timeout);

        // An old one: what is left of the absolute window binds.
        record.created_at = Utc::now() - chrono::Duration::days(88);
        let ttl = record.ttl(&config, Utc::now());
        assert!(ttl < config.idle_timeout);
        assert!(ttl <= Duration::from_secs(2 * 24 * 3600 + 60));
    }

    #[test]
    fn a_device_label_is_coarse_and_readable() {
        let cases = [
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Gecko/20100101 Firefox/128.0",
                "Firefox on macOS",
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0 Safari/537.36",
                "Chrome on Windows",
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1 Safari/604.1",
                "Safari on iOS",
            ),
            ("curl/8.4.0", "curl"),
        ];

        for (agent, expected) in cases {
            let device = DeviceInfo::from_request(Some(agent), Some("203.0.113.7"));
            assert_eq!(device.label.as_deref(), Some(expected), "for `{agent}`");
            assert_eq!(device.ip.as_deref(), Some("203.0.113.7"));
        }
    }

    #[test]
    fn an_absurd_user_agent_is_truncated() {
        let device = DeviceInfo::from_request(Some(&"x".repeat(10_000)), None);
        assert_eq!(device.user_agent.unwrap().chars().count(), MAX_USER_AGENT);
    }

    #[test]
    fn devices_are_not_recorded_when_the_application_says_not_to() {
        let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
        let config = SessionConfig {
            track_devices: false,
            ..SessionConfig::default()
        };

        let layer = SessionLayer::new(store, config).keys(vec![SecretBytes::new(vec![8; 32])]);
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::USER_AGENT, "curl/8.4.0".parse().unwrap());

        let session = layer.begin(&headers);
        assert_eq!(session.inner.device, DeviceInfo::default());
    }

    #[test]
    fn a_subject_round_trips_for_every_shape_of_identifier() {
        assert_eq!(encode_subject(&"usr_1".to_owned()).unwrap(), "usr_1");
        assert_eq!(encode_subject(&42_u64).unwrap(), "42");

        assert_eq!(decode_subject::<String>("usr_1").unwrap(), "usr_1");
        assert_eq!(decode_subject::<u64>("42").unwrap(), 42);

        let id = uuid::Uuid::new_v4();
        assert_eq!(
            decode_subject::<uuid::Uuid>(&encode_subject(&id).unwrap()).unwrap(),
            id
        );

        assert!(matches!(
            decode_subject::<u64>("not a number"),
            Err(Error::InvalidCredentials)
        ));
    }

    #[tokio::test]
    async fn the_kv_store_round_trips_a_session_and_lists_it() {
        let store = KvSessionStore::new(moso_kv::Kv::in_memory("auth-test").unwrap());
        let id = SessionId::generate();
        let mut record = SessionRecord::new(id.clone());
        record.user_id = Some("usr_7".to_owned());
        record.device = DeviceInfo::from_request(Some("curl/8.4.0"), Some("203.0.113.7"));

        store.save(&record, Duration::from_secs(600)).await.unwrap();

        let loaded = store.load(&id).await.unwrap().unwrap();
        assert_eq!(loaded.user_id.as_deref(), Some("usr_7"));
        assert_eq!(loaded.device.label.as_deref(), Some("curl"));

        let listed = store.list_for_user("usr_7").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
    }

    #[tokio::test]
    async fn the_kv_store_renames_atomically_enough_to_never_lose_a_session() {
        let store = KvSessionStore::new(moso_kv::Kv::in_memory("auth-rename").unwrap());
        let from = SessionId::generate();
        let to = SessionId::generate();
        let mut record = SessionRecord::new(from.clone());
        record.user_id = Some("usr_9".to_owned());
        store.save(&record, Duration::from_secs(600)).await.unwrap();

        store.rename(&from, &to).await.unwrap();

        assert!(store.load(&from).await.unwrap().is_none());
        let moved = store.load(&to).await.unwrap().unwrap();
        assert_eq!(moved.id, to);

        let listed = store.list_for_user("usr_9").await.unwrap();
        assert_eq!(listed.len(), 1, "the index followed the rename");
        assert_eq!(listed[0].id, to);
    }

    #[tokio::test]
    async fn the_kv_store_revokes_every_session_but_the_current_one() {
        let store = KvSessionStore::new(moso_kv::Kv::in_memory("auth-revoke").unwrap());

        let mut ids = Vec::new();
        for _ in 0..4 {
            let id = SessionId::generate();
            let mut record = SessionRecord::new(id.clone());
            record.user_id = Some("usr_3".to_owned());
            store.save(&record, Duration::from_secs(600)).await.unwrap();
            ids.push(id);
        }

        let keep = ids[0].clone();
        let deleted = store.delete_for_user("usr_3", Some(&keep)).await.unwrap();

        assert_eq!(deleted, 3);
        let remaining = store.list_for_user("usr_3").await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, keep);
    }

    #[tokio::test]
    async fn the_kv_store_forgets_a_session_it_deleted() {
        let store = KvSessionStore::new(moso_kv::Kv::in_memory("auth-delete").unwrap());
        let id = SessionId::generate();
        let mut record = SessionRecord::new(id.clone());
        record.user_id = Some("usr_5".to_owned());
        store.save(&record, Duration::from_secs(600)).await.unwrap();

        assert!(store.delete(&id).await.unwrap());
        assert!(!store.delete(&id).await.unwrap());
        assert!(
            store.list_for_user("usr_5").await.unwrap().is_empty(),
            "a deleted session must not linger in the listing as a ghost device"
        );
    }

    #[tokio::test]
    async fn the_kv_store_probes_healthy() {
        let store = KvSessionStore::new(moso_kv::Kv::in_memory("auth-probe").unwrap());
        store.probe().await.unwrap();
    }
}
