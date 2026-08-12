//! The account lifecycle: register, verify, reset, change password, change
//! address, log out, log out everywhere.
//!
//! These are the flows every application rewrites and most get subtly wrong.
//! What is here is the *logic* — the ordering, the token handling, the epoch
//! bump, the enumeration defences — with the two things that cannot be
//! generic left to the application behind traits:
//!
//! - **Where accounts live** is [`AccountStore`]. A user row has columns Moso
//!   knows nothing about, and a `create` that took a fixed struct would be
//!   wrong for the second application that used it.
//! - **How mail is sent** is nowhere. Every flow *returns* the token it minted;
//!   the caller sends it. That is what keeps `moso-auth` from depending on
//!   `moso-mail`, and what lets an application send its own template through
//!   its own provider on its own queue.
//!
//! # The five rules these flows follow
//!
//! 1. **Nothing reveals whether an account exists.** Registering an address
//!    that is taken, asking to reset a password for an address that is not
//!    there, and resending a verification to nobody all return the same shape
//!    and do the same work. [`Registration::outcome`] tells the *server* what
//!    happened, and the documentation says loudly that it must not reach a
//!    client.
//! 2. **A token is single-use, short-lived, and stored hashed.** A dump of the
//!    token store is not a set of live password resets.
//! 3. **A password change bumps the epoch**, which invalidates every session
//!    through `auth_hash` at the next request, and eagerly deletes them so the
//!    "your devices" listing empties immediately.
//! 4. **Changing an address is double opt-in**: the new address must confirm
//!    before the change lands, so a typo cannot lock an account away and an
//!    attacker with a borrowed session cannot silently take the account over.
//! 5. **Changing a password requires the current one**, even inside an
//!    authenticated session. Without it an unattended browser is a password
//!    change, and a password change is everything.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use moso_schema::Password;
use serde::{Deserialize, Serialize};

use crate::password::PasswordHash;
use crate::session::{Session, SessionId, SessionStore, encode_subject};
use crate::{AuthUser, Error, PasswordPolicy, Result};

/// The alphabet a lifecycle token is written in.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// How many bytes of entropy a verification or reset token carries.
///
/// The same 256 bits a session id gets, for the same reason: it is a bearer
/// credential, and one that arrives by email where it may sit in a mailbox for
/// an hour.
///
/// ```
/// assert_eq!(moso_auth::lifecycle::TOKEN_BYTES, 32);
/// ```
pub const TOKEN_BYTES: usize = 32;

/// What a lifecycle token is for.
///
/// The purpose is part of the stored key, so a verification token cannot be
/// presented as a password reset. Confusing one for another is a real
/// vulnerability class — an emailed "confirm your address" link that also
/// resets a password is an account takeover by mail forwarding.
///
/// ```
/// use moso_auth::TokenPurpose;
///
/// assert_eq!(TokenPurpose::ResetPassword.as_str(), "reset_password");
/// assert_ne!(TokenPurpose::VerifyEmail, TokenPurpose::ChangeEmail);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TokenPurpose {
    /// Prove that a newly registered address is reachable.
    VerifyEmail,
    /// Set a new password without knowing the old one.
    ResetPassword,
    /// Confirm the *new* address of an address change.
    ChangeEmail,
}

impl TokenPurpose {
    /// The name used in the store key and in log fields.
    ///
    /// ```
    /// use moso_auth::TokenPurpose;
    ///
    /// assert_eq!(TokenPurpose::VerifyEmail.as_str(), "verify_email");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VerifyEmail => "verify_email",
            Self::ResetPassword => "reset_password",
            Self::ChangeEmail => "change_email",
        }
    }
}

/// A token that was minted, and everything the caller needs to deliver it.
///
/// The plaintext exists exactly here and nowhere else: the store keeps only its
/// SHA-256. Send it, then drop it.
///
/// ```no_run
/// use moso_auth::IssuedToken;
///
/// # fn f(token: &IssuedToken) {
/// let _ = (token.expose(), &token.destination, token.expires_at);
/// # }
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub struct IssuedToken {
    /// The secret, base64url. Redacted from `Debug`.
    token: String,
    /// What it is for.
    pub purpose: TokenPurpose,
    /// Which account it acts on, in the same text form the session records:
    /// a string identifier verbatim, anything else as its JSON encoding.
    pub subject: String,
    /// Where it should be sent — the address, which for a change-of-address
    /// token is the *new* one.
    pub destination: String,
    /// When it stops working.
    pub expires_at: DateTime<Utc>,
}

impl IssuedToken {
    /// The secret, for putting in a link.
    ///
    /// ```no_run
    /// # use moso_auth::IssuedToken;
    /// # fn f(token: &IssuedToken) -> String {
    /// format!("https://example.com/verify?token={}", token.expose())
    /// # }
    /// ```
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.token
    }
}

impl core::fmt::Debug for IssuedToken {
    /// Redacted. A reset token in a log line is a password reset in a log
    /// aggregator.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IssuedToken")
            .field("purpose", &self.purpose)
            .field("subject", &self.subject)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// What a stored token stands for.
///
/// ```
/// use moso_auth::lifecycle::TokenClaim;
///
/// # fn f(claim: &TokenClaim) {
/// let _ = (&claim.subject, &claim.destination);
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TokenClaim {
    /// Which account, in the same text form the session records.
    pub subject: String,
    /// The address the token was sent to.
    pub destination: String,
    /// When it was minted.
    pub issued_at: DateTime<Utc>,
    /// When it stops working.
    pub expires_at: DateTime<Utc>,
}

/// Where single-use lifecycle tokens live.
///
/// Dyn-compatible, because an application picks the same store its sessions use
/// and the flows do not care which it was.
///
/// ```no_run
/// use moso_auth::{LifecycleTokens, TokenPurpose};
///
/// async fn burn(tokens: &dyn LifecycleTokens, token: &str) -> moso_auth::Result<bool> {
///     Ok(tokens
///         .consume(TokenPurpose::ResetPassword, token)
///         .await?
///         .is_some())
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot hold lifecycle tokens",
    label = "not a lifecycle token store",
    note = "a lifecycle token store implements `issue`, `consume` and `revoke_all`",
    note = "help: `KvLifecycleTokens::new(kv)` works over the same `moso_kv::Kv` the sessions \
            use, and stores only a SHA-256 of each token",
    note = "help: whatever it is, `consume` must be single-use — a reset token that works twice \
            is a reset token an attacker can replay"
)]
pub trait LifecycleTokens: Send + Sync + 'static {
    /// Mint a token for `subject`, to be delivered to `destination`.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn issue<'a>(
        &'a self,
        purpose: TokenPurpose,
        subject: &'a str,
        destination: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<IssuedToken>>;

    /// Redeem a token, which must then never work again.
    ///
    /// `Ok(None)` for a token that is unknown, expired, already used or of the
    /// wrong purpose — deliberately one answer, because telling them apart
    /// tells an attacker whether they guessed a real token.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn consume<'a>(
        &'a self,
        purpose: TokenPurpose,
        token: &'a str,
    ) -> BoxFuture<'a, Result<Option<TokenClaim>>>;

    /// Invalidate every outstanding token of this purpose for this subject.
    ///
    /// What issuing a new password reset does first. Without it, a reset an
    /// attacker triggered an hour ago still works after the real user has
    /// reset their own password.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn revoke_all<'a>(
        &'a self,
        purpose: TokenPurpose,
        subject: &'a str,
    ) -> BoxFuture<'a, Result<u64>>;
}

moso_kv::namespace! {
    /// One outstanding lifecycle token, keyed by the SHA-256 of the secret.
    pub(crate) LifecycleToken: str => TokenClaim, on_failure = fail;

    /// The digests of one subject's outstanding tokens, so they can be revoked
    /// together.
    pub(crate) LifecycleTokenIndex: str => Vec<String>, on_failure = fail;
}

/// A key-value failure, as a lifecycle failure.
///
/// Always [`Error::Unavailable`]: a token store that
/// could not be read has not said the token is bad, and answering "invalid
/// token" would send a user round the reset loop for as long as the outage
/// lasted.
fn kv_failed(error: moso_kv::Error) -> Error {
    Error::Unavailable {
        component: "token store",
        detail: error.to_string(),
        source: Some(Box::new(error)),
    }
}

