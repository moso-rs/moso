//! Sign up, sign in, sessions and password reset — your copy, not the
//! framework's.
//!
//! `moso new --auth` copied these handlers out of `moso-auth` and into your
//! project. That is the second of the battery's two tiers: the first,
//! `moso::auth::routes()`, mounts a fixed set of flows over the framework's own
//! `DefaultUser` and exists for prototyping. This file is the one you keep.
//! Every flow below runs over [`User`], a type declared *here*, so adding a
//! field to your account is an edit rather than a feature request.
//!
//! # Why the copy is documented and the mounted set is not
//!
//! Every handler below carries `#[endpoint]`, so its request body, its response
//! body and its status codes are in **your** OpenAPI document: `moso openapi
//! export` publishes them and `moso client` generates a typed client for them.
//! The mounted set cannot do that. `moso-auth` sits below the facade in the
//! dependency graph, and a macro expansion may only name `::moso::__private::…`,
//! so `#[endpoint]` is unavailable to it and its operations are registered as
//! undocumented. That one difference is the best reason this tier exists.
//!
//! # What is real here, and what you replace before you deploy
//!
//! | Real, and production-shaped | Yours to replace |
//! | --- | --- |
//! | argon2id hashing, on the bounded blocking pool | [`Users`] — a `HashMap` in this process |
//! | The signed, `HttpOnly`, `SameSite=Lax` session cookie | [`Outbox`] — it sends no email |
//! | Single-use lifecycle tokens, stored as digests | the in-memory `Kv` holding them |
//! | The enumeration and timing defences below | |
//!
//! [`Users`] is eight small methods over one map. Point them at your database —
//! `moso new --with-db` gives you `migrations/` and `moso db` to do it with —
//! and nothing else in this file changes: the flows own the ordering, the
//! tokens and the epoch, and your store owns the columns.
//!
//! # The three rules the handlers are shaped by
//!
//! **A response must not say whether an address has an account.**
//! `/auth/register` and `/auth/password/forgot` answer `202` with one constant
//! sentence and do the same work either way, because
//! [`Accounts`](moso::auth::Accounts) hashes on the taken-address path and mints
//! a token it then drops on the unknown-address path. `/auth/login` answers the
//! same `401` for "no such account", "wrong password" and "suspended", and pays
//! for a password verification on all three.
//!
//! **The session cookie is written by the layer, never by a handler.**
//! [`Session::log_in`](moso::auth::Session::log_in) cycles the identifier, which
//! is the fixation defence, and `SessionLayer` turns the changed session into
//! the `Set-Cookie` on the way out.
//!
//! **A session listing is not a list of credentials.** A session identifier
//! *is* the credential, so [`SessionOut`] carries no identifier at all — only
//! what a person needs to recognise their own devices, plus which row is this
//! browser.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use moso::BoxFuture;
use moso::auth::lifecycle::KvLifecycleTokens;
use moso::auth::{
    AccountStore, Accounts, AuthSession, AuthUser, Error as AuthError, HashParams, IssuedToken,
    MemorySessionStore, NewAccount, PasswordHash, Session, SessionConfig, SessionLayer,
    SessionRecord, SessionStore, TokenPurpose,
};
use moso::config::Profile;
use moso::deps::serde_json::{Value, json};
use moso::openapi::SecurityRequirement;
use moso::prelude::*;
use moso::response::Accepted;
use moso::schema::Password;
use moso_kv::Kv;

// ---------------------------------------------------------------------------
// Your account
// ---------------------------------------------------------------------------

/// The principal this application authenticates.
///
/// Yours. Add the fields your product needs — a locale, a plan, a tenant — and
/// they travel through every flow below without touching the framework.
///
/// It is deliberately not the row: the password hash lives in [`Users`] beside
/// this, never on the type a handler can return. `Entity` and `Schema` are
/// separate for the same reason, and it is what makes leaking a hash a compile
/// error rather than something a reviewer has to notice.
#[derive(Clone, Debug)]
pub struct User {
    /// The account key. Also what the session record holds.
    pub id: String,
    /// The address that signs in, normalised to lower case.
    pub email: String,
    /// What to call them.
    pub name: String,
    /// Whether the address has been proved reachable.
    pub verified: bool,
    /// Whether the account may sign in at all.
    pub active: bool,
    /// Bumped whenever the credentials change. See [`User::auth_hash`].
    epoch: u64,
}