/// Lifecycle tokens in a [`moso_kv::Kv`].
///
/// The default. Only the SHA-256 of a token is stored, so a dump of the store
/// is not a set of live password resets, and the TTL is the store's so an
/// abandoned token disappears without a sweeper.
///
/// ```
/// use moso_auth::lifecycle::KvLifecycleTokens;
/// use moso_auth::{LifecycleTokens, TokenPurpose};
/// use moso_kv::Kv;
/// use std::time::Duration;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso_auth::Result<()> {
/// let tokens = KvLifecycleTokens::new(Kv::in_memory("shop").unwrap());
///
/// let issued = tokens
///     .issue(TokenPurpose::VerifyEmail, "usr_1", "ada@example.com", Duration::from_secs(60))
///     .await?;
///
/// let claim = tokens.consume(TokenPurpose::VerifyEmail, issued.expose()).await?;
/// assert_eq!(claim.unwrap().subject, "usr_1");
///
/// // Single use.
/// assert!(tokens.consume(TokenPurpose::VerifyEmail, issued.expose()).await?.is_none());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct KvLifecycleTokens {
    /// Where the digests go.
    kv: moso_kv::Kv,
}

impl KvLifecycleTokens {
    /// A token store over `kv`.
    ///
    /// ```
    /// use moso_auth::lifecycle::KvLifecycleTokens;
    /// use moso_kv::Kv;
    ///
    /// let _ = KvLifecycleTokens::new(Kv::in_memory("shop").unwrap());
    /// ```
    #[must_use]
    pub fn new(kv: moso_kv::Kv) -> Self {
        Self { kv }
    }

    /// A token store over `kv`, behind an [`Arc`].
    ///
    /// ```
    /// use moso_auth::lifecycle::KvLifecycleTokens;
    /// use moso_auth::LifecycleTokens;
    /// use moso_kv::Kv;
    ///
    /// let tokens: std::sync::Arc<dyn LifecycleTokens> =
    ///     KvLifecycleTokens::shared(Kv::in_memory("shop").unwrap());
    /// let _ = tokens;
    /// ```
    #[must_use]
    pub fn shared(kv: moso_kv::Kv) -> Arc<Self> {
        Arc::new(Self::new(kv))
    }

    /// The key a token is stored under: its purpose and its digest.
    fn key_for(purpose: TokenPurpose, token: &str) -> String {
        format!("{}:{}", purpose.as_str(), digest(token))
    }

    /// The key a subject's index is stored under.
    fn index_for(purpose: TokenPurpose, subject: &str) -> String {
        format!("{}:{subject}", purpose.as_str())
    }
}

/// The SHA-256 of a token, base64url, which is what is actually stored.
fn digest(token: &str) -> String {
    use sha2::{Digest, Sha256};

    B64.encode(Sha256::digest(token.as_bytes()))
}

impl LifecycleTokens for KvLifecycleTokens {
    fn issue<'a>(
        &'a self,
        purpose: TokenPurpose,
        subject: &'a str,
        destination: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<IssuedToken>> {
        Box::pin(async move {
            let mut entropy = [0_u8; TOKEN_BYTES];
            getrandom::fill(&mut entropy).map_err(|error| Error::Unavailable {
                component: "system random generator",
                detail: error.to_string(),
                source: None,
            })?;
            let token = B64.encode(entropy);

            let now = Utc::now();
            let expires_at = now
                + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::hours(1));

            let claim = TokenClaim {
                subject: subject.to_owned(),
                destination: destination.to_owned(),
                issued_at: now,
                expires_at,
            };

            let key = Self::key_for(purpose, &token);
            self.kv
                .set_ttl::<LifecycleToken>(&key, &claim, ttl)
                .await
                .map_err(kv_failed)?;

            let index = Self::index_for(purpose, subject);
            let mut held = self
                .kv
                .get::<LifecycleTokenIndex>(&index)
                .await
                .map_err(kv_failed)?
                .unwrap_or_default();
            held.push(key);
            self.kv
                .set_ttl::<LifecycleTokenIndex>(&index, &held, ttl)
                .await
                .map_err(kv_failed)?;

            Ok(IssuedToken {
                token,
                purpose,
                subject: subject.to_owned(),
                destination: destination.to_owned(),
                expires_at,
            })
        })
    }

    fn consume<'a>(
        &'a self,
        purpose: TokenPurpose,
        token: &'a str,
    ) -> BoxFuture<'a, Result<Option<TokenClaim>>> {
        Box::pin(async move {
            let key = Self::key_for(purpose, token);
            let Some(claim) = self
                .kv
                .get::<LifecycleToken>(&key)
                .await
                .map_err(kv_failed)?
            else {
                return Ok(None);
            };

            // Delete before returning: a token that is read and then fails to
            // be deleted must not be usable a second time.
            self.kv
                .delete::<LifecycleToken>(&key)
                .await
                .map_err(kv_failed)?;

            let index = Self::index_for(purpose, &claim.subject);
            if let Some(mut held) = self
                .kv
                .get::<LifecycleTokenIndex>(&index)
                .await
                .map_err(kv_failed)?
            {
                held.retain(|entry| entry != &key);
                self.kv
                    .set_ttl::<LifecycleTokenIndex>(&index, &held, Duration::from_secs(3600))
                    .await
                    .map_err(kv_failed)?;
            }

            if claim.expires_at <= Utc::now() {
                return Ok(None);
            }

            Ok(Some(claim))
        })
    }

    fn revoke_all<'a>(
        &'a self,
        purpose: TokenPurpose,
        subject: &'a str,
    ) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let index = Self::index_for(purpose, subject);
            let Some(held) = self
                .kv
                .get::<LifecycleTokenIndex>(&index)
                .await
                .map_err(kv_failed)?
            else {
                return Ok(0);
            };

            let mut revoked = 0;
            for key in &held {
                if self
                    .kv
                    .delete::<LifecycleToken>(key)
                    .await
                    .map_err(kv_failed)?
                {
                    revoked += 1;
                }
            }
            self.kv
                .delete::<LifecycleTokenIndex>(&index)
                .await
                .map_err(kv_failed)?;
            Ok(revoked)
        })
    }
}

/// Everything an application knows about an account it is about to create.
///
/// The password arrives already hashed: an [`AccountStore`] never sees a
/// plaintext password, which means an application's own logging cannot leak
/// one.
///
/// ```
/// use moso_auth::NewAccount;
///
/// # fn f(account: &NewAccount) {
/// let _ = (account.identity(), account.password_hash(), account.profile_value());
/// # }
/// ```
#[derive(Debug)]
pub struct NewAccount {
    /// The address or username, normalised.
    identity: String,
    /// The PHC hash of the chosen password.
    password_hash: PasswordHash,
    /// Whatever else the application collected at registration, as JSON, so a
    /// second application with a different signup form needs no new trait.
    profile: serde_json::Value,
}

impl NewAccount {
    /// An account with an identity and a hash.
    ///
    /// ```no_run
    /// # use moso_auth::{NewAccount, PasswordHash};
    /// # fn f(hash: PasswordHash) { let _ = NewAccount::new("ada@example.com", hash); }
    /// ```
    #[must_use]
    pub fn new(identity: impl Into<String>, password_hash: PasswordHash) -> Self {
        Self {
            identity: identity.into(),
            password_hash,
            profile: serde_json::Value::Null,
        }
    }

    /// Attach whatever else the signup form collected.
    ///
    /// ```no_run
    /// # use moso_auth::{NewAccount, PasswordHash};
    /// # fn f(hash: PasswordHash) {
    /// let _ = NewAccount::new("ada@example.com", hash)
    ///     .profile(serde_json::json!({ "name": "Ada" }));
    /// # }
    /// ```
    #[must_use]
    pub fn profile(mut self, profile: serde_json::Value) -> Self {
        self.profile = profile;
        self
    }

    /// The address or username.
    ///
    /// ```no_run
    /// # use moso_auth::NewAccount;
    /// # fn f(a: &NewAccount) -> &str { a.identity() }
    /// ```
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// The PHC hash to store.
    ///
    /// ```no_run
    /// # use moso_auth::{NewAccount, PasswordHash};
    /// # fn f(a: &NewAccount) -> &PasswordHash { a.password_hash() }
    /// ```
    #[must_use]
    pub fn password_hash(&self) -> &PasswordHash {
        &self.password_hash
    }

    /// Whatever else the signup form collected.
    ///
    /// ```no_run
    /// # use moso_auth::NewAccount;
    /// # fn f(a: &NewAccount) -> &serde_json::Value { a.profile_value() }
    /// ```
    #[must_use]
    pub fn profile_value(&self) -> &serde_json::Value {
        &self.profile
    }

    /// Whatever else the signup form collected, decoded.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the profile is not `T`.
    ///
    /// ```no_run
    /// # use moso_auth::NewAccount;
    /// # #[derive(serde::Deserialize)] struct Profile { name: String }
    /// # fn f(a: &NewAccount) -> moso_auth::Result<Profile> { a.profile_as::<Profile>() }
    /// ```
    pub fn profile_as<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.profile.clone()).map_err(|error| {
            Error::Config(
                format!("the registration profile is not the expected shape: {error}").into(),
            )
        })
    }
}

/// Where accounts live.
///
/// Everything the lifecycle flows need to do to a user row, and nothing else. A
/// row has columns Moso knows nothing about, so `create` takes a
/// [`NewAccount`] rather than a fixed struct, and the application decides what
/// a "verified" account or a "session epoch" actually is.
///
/// ```no_run
/// use moso_auth::{AccountStore, AuthUser, DefaultUser, NewAccount, PasswordHash, Result};
/// use moso_core::BoxFuture;
///
/// /// An account store that has exactly one account. For a test.
/// pub struct OneAccount;
///
/// impl AccountStore for OneAccount {
///     type User = DefaultUser;
///
///     fn find_by_identity<'a>(&'a self, identity: &'a str)
///         -> BoxFuture<'a, Result<Option<DefaultUser>>>
///     {
///         Box::pin(async move {
///             Ok((identity == "ada@example.com")
///                 .then(|| DefaultUser::new("usr_1", b"epoch".to_vec())))
///         })
///     }
///
///     fn find_by_id<'a>(&'a self, id: &'a String)
///         -> BoxFuture<'a, Result<Option<DefaultUser>>>
///     {
///         let id = id.clone();
///         Box::pin(async move { Ok(Some(DefaultUser::new(id, b"epoch".to_vec()))) })
///     }
///
///     fn create<'a>(&'a self, account: &'a NewAccount)
///         -> BoxFuture<'a, Result<DefaultUser>>
///     {
///         let id = account.identity().to_owned();
///         Box::pin(async move { Ok(DefaultUser::new(id, b"epoch".to_vec())) })
///     }
///
///     fn password_hash<'a>(&'a self, _id: &'a String)
///         -> BoxFuture<'a, Result<Option<PasswordHash>>>
///     {
///         Box::pin(async { Ok(None) })
///     }
///
///     fn set_password_hash<'a>(&'a self, _id: &'a String, _hash: &'a PasswordHash)
///         -> BoxFuture<'a, Result<()>>
///     {
///         Box::pin(async { Ok(()) })
///     }
///
///     fn set_identity<'a>(&'a self, _id: &'a String, _identity: &'a str)
///         -> BoxFuture<'a, Result<()>>
///     {
///         Box::pin(async { Ok(()) })
///     }
///
///     fn mark_verified<'a>(&'a self, _id: &'a String) -> BoxFuture<'a, Result<()>> {
///         Box::pin(async { Ok(()) })
///     }
///
///     fn bump_epoch<'a>(&'a self, _id: &'a String) -> BoxFuture<'a, Result<()>> {
///         Box::pin(async { Ok(()) })
///     }
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not an account store",
    label = "not an account store",
    note = "an account store implements `find_by_identity`, `find_by_id`, `create`, \
            `password_hash`, `set_password_hash`, `set_identity`, `mark_verified` and `bump_epoch`",
    note = "help: it is eight small methods over your own `User` entity — the lifecycle flows own \
            the ordering, the tokens and the epoch, and your store owns the columns",
    note = "help: `bump_epoch` is what makes \"log out everywhere\" free: change whatever \
            `AuthUser::auth_hash` mixes in, and every session is invalid at the next request"
)]
pub trait AccountStore: Send + Sync + 'static {
    /// The principal this store holds.
    type User: AuthUser;

    /// Find an account by the identity a login presents — an address, usually.
    ///
    /// The identity arrives normalised: trimmed and lowercased.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn find_by_identity<'a>(
        &'a self,
        identity: &'a str,
    ) -> BoxFuture<'a, Result<Option<Self::User>>>;

    /// Find an account by its key.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn find_by_id<'a>(
        &'a self,
        id: &'a <Self::User as AuthUser>::Id,
    ) -> BoxFuture<'a, Result<Option<Self::User>>>;

    /// Create an account.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`]. A duplicate identity
    /// is the caller's problem to avoid — [`Accounts::register`] checks first —
    /// but a store that has a unique index should still report the conflict
    /// rather than overwrite.
    fn create<'a>(&'a self, account: &'a NewAccount) -> BoxFuture<'a, Result<Self::User>>;

    /// The stored password hash, when the account has one.
    ///
    /// `None` for an account that only ever signed in through a provider.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn password_hash<'a>(
        &'a self,
        id: &'a <Self::User as AuthUser>::Id,
    ) -> BoxFuture<'a, Result<Option<PasswordHash>>>;

    /// Replace the stored password hash.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn set_password_hash<'a>(
        &'a self,
        id: &'a <Self::User as AuthUser>::Id,
        hash: &'a PasswordHash,
    ) -> BoxFuture<'a, Result<()>>;

    /// Replace the identity — the address a login presents.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn set_identity<'a>(
        &'a self,
        id: &'a <Self::User as AuthUser>::Id,
        identity: &'a str,
    ) -> BoxFuture<'a, Result<()>>;

    /// Record that the account's address has been proved reachable.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn mark_verified<'a>(
        &'a self,
        id: &'a <Self::User as AuthUser>::Id,
    ) -> BoxFuture<'a, Result<()>>;

    /// Change whatever [`AuthUser::auth_hash`] mixes in, so every existing
    /// session becomes invalid.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn bump_epoch<'a>(&'a self, id: &'a <Self::User as AuthUser>::Id) -> BoxFuture<'a, Result<()>>;
}

/// How the lifecycle flows behave.
///
/// ```
/// use moso_auth::LifecycleConfig;
/// use std::time::Duration;
///
/// let config = LifecycleConfig::default();
/// assert_eq!(config.verification_ttl, Duration::from_secs(3600));
/// assert!(config.revoke_sessions_on_password_change);
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct LifecycleConfig {
    /// How long a verification token lives.
    pub verification_ttl: Duration,
    /// How long a password-reset token lives.
    ///
    /// Shorter than the others: a reset link is the strongest credential the
    /// application ever puts in an email.
    pub reset_ttl: Duration,
    /// How long a change-of-address confirmation lives.
    pub email_change_ttl: Duration,
    /// Whether a password change ends every other session.
    ///
    /// On. The `auth_hash` epoch already invalidates them at the next request;
    /// this deletes them too, so the "your devices" listing empties at once
    /// rather than looking like the attacker is still there.
    pub revoke_sessions_on_password_change: bool,
    /// Whether the new password may be the one being replaced.
    ///
    /// It may not. A "change" that changes nothing is a user who thinks they
    /// have responded to a breach and has not.
    pub refuse_password_reuse: bool,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            verification_ttl: crate::config::TOKEN_TTL,
            reset_ttl: Duration::from_secs(900),
            email_change_ttl: crate::config::TOKEN_TTL,
            revoke_sessions_on_password_change: true,
            refuse_password_reuse: true,
        }
    }
}

/// What registering did.
///
/// ```
/// use moso_auth::lifecycle::RegistrationOutcome;
///
/// assert!(RegistrationOutcome::Created.is_new());
/// assert!(!RegistrationOutcome::AlreadyRegistered.is_new());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegistrationOutcome {
    /// The account did not exist and now does.
    Created,
    /// The address was already registered. **This must not reach a client.**
    AlreadyRegistered,
}