impl AuthUser for User {
    type Id = String;

    fn auth_id(&self) -> String {
        self.id.clone()
    }

    /// What makes "log out everywhere" free.
    ///
    /// The value is written onto every session at login and compared on every
    /// load; a mismatch drops the session. Because [`Users::set_password_hash`]
    /// and [`Users::bump_epoch`] both move the epoch, a password reset
    /// invalidates every session that exists — with no scan, no fan-out and no
    /// index — including sessions in a store this process cannot reach.
    fn auth_hash(&self) -> Vec<u8> {
        self.epoch.to_be_bytes().to_vec()
    }

    fn is_active(&self) -> bool {
        self.active
    }
}

// ---------------------------------------------------------------------------
// Where the accounts are
// ---------------------------------------------------------------------------

/// One stored account: the user, and the credential that is never on it.
#[derive(Clone, Debug)]
struct Row {
    /// The account, as a handler may see it.
    user: User,
    /// The argon2id hash. `None` for an account that only ever signed in
    /// through a provider.
    password_hash: Option<PasswordHash>,
}

/// The accounts, in this process's memory.
///
/// A real store with real semantics and no durability: restart the binary and
/// every account is gone. It is here so that `cargo run` and `cargo test` work
/// the moment the project is created, and it is the **one** thing in this file
/// that is not production-shaped.
///
/// Replacing it is eight methods and no change anywhere else. Against a
/// database, `find_by_identity` becomes a `WHERE lower(email) = $1` over a
/// unique index — two exact comparisons, never `LIKE`, because `_` is a `LIKE`
/// wildcard and one account's password must never sign in as another's.
#[derive(Debug, Default)]
pub struct Users {
    /// Keyed by [`User::id`].
    rows: RwLock<HashMap<String, Row>>,
    /// The next account number. Your database has a sequence.
    next: AtomicU64,
}

impl Users {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `edit` against one row, if it is there.
    ///
    /// A poisoned lock is recovered from rather than propagated: a panic in one
    /// request must not turn every later login into a 500.
    fn edit_row(&self, id: &str, edit: impl FnOnce(&mut Row)) {
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(row) = rows.get_mut(id) {
            edit(row);
        }
    }
}

impl AccountStore for Users {
    type User = User;

    fn find_by_identity<'a>(
        &'a self,
        identity: &'a str,
    ) -> BoxFuture<'a, moso::auth::Result<Option<User>>> {
        Box::pin(async move {
            let rows = self
                .rows
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A scan, because a `HashMap` has one key. Your database has an
            // index on `lower(email)` and this is a single indexed lookup.
            Ok(rows
                .values()
                .find(|row| row.user.email == identity)
                .map(|row| row.user.clone()))
        })
    }

    fn find_by_id<'a>(&'a self, id: &'a String) -> BoxFuture<'a, moso::auth::Result<Option<User>>> {
        Box::pin(async move {
            let rows = self
                .rows
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(rows.get(id).map(|row| row.user.clone()))
        })
    }

    fn create<'a>(&'a self, account: &'a NewAccount) -> BoxFuture<'a, moso::auth::Result<User>> {
        Box::pin(async move {
            let id = format!("usr_{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
            // Whatever `register` was given, which is where your own sign-up
            // fields arrive. It is `serde_json::Value` and not a struct because
            // the lifecycle flows never look inside it.
            let name = account
                .profile_value()
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();

            let row = Row {
                user: User {
                    id: id.clone(),
                    email: account.identity().to_owned(),
                    name,
                    verified: false,
                    active: true,
                    epoch: 1,
                },
                password_hash: Some(account.password_hash().clone()),
            };

            let user = row.user.clone();
            self.rows
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id, row);
            Ok(user)
        })
    }

    fn password_hash<'a>(
        &'a self,
        id: &'a String,
    ) -> BoxFuture<'a, moso::auth::Result<Option<PasswordHash>>> {
        Box::pin(async move {
            let rows = self
                .rows
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(rows.get(id).and_then(|row| row.password_hash.clone()))
        })
    }

    fn set_password_hash<'a>(
        &'a self,
        id: &'a String,
        hash: &'a PasswordHash,
    ) -> BoxFuture<'a, moso::auth::Result<()>> {
        Box::pin(async move {
            self.edit_row(id, |row| {
                row.password_hash = Some(hash.clone());
                // The credential changed, so every session that carries the old
                // epoch stops being valid at its next request.
                row.user.epoch += 1;
            });
            Ok(())
        })
    }

    fn set_identity<'a>(
        &'a self,
        id: &'a String,
        identity: &'a str,
    ) -> BoxFuture<'a, moso::auth::Result<()>> {
        Box::pin(async move {
            self.edit_row(id, |row| row.user.email = identity.to_owned());
            Ok(())
        })
    }

    fn mark_verified<'a>(&'a self, id: &'a String) -> BoxFuture<'a, moso::auth::Result<()>> {
        Box::pin(async move {
            self.edit_row(id, |row| row.user.verified = true);
            Ok(())
        })
    }

    fn bump_epoch<'a>(&'a self, id: &'a String) -> BoxFuture<'a, moso::auth::Result<()>> {
        Box::pin(async move {
            self.edit_row(id, |row| row.user.epoch += 1);
            Ok(())
        })
    }
}


// ---------------------------------------------------------------------------
// Where a minted token goes
// ---------------------------------------------------------------------------

/// One token that was minted and has to reach somebody.
#[derive(Clone, Debug)]
pub struct Sent {
    /// `verify_email`, `reset_password` or `change_email`.
    pub purpose: &'static str,
    /// The address it is for.
    pub destination: String,
    /// The secret to put in the link. Held, never logged.
    pub token: String,
}

/// Where a minted token is handed to whoever sends the email.
///
/// `moso-auth` deliberately does not depend on a mail crate: which provider you
/// use, what the template says and whether the send goes through a job queue are
/// all yours. What the battery owes you is the token, once, with the address it
/// was minted for — which is what [`Sent`] is.
///
/// This implementation sends nothing. It logs a warning naming the fix, and,
/// **outside the production profile**, keeps the last few tokens in memory so a
/// prototype and a test can follow the link. Under `production` it keeps
/// nothing: a live password-reset token sitting in a process's memory is a
/// credential store nobody meant to run.
#[derive(Debug)]
pub struct Outbox {
    /// Whether a token may be kept at all.
    keep: bool,
    /// The most recent deliveries, newest last.
    sent: Mutex<Vec<Sent>>,
}

/// How many deliveries [`Outbox`] remembers outside production.
const OUTBOX_CAPACITY: usize = 16;

impl Outbox {
    /// An outbox for `profile`.
    #[must_use]
    pub fn for_profile(profile: Profile) -> Self {
        Self {
            keep: profile != Profile::Production,
            sent: Mutex::new(Vec::new()),
        }
    }

    /// Take delivery of one token.
    ///
    /// Replace the body with your mailer. The token is deliberately not in the
    /// log line: a reset token in a log aggregator is a password reset in a log
    /// aggregator.
    pub fn send(&self, token: &IssuedToken) {
        moso::deps::tracing::warn!(
            target: "@@LIB_NAME@@::auth",
            purpose = token.purpose.as_str(),
            destination = %token.destination,
            "a token was minted and nothing sends it; help: replace `Outbox::send` in \
             src/auth.rs with your mailer"
        );

        if !self.keep {
            return;
        }

        let mut sent = self
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sent.len() == OUTBOX_CAPACITY {
            sent.remove(0);
        }
        sent.push(Sent {
            purpose: token.purpose.as_str(),
            destination: token.destination.clone(),
            token: token.expose().to_owned(),
        });
    }