impl RegistrationOutcome {
    /// Whether an account was created.
    ///
    /// ```
    /// use moso_auth::lifecycle::RegistrationOutcome;
    ///
    /// assert!(RegistrationOutcome::Created.is_new());
    /// ```
    #[must_use]
    pub const fn is_new(self) -> bool {
        matches!(self, Self::Created)
    }
}

/// The result of a registration.
///
/// # This type is for the server
///
/// [`outcome`](Registration::outcome) says whether the address was already
/// taken, and a response that varies with it is a user-enumeration oracle: an
/// attacker learns which addresses have accounts by trying to register them.
/// The route must answer identically either way and send the *appropriate*
/// email — "confirm your address" or "somebody tried to register with your
/// address" — which is what [`token`](Registration::token) carries.
///
/// ```no_run
/// # use moso_auth::{DefaultUser, Registration};
/// # fn f(registration: &Registration<DefaultUser>) {
/// let _ = (registration.outcome, registration.token.is_some());
/// # }
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub struct Registration<U> {
    /// What happened. **Server-side only.**
    pub outcome: RegistrationOutcome,
    /// The account, when one was created.
    pub user: Option<U>,
    /// The verification token to email, when there is one.
    pub token: Option<IssuedToken>,
}

/// The two tokens a change of address produces.
///
/// Double opt-in: the *new* address must confirm before anything changes, and
/// the *old* one is told that a change was requested, so a borrowed session
/// cannot move an account away silently.
///
/// ```no_run
/// # use moso_auth::EmailChange;
/// # fn f(change: &EmailChange) {
/// let _ = (&change.confirmation, &change.notify_previous);
/// # }
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub struct EmailChange {
    /// The token to send to the **new** address.
    pub confirmation: IssuedToken,
    /// The **old** address, which should be told that a change was requested.
    /// No token: there is nothing for it to confirm, only to notice.
    pub notify_previous: String,
}

/// The account lifecycle, over one [`AccountStore`].
///
/// ```no_run
/// use std::sync::Arc;
///
/// use moso_auth::lifecycle::KvLifecycleTokens;
/// use moso_auth::store::MemorySessionStore;
/// use moso_auth::{AccountStore, Accounts, SessionStore};
/// use moso_kv::Kv;
///
/// # fn f<S: AccountStore>(store: Arc<S>) {
/// let accounts = Accounts::new(
///     store,
///     KvLifecycleTokens::shared(Kv::in_memory("shop").unwrap()),
///     MemorySessionStore::shared() as Arc<dyn SessionStore>,
/// );
/// let _ = accounts;
/// # }
/// ```
pub struct Accounts<S: AccountStore> {
    /// Where the accounts are.
    store: Arc<S>,
    /// Where the single-use tokens are.
    tokens: Arc<dyn LifecycleTokens>,
    /// Where the sessions are, so a password change can end them.
    sessions: Arc<dyn SessionStore>,
    /// What a password must satisfy.
    policy: PasswordPolicy,
    /// How the flows behave.
    config: LifecycleConfig,
}

impl<S: AccountStore> Accounts<S> {
    /// The lifecycle over `store`, with every default.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_auth::lifecycle::KvLifecycleTokens;
    /// # use moso_auth::store::MemorySessionStore;
    /// # use moso_auth::{AccountStore, Accounts, SessionStore};
    /// # fn f<S: AccountStore>(s: Arc<S>, t: Arc<KvLifecycleTokens>, x: Arc<dyn SessionStore>) {
    /// let _ = Accounts::new(s, t, x);
    /// # }
    /// ```
    #[must_use]
    pub fn new(
        store: Arc<S>,
        tokens: Arc<dyn LifecycleTokens>,
        sessions: Arc<dyn SessionStore>,
    ) -> Self {
        Self {
            store,
            tokens,
            sessions,
            policy: PasswordPolicy::default(),
            config: LifecycleConfig::default(),
        }
    }

    /// Use this password policy instead of the default.
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts, PasswordPolicy};
    /// # fn f<S: AccountStore>(a: Accounts<S>) { let _ = a.policy(PasswordPolicy::default()); }
    /// ```
    #[must_use]
    pub fn policy(mut self, policy: PasswordPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Use this configuration instead of the default.
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts, LifecycleConfig};
    /// # fn f<S: AccountStore>(a: Accounts<S>) { let _ = a.config(LifecycleConfig::default()); }
    /// ```
    #[must_use]
    pub fn config(mut self, config: LifecycleConfig) -> Self {
        self.config = config;
        self
    }

    /// The store the flows act on.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_auth::{AccountStore, Accounts};
    /// # fn f<S: AccountStore>(a: &Accounts<S>) -> &Arc<S> { a.store() }
    /// ```
    #[must_use]
    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    // ── registration ──────────────────────────────────────────────────────

    /// Create an account and mint its verification token.
    ///
    /// The password is checked against the policy first, then hashed on the
    /// blocking pool, then the account is created. The hash happens **whether
    /// or not the address is taken**, so the two paths cost the same — a
    /// registration form that answers instantly for taken addresses is a
    /// membership oracle for every address an attacker cares about.
    ///
    /// # Errors
    ///
    /// [`Error::PasswordPolicy`] when the
    /// password is refused — which *is* specific, because the user has to fix
    /// it and cannot guess how — or
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts};
    /// # use moso_schema::Password;
    /// # async fn f<S: AccountStore>(a: &Accounts<S>, p: Password) -> moso_auth::Result<()> {
    /// let registration = a.register("Ada@Example.com", &p, serde_json::Value::Null).await?;
    /// // Respond identically whatever `registration.outcome` says.
    /// # let _ = registration;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn register(
        &self,
        identity: &str,
        password: &Password,
        profile: serde_json::Value,
    ) -> Result<Registration<S::User>> {
        let identity = normalise(identity);

        self.policy.check(password, &[&identity]).await?;

        // Always hash: the taken-address path must cost what the free one does.
        let hash = PasswordHash::new(password).await?;

        if let Some(existing) = self.store.find_by_identity(&identity).await? {
            // Still mint a token, so that the "somebody tried to register with
            // your address" mail can carry a "reset your password" link, and so
            // the two paths do the same amount of work.
            let subject = encode_subject(&existing.auth_id())?;
            let token = self
                .tokens
                .issue(
                    TokenPurpose::ResetPassword,
                    &subject,
                    &identity,
                    self.config.reset_ttl,
                )
                .await?;

            return Ok(Registration {
                outcome: RegistrationOutcome::AlreadyRegistered,
                user: None,
                token: Some(token),
            });
        }

        let account = NewAccount::new(identity.clone(), hash).profile(profile);
        let user = self.store.create(&account).await?;

        let subject = encode_subject(&user.auth_id())?;
        let token = self
            .tokens
            .issue(
                TokenPurpose::VerifyEmail,
                &subject,
                &identity,
                self.config.verification_ttl,
            )
            .await?;

        Ok(Registration {
            outcome: RegistrationOutcome::Created,
            user: Some(user),
            token: Some(token),
        })
    }

    /// Mint another verification token, if the address is one we know.
    ///
    /// `Ok(None)` for an address that is not registered. The route must answer
    /// identically either way.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts, IssuedToken};
    /// # async fn f<S: AccountStore>(a: &Accounts<S>) -> moso_auth::Result<Option<IssuedToken>> {
    /// a.resend_verification("ada@example.com").await
    /// # }
    /// ```
    pub async fn resend_verification(&self, identity: &str) -> Result<Option<IssuedToken>> {
        self.issue_for_identity(
            identity,
            TokenPurpose::VerifyEmail,
            self.config.verification_ttl,
        )
        .await
    }

    /// Redeem a verification token and mark the account verified.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidCredentials`] for a
    /// token that is unknown, expired, already used or of another purpose —
    /// one answer for all four, so a guess reveals nothing.
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts};
    /// # async fn f<S: AccountStore>(a: &Accounts<S>, t: &str) -> moso_auth::Result<()> {
    /// let _user = a.verify_email(t).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn verify_email(&self, token: &str) -> Result<S::User> {
        let claim = self
            .tokens
            .consume(TokenPurpose::VerifyEmail, token)
            .await?
            .ok_or(Error::InvalidCredentials)?;

        let id = crate::session::decode_subject::<<S::User as AuthUser>::Id>(&claim.subject)?;
        let user = self
            .store
            .find_by_id(&id)
            .await?
            .ok_or(Error::InvalidCredentials)?;

        self.store.mark_verified(&id).await?;
        Ok(user)
    }

    // ── password reset ────────────────────────────────────────────────────

    /// Mint a password-reset token, if the address is one we know.
    ///
    /// Every outstanding reset for the account is revoked first, so a reset an
    /// attacker triggered an hour ago stops working the moment the real user
    /// asks for one.
    ///
    /// `Ok(None)` for an address that is not registered. The route must answer
    /// 202 with the same body and the same timing either way: "if that address
    /// has an account, we have sent a link".
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts, IssuedToken};
    /// # async fn f<S: AccountStore>(a: &Accounts<S>) -> moso_auth::Result<Option<IssuedToken>> {
    /// a.request_password_reset("ada@example.com").await
    /// # }
    /// ```
    pub async fn request_password_reset(&self, identity: &str) -> Result<Option<IssuedToken>> {
        self.issue_for_identity(identity, TokenPurpose::ResetPassword, self.config.reset_ttl)
            .await
    }

    /// Redeem a reset token and set a new password.
    ///
    /// Bumps the epoch and deletes every session, so a password reset ends the
    /// attacker's session as well as the user's own.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidCredentials`] for a
    /// bad token, [`Error::PasswordPolicy`] for a
    /// refused password, or
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts};
    /// # use moso_schema::Password;
    /// # async fn f<S: AccountStore>(a: &Accounts<S>, t: &str, p: Password)
    /// #     -> moso_auth::Result<()> {
    /// let _user = a.reset_password(t, &p).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn reset_password(&self, token: &str, new_password: &Password) -> Result<S::User> {
        let claim = self
            .tokens
            .consume(TokenPurpose::ResetPassword, token)
            .await?
            .ok_or(Error::InvalidCredentials)?;

        let id = crate::session::decode_subject::<<S::User as AuthUser>::Id>(&claim.subject)?;
        let user = self
            .store
            .find_by_id(&id)
            .await?
            .ok_or(Error::InvalidCredentials)?;

        self.policy
            .check(new_password, &[claim.destination.as_str()])
            .await?;

        if self.config.refuse_password_reuse
            && let Some(current) = self.store.password_hash(&id).await?
            && current.verify(new_password).await?.is_valid()
        {
            return Err(Error::PasswordPolicy {
                code: "reused",
                detail: "this is the password you already had; choose a different one".into(),
            });
        }

        let hash = PasswordHash::new(new_password).await?;
        self.store.set_password_hash(&id, &hash).await?;
        self.store.bump_epoch(&id).await?;

        // Every session, including the one that asked: a reset is a recovery,
        // and a recovery that leaves the attacker signed in is not one.
        self.sessions.delete_for_user(&claim.subject, None).await?;

        Ok(user)
    }

    // ── password change ───────────────────────────────────────────────────

    /// Change a signed-in user's password, given the current one.
    ///
    /// The current password is required even inside a session: without it an
    /// unattended browser is a password change, and a password change is
    /// everything.
    ///
    /// `keep` is the session making the request, which survives — signing the
    /// user out of the browser they just used to change their password is a
    /// support ticket, not a security control. Pass `None` to end that one too.
    ///
    /// Returns how many sessions were revoked.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidCredentials`] when the
    /// current password is wrong,
    /// [`Error::PasswordPolicy`] when the new one
    /// is refused, or [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts, AuthUser};
    /// # use moso_schema::Password;
    /// # async fn f<S: AccountStore>(a: &Accounts<S>, u: &S::User, old: Password, new: Password)
    /// #     -> moso_auth::Result<u64> {
    /// a.change_password(&u.auth_id(), &old, &new, None).await
    /// # }
    /// ```
    pub async fn change_password(
        &self,
        id: &<S::User as AuthUser>::Id,
        current: &Password,
        new_password: &Password,
        keep: Option<&SessionId>,
    ) -> Result<u64> {
        let stored = self.store.password_hash(id).await?;

        // The absent-hash path still costs a verification, so "this account has
        // no password" is not readable from the clock.
        let ok = match stored.as_ref() {
            Some(hash) => hash.verify(current).await?.is_valid(),
            None => {
                crate::password::dummy_verify().await?;
                false
            }
        };

        if !ok {
            return Err(Error::InvalidCredentials);
        }

        self.policy.check(new_password, &[]).await?;

        if self.config.refuse_password_reuse
            && let Some(hash) = stored.as_ref()
            && hash.verify(new_password).await?.is_valid()
        {
            return Err(Error::PasswordPolicy {
                code: "reused",
                detail: "this is the password you already had; choose a different one".into(),
            });
        }

        let hash = PasswordHash::new(new_password).await?;
        self.store.set_password_hash(id, &hash).await?;
        self.store.bump_epoch(id).await?;

        if !self.config.revoke_sessions_on_password_change {
            return Ok(0);
        }

        let subject = encode_subject(id)?;
        self.sessions.delete_for_user(&subject, keep).await
    }

    // ── address change ────────────────────────────────────────────────────

    /// Begin a change of address: mint a token for the **new** one.
    ///
    /// Nothing changes until [`confirm_email_change`](Accounts::confirm_email_change)
    /// redeems it. Double opt-in is what stops a typo from locking an account
    /// away and stops a borrowed session from moving an account silently.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`] when the new address is
    /// already registered — which is safe to say here, because the caller is
    /// already authenticated as somebody and learns nothing they could not
    /// learn by trying to register. [`Error::Unavailable`]
    /// otherwise.
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts, AuthUser, EmailChange};
    /// # async fn f<S: AccountStore>(a: &Accounts<S>, u: &S::User)
    /// #     -> moso_auth::Result<EmailChange> {
    /// a.request_email_change(&u.auth_id(), "ada@example.com", "new@example.com").await
    /// # }
    /// ```
    pub async fn request_email_change(
        &self,
        id: &<S::User as AuthUser>::Id,
        current_identity: &str,
        new_identity: &str,
    ) -> Result<EmailChange> {
        let new_identity = normalise(new_identity);

        if new_identity == normalise(current_identity) {
            return Err(ceremony_failed(
                "the new address is the one already on the account",
            ));
        }

        if self.store.find_by_identity(&new_identity).await?.is_some() {
            return Err(ceremony_failed("that address is already in use"));
        }

        let subject = encode_subject(id)?;
        self.tokens
            .revoke_all(TokenPurpose::ChangeEmail, &subject)
            .await?;

        let confirmation = self
            .tokens
            .issue(
                TokenPurpose::ChangeEmail,
                &subject,
                &new_identity,
                self.config.email_change_ttl,
            )
            .await?;

        Ok(EmailChange {
            confirmation,
            notify_previous: normalise(current_identity),
        })
    }

    /// Redeem a change-of-address token.
    ///
    /// The address is checked again at redemption: an hour passed, and it may
    /// have been registered in between.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidCredentials`] for a
    /// bad token, [`Error::Ceremony`] when the address
    /// was taken in the meantime, or
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts};
    /// # async fn f<S: AccountStore>(a: &Accounts<S>, t: &str) -> moso_auth::Result<()> {
    /// let _user = a.confirm_email_change(t).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn confirm_email_change(&self, token: &str) -> Result<S::User> {
        let claim = self
            .tokens
            .consume(TokenPurpose::ChangeEmail, token)
            .await?
            .ok_or(Error::InvalidCredentials)?;

        let id = crate::session::decode_subject::<<S::User as AuthUser>::Id>(&claim.subject)?;
        let user = self
            .store
            .find_by_id(&id)
            .await?
            .ok_or(Error::InvalidCredentials)?;

        if self
            .store
            .find_by_identity(&claim.destination)
            .await?
            .is_some()
        {
            return Err(ceremony_failed(
                "that address was registered while the confirmation was outstanding",
            ));
        }

        self.store.set_identity(&id, &claim.destination).await?;
        // The address is proved reachable by the fact that this token arrived.
        self.store.mark_verified(&id).await?;
        Ok(user)
    }

    // ── logging out ───────────────────────────────────────────────────────

    /// End one session.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts, Session};
    /// # async fn f<S: AccountStore>(a: &Accounts<S>, s: &Session) -> moso_auth::Result<()> {
    /// a.log_out(s).await
    /// # }
    /// ```
    pub async fn log_out(&self, session: &Session) -> Result<()> {
        session.destroy().await
    }

    /// End every session a user has, optionally keeping one.
    ///
    /// Two things happen, and both matter. The epoch is bumped, which makes
    /// every session invalid at its next request through `auth_hash` — no scan,
    /// no fan-out, and it covers sessions in a store this process cannot reach.
    /// Then the sessions are deleted, so the listing empties now rather than
    /// when each device next calls.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts, AuthUser};
    /// # async fn f<S: AccountStore>(a: &Accounts<S>, u: &S::User) -> moso_auth::Result<u64> {
    /// a.log_out_everywhere(&u.auth_id(), None).await
    /// # }
    /// ```
    pub async fn log_out_everywhere(
        &self,
        id: &<S::User as AuthUser>::Id,
        keep: Option<&SessionId>,
    ) -> Result<u64> {
        self.store.bump_epoch(id).await?;
        let subject = encode_subject(id)?;
        self.sessions.delete_for_user(&subject, keep).await
    }

    /// Every live session a user has, for the "your devices" listing.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::{AccountStore, Accounts, AuthUser, SessionRecord};
    /// # async fn f<S: AccountStore>(a: &Accounts<S>, u: &S::User)
    /// #     -> moso_auth::Result<Vec<SessionRecord>> {
    /// a.sessions_of(&u.auth_id()).await
    /// # }
    /// ```
    pub async fn sessions_of(
        &self,
        id: &<S::User as AuthUser>::Id,
    ) -> Result<Vec<crate::SessionRecord>> {
        let subject = encode_subject(id)?;
        self.sessions.list_for_user(&subject).await
    }

    /// Mint a token for whoever holds `identity`, doing the same work either
    /// way.
    async fn issue_for_identity(
        &self,
        identity: &str,
        purpose: TokenPurpose,
        ttl: Duration,
    ) -> Result<Option<IssuedToken>> {
        let identity = normalise(identity);
        let found = self.store.find_by_identity(&identity).await?;

        // Whether or not the account exists, a token is minted and stored. The
        // one for nobody is issued against a subject no account can have and is
        // dropped on return, so the miss path costs the same round trips as the
        // hit path — which is the whole of the enumeration defence here.
        let subject = match found.as_ref() {
            Some(user) => encode_subject(&user.auth_id())?,
            None => format!("\u{0}absent:{identity}"),
        };

        if found.is_some() {
            self.tokens.revoke_all(purpose, &subject).await?;
        }

        let token = self.tokens.issue(purpose, &subject, &identity, ttl).await?;

        if found.is_none() {
            // Burn it immediately: nothing may redeem a token for nobody.
            self.tokens.revoke_all(purpose, &subject).await?;
            return Ok(None);
        }

        Ok(Some(token))
    }
}