    /// The most recent token of one purpose, if it is still remembered.
    ///
    /// Always `None` under the production profile, where nothing is kept.
    #[must_use]
    pub fn latest(&self, purpose: TokenPurpose) -> Option<Sent> {
        let sent = self
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sent.iter()
            .rev()
            .find(|delivery| delivery.purpose == purpose.as_str())
            .cloned()
    }
}

// ---------------------------------------------------------------------------
// The one provider the handlers take
// ---------------------------------------------------------------------------

/// Everything the routes below need, in one provider.
///
/// One struct rather than four providers, because these handlers are meant to
/// be edited: a file that has to be rewired every time a dependency is added is
/// a file that rots.
pub struct Auth {
    /// Registration, reset, verification and the session bookkeeping.
    accounts: Accounts<Users>,
    /// Where sessions live. The same store the layer writes through.
    sessions: Arc<dyn SessionStore>,
    /// Where minted tokens go.
    outbox: Arc<Outbox>,
    /// How the cookie is written. Held rather than rebuilt, so the layer and
    /// the OpenAPI document cannot disagree about its name.
    session: SessionConfig,
}

impl Auth {
    /// Everything in this process's memory: accounts, sessions and tokens.
    ///
    /// # Errors
    /// When the key-value store behind the lifecycle tokens cannot be built.
    pub fn in_memory(config: &crate::AppConfig) -> Result<Self> {
        // Process-wide, and on purpose: threading a parameter set through every
        // call site is a parameter set that is wrong in one of them, and the one
        // it is wrong in is the one that writes the weak hash. `install_params`
        // raises anything below OWASP's floor rather than obeying it.
        moso::auth::password::install_params(HashParams::new(
            config.hash_memory_kib,
            config.hash_iterations,
            config.hash_parallelism,
        ));

        let sessions = MemorySessionStore::shared() as Arc<dyn SessionStore>;
        let kv = Kv::in_memory("@@LIB_NAME@@").map_err(Error::internal)?;
        let accounts = Accounts::new(
            Arc::new(Users::new()),
            KvLifecycleTokens::shared(kv),
            Arc::clone(&sessions),
        );

        let profile = Profile::detect();
        Ok(Self {
            accounts,
            sessions,
            outbox: Arc::new(Outbox::for_profile(profile)),
            session: session_config(profile),
        })
    }

    /// The layer that reads the cookie on the way in and writes it on the way
    /// out.
    ///
    /// `Secure` comes off the cookie in **development only**, because nothing
    /// works on `http://localhost` with it on;
    /// [`AuthConfig::cookie_for`](moso::auth::AuthConfig::cookie_for) refuses to
    /// drop it in any other profile and logs a warning if asked. Dropping it
    /// also drops the `__Host-` prefix, which a browser only honours on a secure
    /// cookie.
    ///
    /// # Errors
    /// When the signing key is missing or shorter than 32 bytes — the width of
    /// the HMAC-SHA256 that signs the cookie. A short key does not make the
    /// signature shorter, only guessable.
    pub fn session_layer(&self, config: &crate::AppConfig) -> Result<SessionLayer> {
        let layer = SessionLayer::new(Arc::clone(&self.sessions), self.session.clone())
            .keys(vec![config.session_secret.clone()]);
        layer.validate()?;
        Ok(layer)
    }

    /// The account lifecycle: register, reset, verify, log out everywhere.
    #[must_use]
    pub fn accounts(&self) -> &Accounts<Users> {
        &self.accounts
    }

    /// How the session cookie is described in the OpenAPI document.
    ///
    /// Read from the live configuration rather than written out, because the
    /// name changes with the profile: the `__Host-` prefix is only applied to a
    /// `Secure` cookie, so a hard-coded name would be right in production and
    /// wrong on `http://localhost`.
    #[must_use]
    pub fn session_scheme(&self) -> SecurityScheme {
        SecurityScheme::cookie(self.session.cookie.full_name())
    }