impl<S: AccountStore> core::fmt::Debug for Accounts<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Accounts")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// A change-of-address ceremony that did not check out.
///
/// Constructed rather than routed through `Error::ceremony` so that this module
/// does not depend on a constructor in `error.rs`.
fn ceremony_failed(reason: &'static str) -> Error {
    Error::Ceremony {
        ceremony: "email_change",
        reason: reason.into(),
    }
}

/// Normalise an identity: trimmed and lowercased.
///
/// `Ada@Example.com` and `ada@example.com` are the same mailbox, and a user who
/// cannot sign in because of a capital letter files a support ticket. Applied
/// on every path that looks an account up or creates one, so the two can never
/// disagree.
///
/// ```
/// use moso_auth::lifecycle::normalise;
///
/// assert_eq!(normalise("  Ada@Example.COM "), "ada@example.com");
/// ```
#[must_use]
pub fn normalise(identity: &str) -> String {
    identity.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::store::MemorySessionStore;
    use crate::{DefaultUser, SessionConfig};

    /// One row of the test account store.
    #[derive(Clone, Debug)]
    struct Row {
        /// The address the account signs in with.
        identity: String,
        /// Its PHC hash, when it has one.
        hash: Option<String>,
        /// How many times the epoch has been bumped.
        epoch: u32,
        /// Whether the address has been proved reachable.
        verified: bool,
    }

    /// An account store in a map, with the same semantics a table would have.
    #[derive(Default)]
    struct Accounts0 {
        /// The rows, by identifier.
        rows: Mutex<HashMap<String, Row>>,
        /// The next identifier.
        next: std::sync::atomic::AtomicU64,
    }

    impl Accounts0 {
        fn rows(&self) -> std::sync::MutexGuard<'_, HashMap<String, Row>> {
            self.rows.lock().unwrap_or_else(|p| p.into_inner())
        }

        fn user(id: &str, row: &Row) -> DefaultUser {
            let mut material = row.hash.clone().unwrap_or_default().into_bytes();
            material.extend_from_slice(&row.epoch.to_le_bytes());
            DefaultUser::new(id, material)
        }

        fn row_of(&self, id: &str) -> Option<Row> {
            self.rows().get(id).cloned()
        }
    }

    impl AccountStore for Accounts0 {
        type User = DefaultUser;

        fn find_by_identity<'a>(
            &'a self,
            identity: &'a str,
        ) -> BoxFuture<'a, Result<Option<DefaultUser>>> {
            Box::pin(async move {
                Ok(self
                    .rows()
                    .iter()
                    .find(|(_, row)| row.identity == identity)
                    .map(|(id, row)| Self::user(id, row)))
            })
        }

        fn find_by_id<'a>(&'a self, id: &'a String) -> BoxFuture<'a, Result<Option<DefaultUser>>> {
            Box::pin(async move { Ok(self.row_of(id).map(|row| Self::user(id, &row))) })
        }

        fn create<'a>(&'a self, account: &'a NewAccount) -> BoxFuture<'a, Result<DefaultUser>> {
            Box::pin(async move {
                let id = format!(
                    "usr_{}",
                    self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                );
                let row = Row {
                    identity: account.identity().to_owned(),
                    hash: Some(account.password_hash().as_str().to_owned()),
                    epoch: 0,
                    verified: false,
                };
                self.rows().insert(id.clone(), row.clone());
                Ok(Self::user(&id, &row))
            })
        }

        fn password_hash<'a>(
            &'a self,
            id: &'a String,
        ) -> BoxFuture<'a, Result<Option<PasswordHash>>> {
            Box::pin(async move {
                self.row_of(id)
                    .and_then(|row| row.hash)
                    .map(|phc| PasswordHash::parse(&phc))
                    .transpose()
            })
        }

        fn set_password_hash<'a>(
            &'a self,
            id: &'a String,
            hash: &'a PasswordHash,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if let Some(row) = self.rows().get_mut(id) {
                    row.hash = Some(hash.as_str().to_owned());
                }
                Ok(())
            })
        }

        fn set_identity<'a>(
            &'a self,
            id: &'a String,
            identity: &'a str,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if let Some(row) = self.rows().get_mut(id) {
                    row.identity = identity.to_owned();
                }
                Ok(())
            })
        }

        fn mark_verified<'a>(&'a self, id: &'a String) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if let Some(row) = self.rows().get_mut(id) {
                    row.verified = true;
                }
                Ok(())
            })
        }

        fn bump_epoch<'a>(&'a self, id: &'a String) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if let Some(row) = self.rows().get_mut(id) {
                    row.epoch += 1;
                }
                Ok(())
            })
        }
    }

    /// The three pieces every test here wants.
    fn fixture() -> (Arc<Accounts0>, Arc<MemorySessionStore>, Accounts<Accounts0>) {
        let store = Arc::new(Accounts0::default());
        let sessions = MemorySessionStore::shared();
        let accounts = Accounts::new(
            Arc::clone(&store),
            KvLifecycleTokens::shared(moso_kv::Kv::in_memory("lifecycle").unwrap()),
            Arc::clone(&sessions) as Arc<dyn SessionStore>,
        );
        (store, sessions, accounts)
    }

    /// A password strong enough for the policy and unique per test.
    fn password(seed: &str) -> Password {
        Password::new(format!("wharf-lentil-oxide-{seed}")).unwrap()
    }

    #[tokio::test]
    async fn registering_creates_an_account_and_a_verification_token() {
        let (store, _, accounts) = fixture();

        let registration = accounts
            .register("Ada@Example.com", &password("aa"), serde_json::Value::Null)
            .await
            .unwrap();

        assert_eq!(registration.outcome, RegistrationOutcome::Created);
        let user = registration.user.unwrap();
        let token = registration.token.unwrap();

        assert_eq!(token.purpose, TokenPurpose::VerifyEmail);
        assert_eq!(
            token.destination, "ada@example.com",
            "the address is normalised before it is stored or emailed"
        );

        let row = store.row_of(&user.auth_id()).unwrap();
        assert_eq!(row.identity, "ada@example.com");
        assert!(!row.verified);
    }

    #[tokio::test]
    async fn registering_a_taken_address_looks_exactly_like_registering_a_free_one() {
        let (_, _, accounts) = fixture();

        accounts
            .register("ada@example.com", &password("ab"), serde_json::Value::Null)
            .await
            .unwrap();

        let second = accounts
            .register("ada@example.com", &password("ac"), serde_json::Value::Null)
            .await
            .unwrap();

        assert_eq!(second.outcome, RegistrationOutcome::AlreadyRegistered);
        assert!(second.user.is_none(), "no second account was created");
        assert!(
            second.token.is_some(),
            "the taken path still produces something to send, so the two do the same work"
        );
    }

    #[tokio::test]
    async fn a_refused_password_never_creates_an_account() {
        let (store, _, accounts) = fixture();

        let error = accounts
            .register(
                "ada@example.com",
                &Password::new("password1234").unwrap(),
                serde_json::Value::Null,
            )
            .await
            .unwrap_err();

        assert!(matches!(error, Error::PasswordPolicy { .. }));
        assert!(store.rows().is_empty());
    }

    #[tokio::test]
    async fn verifying_marks_the_account_and_burns_the_token() {
        let (store, _, accounts) = fixture();

        let registration = accounts
            .register("ada@example.com", &password("ad"), serde_json::Value::Null)
            .await
            .unwrap();
        let token = registration.token.unwrap();

        let user = accounts.verify_email(token.expose()).await.unwrap();
        assert!(store.row_of(&user.auth_id()).unwrap().verified);

        assert!(
            matches!(
                accounts.verify_email(token.expose()).await,
                Err(Error::InvalidCredentials)
            ),
            "a verification token must work exactly once"
        );
    }

    #[tokio::test]
    async fn a_token_of_the_wrong_purpose_is_refused() {
        let (_, _, accounts) = fixture();

        let registration = accounts
            .register("ada@example.com", &password("ae"), serde_json::Value::Null)
            .await
            .unwrap();
        let verification = registration.token.unwrap();

        assert!(
            matches!(
                accounts
                    .reset_password(verification.expose(), &password("af"))
                    .await,
                Err(Error::InvalidCredentials)
            ),
            "a \"confirm your address\" link must not also reset a password"
        );
    }

    #[tokio::test]
    async fn asking_to_reset_an_unknown_address_is_indistinguishable_from_a_known_one() {
        let (_, _, accounts) = fixture();

        accounts
            .register("ada@example.com", &password("ag"), serde_json::Value::Null)
            .await
            .unwrap();

        assert!(
            accounts
                .request_password_reset("ada@example.com")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            accounts
                .request_password_reset("nobody@example.com")
                .await
                .unwrap()
                .is_none(),
            "the caller gets nothing to send, and must answer 202 anyway"
        );
    }

    #[tokio::test]
    async fn a_new_reset_request_revokes_the_previous_one() {
        let (_, _, accounts) = fixture();
        accounts
            .register("ada@example.com", &password("ah"), serde_json::Value::Null)
            .await
            .unwrap();

        let first = accounts
            .request_password_reset("ada@example.com")
            .await
            .unwrap()
            .unwrap();
        let second = accounts
            .request_password_reset("ada@example.com")
            .await
            .unwrap()
            .unwrap();

        assert!(
            matches!(
                accounts
                    .reset_password(first.expose(), &password("ai"))
                    .await,
                Err(Error::InvalidCredentials)
            ),
            "the reset an attacker triggered must stop working"
        );
        accounts
            .reset_password(second.expose(), &password("aj"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn resetting_a_password_ends_every_session_including_the_attackers() {
        let (store, sessions, accounts) = fixture();

        let user = accounts
            .register("ada@example.com", &password("ak"), serde_json::Value::Null)
            .await
            .unwrap()
            .user
            .unwrap();

        // Two devices, one of them the attacker's.
        for _ in 0..2 {
            let session = Session::detached(
                Arc::clone(&sessions) as Arc<dyn SessionStore>,
                SessionConfig::default(),
            );
            session.log_in(&user).await.unwrap();
            session.save().await.unwrap();
        }
        assert_eq!(
            sessions.list_for_user(&user.auth_id()).await.unwrap().len(),
            2
        );

        let token = accounts
            .request_password_reset("ada@example.com")
            .await
            .unwrap()
            .unwrap();
        accounts
            .reset_password(token.expose(), &password("al"))
            .await
            .unwrap();

        assert!(
            sessions
                .list_for_user(&user.auth_id())
                .await
                .unwrap()
                .is_empty(),
            "a recovery that leaves the attacker signed in is not one"
        );

        let after = store.row_of(&user.auth_id()).unwrap();
        assert_eq!(
            after.epoch, 1,
            "and the epoch invalidates anything we missed"
        );
    }

    #[tokio::test]
    async fn resetting_to_the_same_password_is_refused() {
        let (_, _, accounts) = fixture();
        let chosen = password("am");

        accounts
            .register("ada@example.com", &chosen, serde_json::Value::Null)
            .await
            .unwrap();

        let token = accounts
            .request_password_reset("ada@example.com")
            .await
            .unwrap()
            .unwrap();

        let error = accounts
            .reset_password(token.expose(), &chosen)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::PasswordPolicy { code: "reused", .. }
        ));
    }

    #[tokio::test]
    async fn changing_a_password_needs_the_current_one() {
        let (_, _, accounts) = fixture();
        let old = password("an");

        let user = accounts
            .register("ada@example.com", &old, serde_json::Value::Null)
            .await
            .unwrap()
            .user
            .unwrap();

        let wrong = password("ao");
        assert!(matches!(
            accounts
                .change_password(&user.auth_id(), &wrong, &password("ap"), None)
                .await,
            Err(Error::InvalidCredentials)
        ));

        accounts
            .change_password(&user.auth_id(), &old, &password("aq"), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn changing_a_password_keeps_the_current_session_and_ends_the_others() {
        let (store, sessions, accounts) = fixture();
        let old = password("ar");

        let user = accounts
            .register("ada@example.com", &old, serde_json::Value::Null)
            .await
            .unwrap()
            .user
            .unwrap();

        let mut ids = Vec::new();
        for _ in 0..3 {
            let session = Session::detached(
                Arc::clone(&sessions) as Arc<dyn SessionStore>,
                SessionConfig::default(),
            );
            session.log_in(&user).await.unwrap();
            session.save().await.unwrap();
            ids.push(session.id());
        }

        let keep = ids[0].clone();
        let revoked = accounts
            .change_password(&user.auth_id(), &old, &password("as"), Some(&keep))
            .await
            .unwrap();

        assert_eq!(revoked, 2);
        let remaining = sessions.list_for_user(&user.auth_id()).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, keep);

        assert_eq!(store.row_of(&user.auth_id()).unwrap().epoch, 1);
    }

    /// The `auth_hash` epoch is what makes "log out everywhere" free: after a
    /// bump, the hash the sessions carry no longer matches the user's.
    #[tokio::test]
    async fn the_epoch_changes_the_auth_hash_so_every_session_is_stale() {
        let (store, sessions, accounts) = fixture();

        let user = accounts
            .register("ada@example.com", &password("at"), serde_json::Value::Null)
            .await
            .unwrap()
            .user
            .unwrap();

        let session = Session::detached(
            Arc::clone(&sessions) as Arc<dyn SessionStore>,
            SessionConfig::default(),
        );
        session.log_in(&user).await.unwrap();
        session.save().await.unwrap();
        let recorded = session.auth_hash().unwrap();

        accounts
            .log_out_everywhere(&user.auth_id(), None)
            .await
            .unwrap();

        let after = store.find_by_id(&user.auth_id()).await.unwrap().unwrap();

        assert_ne!(
            after.auth_hash(),
            recorded,
            "a session comparing its recorded hash against this one must now drop"
        );
        assert!(
            sessions
                .list_for_user(&user.auth_id())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn logging_out_everywhere_can_keep_the_current_device() {
        let (_, sessions, accounts) = fixture();

        let user = accounts
            .register("ada@example.com", &password("au"), serde_json::Value::Null)
            .await
            .unwrap()
            .user
            .unwrap();

        let mut ids = Vec::new();
        for _ in 0..3 {
            let session = Session::detached(
                Arc::clone(&sessions) as Arc<dyn SessionStore>,
                SessionConfig::default(),
            );
            session.log_in(&user).await.unwrap();
            session.save().await.unwrap();
            ids.push(session.id());
        }

        let keep = ids[2].clone();
        assert_eq!(
            accounts
                .log_out_everywhere(&user.auth_id(), Some(&keep))
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            accounts.sessions_of(&user.auth_id()).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn logging_out_destroys_only_that_session() {
        let (_, sessions, accounts) = fixture();

        let user = accounts
            .register("ada@example.com", &password("av"), serde_json::Value::Null)
            .await
            .unwrap()
            .user
            .unwrap();

        let first = Session::detached(
            Arc::clone(&sessions) as Arc<dyn SessionStore>,
            SessionConfig::default(),
        );
        first.log_in(&user).await.unwrap();
        first.save().await.unwrap();

        let second = Session::detached(
            Arc::clone(&sessions) as Arc<dyn SessionStore>,
            SessionConfig::default(),
        );
        second.log_in(&user).await.unwrap();
        second.save().await.unwrap();

        accounts.log_out(&first).await.unwrap();

        let remaining = accounts.sessions_of(&user.auth_id()).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, second.id());
    }

    #[tokio::test]
    async fn changing_an_address_is_double_opt_in() {
        let (store, _, accounts) = fixture();

        let user = accounts
            .register("ada@example.com", &password("aw"), serde_json::Value::Null)
            .await
            .unwrap()
            .user
            .unwrap();

        let change = accounts
            .request_email_change(&user.auth_id(), "ada@example.com", "New@Example.com")
            .await
            .unwrap();

        assert_eq!(change.confirmation.destination, "new@example.com");
        assert_eq!(change.notify_previous, "ada@example.com");
        assert_eq!(
            store.row_of(&user.auth_id()).unwrap().identity,
            "ada@example.com",
            "nothing changes until the new address confirms"
        );

        accounts
            .confirm_email_change(change.confirmation.expose())
            .await
            .unwrap();

        let row = store.row_of(&user.auth_id()).unwrap();
        assert_eq!(row.identity, "new@example.com");
        assert!(
            row.verified,
            "arriving at the new address proves it reachable"
        );
    }

    #[tokio::test]
    async fn an_address_change_to_a_taken_address_is_refused_twice() {
        let (_, _, accounts) = fixture();

        let first = accounts
            .register("ada@example.com", &password("ax"), serde_json::Value::Null)
            .await
            .unwrap()
            .user
            .unwrap();
        accounts
            .register("bob@example.com", &password("ay"), serde_json::Value::Null)
            .await
            .unwrap();

        // Refused up front...
        assert!(matches!(
            accounts
                .request_email_change(&first.auth_id(), "ada@example.com", "bob@example.com")
                .await,
            Err(Error::Ceremony { .. })
        ));

        // ...and again at redemption, for an address taken in the meantime.
        let change = accounts
            .request_email_change(&first.auth_id(), "ada@example.com", "carol@example.com")
            .await
            .unwrap();
        accounts
            .register(
                "carol@example.com",
                &password("az"),
                serde_json::Value::Null,
            )
            .await
            .unwrap();

        assert!(matches!(
            accounts
                .confirm_email_change(change.confirmation.expose())
                .await,
            Err(Error::Ceremony { .. })
        ));
    }

    #[tokio::test]
    async fn changing_an_address_to_the_current_one_is_refused() {
        let (_, _, accounts) = fixture();
        let user = accounts
            .register("ada@example.com", &password("ba"), serde_json::Value::Null)
            .await
            .unwrap()
            .user
            .unwrap();

        assert!(matches!(
            accounts
                .request_email_change(&user.auth_id(), "ada@example.com", "  Ada@Example.com ")
                .await,
            Err(Error::Ceremony { .. })
        ));
    }

    #[tokio::test]
    async fn a_token_store_keeps_only_a_digest() {
        let kv = moso_kv::Kv::in_memory("digest-check").unwrap();
        let tokens = KvLifecycleTokens::new(kv);

        let issued = tokens
            .issue(
                TokenPurpose::ResetPassword,
                "usr_1",
                "ada@example.com",
                Duration::from_secs(60),
            )
            .await
            .unwrap();

        let stored_under = KvLifecycleTokens::key_for(TokenPurpose::ResetPassword, issued.expose());
        assert!(
            !stored_under.contains(issued.expose()),
            "the plaintext token must not be the key"
        );
        assert!(
            tokens
                .kv
                .get::<LifecycleToken>(&stored_under)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_expired_token_is_refused() {
        let tokens = KvLifecycleTokens::new(moso_kv::Kv::in_memory("expiry").unwrap());

        let issued = tokens
            .issue(
                TokenPurpose::VerifyEmail,
                "usr_1",
                "ada@example.com",
                Duration::from_millis(20),
            )
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            tokens
                .consume(TokenPurpose::VerifyEmail, issued.expose())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn revoking_burns_every_outstanding_token_for_a_subject() {
        let tokens = KvLifecycleTokens::new(moso_kv::Kv::in_memory("revoke-all").unwrap());

        let mut issued = Vec::new();
        for _ in 0..3 {
            issued.push(
                tokens
                    .issue(
                        TokenPurpose::ResetPassword,
                        "usr_1",
                        "ada@example.com",
                        Duration::from_secs(60),
                    )
                    .await
                    .unwrap(),
            );
        }

        assert_eq!(
            tokens
                .revoke_all(TokenPurpose::ResetPassword, "usr_1")
                .await
                .unwrap(),
            3
        );

        for token in &issued {
            assert!(
                tokens
                    .consume(TokenPurpose::ResetPassword, token.expose())
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn a_token_never_prints_itself() {
        let token = IssuedToken {
            token: "the-secret".to_owned(),
            purpose: TokenPurpose::ResetPassword,
            subject: "usr_1".to_owned(),
            destination: "ada@example.com".to_owned(),
            expires_at: Utc::now(),
        };

        let printed = format!("{token:?}");
        assert!(!printed.contains("the-secret"), "{printed}");
        assert!(printed.contains("<redacted>"));
    }

    #[test]
    fn identities_are_normalised_the_same_way_everywhere() {
        assert_eq!(normalise("  Ada@Example.COM "), "ada@example.com");
        assert_eq!(normalise("ada@example.com"), "ada@example.com");
    }

    /// Acceptance criterion 3, at the flow level: the two enumeration-sensitive
    /// paths must not be told apart by a stopwatch.
    #[tokio::test]
    async fn a_reset_for_a_known_and_an_unknown_address_take_the_same_time() {
        use std::time::Instant;

        let (_, _, accounts) = fixture();
        accounts
            .register("ada@example.com", &password("bb"), serde_json::Value::Null)
            .await
            .unwrap();

        let mut known = Vec::new();
        let mut unknown = Vec::new();

        for index in 0..40 {
            let started = Instant::now();
            accounts
                .request_password_reset("ada@example.com")
                .await
                .unwrap();
            known.push(started.elapsed());

            let started = Instant::now();
            accounts
                .request_password_reset(&format!("nobody{index}@example.com"))
                .await
                .unwrap();
            unknown.push(started.elapsed());
        }

        known.sort_unstable();
        unknown.sort_unstable();
        let p95 = |samples: &[Duration]| samples[samples.len() * 95 / 100];

        let difference = p95(&known).abs_diff(p95(&unknown));
        assert!(
            difference < Duration::from_millis(10),
            "p95 differed by {difference:?} between a known and an unknown address; \
             known {:?}, unknown {:?}",
            p95(&known),
            p95(&unknown)
        );
    }
}