    /// Where the sessions are.
    #[must_use]
    pub fn sessions(&self) -> &Arc<dyn SessionStore> {
        &self.sessions
    }

    /// Where minted tokens go.
    #[must_use]
    pub fn outbox(&self) -> &Outbox {
        &self.outbox
    }

    /// The account this session authenticates.
    ///
    /// Three checks, and each closes something. The session has to name
    /// somebody; that account has to still exist; and the `auth_hash` recorded
    /// at login has to still match the account's. The third is what makes "log
    /// out everywhere" free: [`Users::set_password_hash`] and
    /// [`Users::bump_epoch`] both move the epoch, so a password reset
    /// invalidates every session in existence at its next request — no scan, no
    /// fan-out, and it reaches sessions in a store this process cannot see.
    ///
    /// A session that fails any of them is destroyed rather than merely
    /// refused, so the cookie stops coming back.
    ///
    /// # Errors
    /// [`AuthError::Unauthenticated`], the same value for all three, because a
    /// caller learns nothing useful from being told which.
    async fn current_user(&self, session: &Session) -> Result<User> {
        let Some(id) = session.user_id() else {
            return Err(AuthError::Unauthenticated.into());
        };

        let found = self.accounts.store().find_by_id(&id).await?;
        let recorded = session.auth_hash().unwrap_or_default();

        match found {
            Some(user)
                if moso::auth::password::constant_time_eq(&recorded, &user.auth_hash())
                    && user.is_active() =>
            {
                Ok(user)
            }
            _ => {
                // A failure to tidy up is logged and not propagated: the request
                // is already going to be refused, and turning "we could not
                // delete a row" into a 503 would make a store blip look like an
                // outage.
                if let Err(error) = session.destroy().await {
                    moso::deps::tracing::warn!(
                        target: "@@LIB_NAME@@::auth",
                        %error,
                        "could not destroy a session that no longer authenticates anybody"
                    );
                }
                Err(AuthError::Unauthenticated.into())
            }
        }
    }

    /// Find the account and check the password, at the same cost either way.
    ///
    /// # Errors
    /// [`AuthError::InvalidCredentials`] for a missing account, a wrong
    /// password and a suspended account alike — one value, so the three cannot
    /// be told apart by a body, a status or a clock.
    async fn verify(&self, identity: &str, password: &Password) -> moso::auth::Result<User> {
        // The store is asked for the normalised form, which is what it holds:
        // trimmed and lower-cased, matched with `=` and never with `LIKE`.
        let identity = identity.trim().to_lowercase();
        let found = self.accounts.store().find_by_identity(&identity).await?;

        let user = match found {
            Some(user) => match self.accounts.store().password_hash(&user.auth_id()).await? {
                Some(hash) if hash.verify(password).await?.is_valid() => user,
                Some(_) => return Err(AuthError::InvalidCredentials),
                None => {
                    // No password on this account. Still pay for a verify.
                    moso::auth::dummy_verify().await?;
                    return Err(AuthError::InvalidCredentials);
                }
            },
            None => {
                // The miss path pays for a verification too. Without it, "no
                // such account" is a faster 401 than "wrong password", and the
                // clock is the oracle.
                moso::auth::dummy_verify().await?;
                return Err(AuthError::InvalidCredentials);
            }
        };

        // Checked *after* the verification, so a suspended account is not a
        // faster answer than a wrong password either.
        if !user.is_active() {
            return Err(AuthError::InvalidCredentials);
        }
        Ok(user)
    }
}

/// How the session cookie behaves under `profile`.
///
/// `Secure` comes off in **development only**, because nothing works on
/// `http://localhost` with it on. The decision is not made here:
/// [`AuthConfig::cookie_for`](moso::auth::AuthConfig::cookie_for) owns it,
/// refuses to drop `Secure` in any other profile, and logs a warning if asked.
/// Dropping it also drops the `__Host-` prefix, which a browser only honours on
/// a secure cookie.
fn session_config(profile: Profile) -> SessionConfig {
    let mut auth = moso::auth::AuthConfig::default();
    auth.allow_insecure_cookies = profile == Profile::Dev;

    let mut session = SessionConfig::default();
    session.cookie = auth.cookie_for(profile);

    // The prefix goes with it. A browser only honours `__Host-` on a secure
    // cookie, and `SessionConfig::validate` refuses the combination outright
    // rather than quietly ignoring it — so asking for both is a boot error, not
    // a cookie with a decorative name.
    if !session.cookie.secure {
        session.cookie.host_prefix = false;
    }

    session
}

// ---------------------------------------------------------------------------
// What goes on the wire
// ---------------------------------------------------------------------------

/// What `POST /auth/register` accepts.
#[derive(Schema, Debug)]
pub struct Register {
    /// The address that will sign in.
    pub email: Email,
    /// What to call them.
    #[schema(len = 1..=80)]
    pub name: String,
    /// At least twelve characters, checked against a breach list.
    ///
    /// No composition rule: "one uppercase and a symbol" produces `Password1!`
    /// and nothing else. Length, a breach check and a strength estimate are
    /// current NIST guidance.
    pub password: Password,
}

/// What `POST /auth/login` accepts.
#[derive(Schema, Debug)]
pub struct Login {
    /// The address the account signs in with.
    pub email: Email,
    /// The password.
    pub password: Password,
}

/// What `POST /auth/password/forgot` accepts.
#[derive(Schema, Debug)]
pub struct Address {
    /// Where to send the link, if there is an account.
    pub email: Email,
}

/// What `POST /auth/password/reset` accepts.
#[derive(Schema, Debug)]
pub struct Reset {
    /// The token from the link.
    #[schema(len = 1..=512)]
    pub token: String,
    /// The new password.
    pub password: Password,
}

/// The one sentence every "we have done something you cannot see" answer gives.
///
/// Constant, and it has to be: a body that varied with whether the address
/// exists would be the enumeration oracle the `202` exists to close.
#[derive(Schema, Debug)]
pub struct Acknowledged {
    /// What happened, as far as anybody unauthenticated is told.
    pub message: String,
}

impl Acknowledged {
    /// The answer.
    fn new() -> Self {
        Self {
            message: "if that address has an account, we have sent it an email".to_owned(),
        }
    }
}

/// An account, as the API shows it.
#[derive(Schema, Debug)]
pub struct UserOut {
    /// The account key.
    pub id: String,
    /// The address it signs in with.
    pub email: String,
    /// What to call them.
    pub name: String,
    /// Whether the address has been proved reachable.
    pub verified: bool,
}

impl From<User> for UserOut {
    fn from(user: User) -> Self {
        Self {
            id: user.id,
            email: user.email,
            name: user.name,
            verified: user.verified,
        }
    }
}

/// One live session, for the "your devices" listing.
///
/// No identifier, of any kind. A session identifier is the credential, and a
/// digest of one is a name an attacker can go looking for; what a person needs
/// is to recognise their own devices and to know which row is this browser.
#[derive(Schema, Debug)]
pub struct SessionOut {
    /// Whether this is the session making the request.
    pub current: bool,
    /// A coarse label — `Firefox on macOS` — or nothing when the request
    /// carried no user agent.
    pub label: Option<String>,
    /// The address it was created from.
    pub ip: Option<String>,
    /// When it was created, RFC 3339.
    pub created_at: String,
    /// When it was last used, RFC 3339.
    pub last_seen_at: String,
}

impl SessionOut {
    /// Summarise one record, saying whether it is the caller's own.
    fn of(record: &SessionRecord, current: &moso::auth::SessionId) -> Self {
        Self {
            current: record.id.as_str() == current.as_str(),
            label: record.device.label.clone(),
            ip: record.device.ip.clone(),
            created_at: record.created_at.to_rfc3339(),
            last_seen_at: record.last_seen_at.to_rfc3339(),
        }
    }
}

// ---------------------------------------------------------------------------
// The handlers
// ---------------------------------------------------------------------------

/// Create an account.
///
/// Answers `202` with the same body whether or not the address was already
/// registered, and does the same work either way — the taken-address path hashes
/// the password and mints a token too. Only the *content* of the email differs:
/// "confirm your address", or "somebody tried to register with your address".
#[endpoint]
async fn register(
    Inject(auth): Inject<Auth>,
    Json(body): Json<Register>,
) -> Result<Accepted<Json<Acknowledged>>> {
    let profile = json!({ "name": body.name });
    let registration = auth
        .accounts()
        .register(body.email.as_str(), &body.password, profile)
        .await?;

    if let Some(token) = registration.token {
        auth.outbox().send(&token);
    }

    Ok(Accepted::new(Json(Acknowledged::new())))
}

/// Sign in.
///
/// One answer for every way it can fail, at one cost. On success
/// `Session::log_in` cycles the session identifier — the fixation defence — and
/// the session layer writes the replacement cookie.
#[endpoint]
async fn login(
    Inject(auth): Inject<Auth>,
    Depends(AuthSession(session)): Depends<AuthSession>,
    Json(body): Json<Login>,
) -> Result<Json<UserOut>> {
    let user = auth.verify(body.email.as_str(), &body.password).await?;
    session.log_in(&user).await?;
    Ok(Json(UserOut::from(user)))
}

/// Sign out of this browser.
#[endpoint]
async fn logout(Depends(AuthSession(session)): Depends<AuthSession>) -> Result<NoContent> {
    session.destroy().await?;
    Ok(NoContent)
}

/// Every session this account has, newest activity first.
#[endpoint]
async fn sessions(
    Inject(auth): Inject<Auth>,
    Depends(AuthSession(session)): Depends<AuthSession>,
) -> Result<Json<Vec<SessionOut>>> {
    let user = auth.current_user(&session).await?;
    let current = session.id();

    let mut rows: Vec<SessionOut> = auth
        .accounts()
        .sessions_of(&user.auth_id())
        .await?
        .iter()
        .map(|record| SessionOut::of(record, &current))
        .collect();
    rows.sort_by(|left, right| right.last_seen_at.cmp(&left.last_seen_at));
    Ok(Json(rows))
}

/// End every session but this one.
///
/// What a person does after seeing a device in the listing they do not
/// recognise, from the browser they trust.
///
/// The store is asked directly rather than through
/// `Accounts::log_out_everywhere`, which also bumps the epoch: bumping it would
/// invalidate *this* session too at its next request, and signing somebody out
/// of the browser they used to sign the other devices out is not what the button
/// says. `Accounts::log_out_everywhere(id, None)` is the other one, for a
/// password change.
#[endpoint]
async fn revoke_other_sessions(
    Inject(auth): Inject<Auth>,
    Depends(AuthSession(session)): Depends<AuthSession>,
) -> Result<NoContent> {
    let user = auth.current_user(&session).await?;
    let current = session.id();
    auth.sessions()
        .delete_for_user(&user.auth_id(), Some(&current))
        .await?;
    Ok(NoContent)
}

/// Ask for a password-reset link.
///
/// Always `202`, always the same body, always the same work. Issuing a new
/// token revokes the outstanding ones first, so a reset an attacker triggered an
/// hour ago stops working the moment the real user asks for their own.
#[endpoint]
async fn forgot_password(
    Inject(auth): Inject<Auth>,
    Json(body): Json<Address>,
) -> Result<Accepted<Json<Acknowledged>>> {
    let issued = auth
        .accounts()
        .request_password_reset(body.email.as_str())
        .await?;
    if let Some(token) = issued {
        auth.outbox().send(&token);
    }

    Ok(Accepted::new(Json(Acknowledged::new())))
}

/// Redeem a reset link and set a new password.
///
/// Bumps the epoch and deletes every session, this one included: a reset is a
/// recovery, and a recovery that leaves the attacker signed in is not one.
#[endpoint]
async fn reset_password(
    Inject(auth): Inject<Auth>,
    Json(body): Json<Reset>,
) -> Result<NoContent> {
    auth.accounts()
        .reset_password(&body.token, &body.password)
        .await?;
    Ok(NoContent)
}

/// Every route this file serves.
///
/// Nothing is mounted for you: `src/lib.rs` calls this, and you can see it.
///
/// Two groups, because they differ in what a client has to send. The second
/// declares the session cookie as its credential and the `401` it answers
/// without one, so a generated client asks for a cookie on exactly the three
/// routes that need one — and `src/lib.rs` declares the scheme itself, from the
/// live configuration, because the cookie's name depends on the profile.
pub fn router() -> Router {
    let public = moso::routes! {
        POST "/auth/register"        => register,
        POST "/auth/login"           => login,
        POST "/auth/password/forgot" => forgot_password,
        POST "/auth/password/reset"  => reset_password,
    };

    let authenticated = moso::routes! {
        POST   "/auth/logout"   => logout,
        GET    "/auth/sessions" => sessions,
        DELETE "/auth/sessions" => revoke_other_sessions,
    }
    .security(SecurityRequirement::scheme(moso::auth::extract::SESSION_SCHEME))
    .responds(
        401,
        ResponseSpec::problem("no session, or one that no longer authenticates anybody"),
    );

    public.merge(authenticated).tag("auth")
}

// ---------------------------------------------------------------------------
// `moso auth calibrate`
// ---------------------------------------------------------------------------

/// Measure argon2id on this machine and say what to configure.
///
/// The measurement happens **here**, in your binary, because that is the only
/// place it means anything: parameters that hit 250 ms on a laptop are three
/// times too slow in a container with half a CPU, and a constant is wrong on
/// both. `moso auth calibrate` runs this binary with `--dump-auth` and renders
/// what comes back.
///
/// The floor travels with the answer. `moso auth calibrate` refuses to print
/// anything weaker, and it reads the number from here rather than keeping a
/// second copy of OWASP's minimum that could drift from this one.
pub async fn calibrate(request: &Value) -> Value {
    let target_ms = request
        .get("target_ms")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            u64::try_from(moso::auth::TARGET_HASH_TIME.as_millis()).unwrap_or(250)
        });

    let params = match moso::auth::calibrate(Duration::from_millis(target_ms)).await {
        Ok(params) => params,
        Err(error) => {
            return json!({
                "available": false,
                "request": request,
                "reason": format!("the calibration could not run: {error}"),
                "help": "the blocking pool was saturated; run this again on an idle machine",
            });
        }
    };

    json!({
        "available": true,
        "request": request,
        "action": "calibrate",
        "target_ms": target_ms,
        "measured_ms": measure(params).await,
        "params": describe(params),
        "floor": describe(HashParams::OWASP_MINIMUM),
        // The keys this application actually reads them from, so what the CLI
        // prints can be pasted rather than translated.
        "config": [
            format!("{}__HASH_MEMORY_KIB={}", crate::ENV_PREFIX, params.memory_kib),
            format!("{}__HASH_ITERATIONS={}", crate::ENV_PREFIX, params.iterations),
            format!("{}__HASH_PARALLELISM={}", crate::ENV_PREFIX, params.parallelism),
        ],
    })
}

/// One hash at `params`, in milliseconds, or `null` if the probe would not run.
///
/// The search itself is a dozen hashes, so its total says nothing a person
/// wants; what they asked is how long *one login* now costs.
async fn measure(params: HashParams) -> Value {
    let Ok(probe) = Password::new("calibration probe password") else {
        return Value::Null;
    };

    let started = Instant::now();
    match PasswordHash::with_params(&probe, params).await {
        Ok(_) => json!(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
        Err(_) => Value::Null,
    }
}

/// One parameter set, as the CLI reads it.
fn describe(params: HashParams) -> Value {
    json!({
        "memory_kib": params.memory_kib,
        "iterations": params.iterations,
        "parallelism": params.parallelism,
    })
}
