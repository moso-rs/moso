//! [`AuthBackend`] — how a principal is loaded and how credentials are checked
//! — and the [`DatabaseBackend`] most applications use unchanged.
//!
//! # A login is one call, even when it takes two requests
//!
//! [`AuthBackend::authenticate`] has three answers, not two.
//! `Ok(Some(user))` is a login, `Ok(None)` is a refusal, and
//! [`Err(Error::SecondFactorRequired)`](Error::SecondFactorRequired) is "the
//! password was right, now prove the rest". A backend that had only the first
//! two would force every application to re-implement the second step, and the
//! two implementations would disagree about how long a partial authentication
//! lives — which is the whole risk.
//!
//! ```text
//! POST /login  (identity, password)
//!   └─ authenticate → Err(SecondFactorRequired { challenge })      401 + challenge
//! POST /login  (identity, password, challenge, totp)
//!   └─ authenticate → Ok(Some(user))                               200
//! ```
//!
//! The challenge itself is [`crate::mfa::SecondFactorChallenge`] and is minted
//! and claimed by [`crate::mfa::SecondFactorChallenges`]; this module decides
//! *when* one is required and nothing else about it.

use moso_core::BoxFuture;
use moso_orm::{Column, Db, Entity, Predicate, Select, Update};
use moso_schema::Password;

use crate::mfa::SecondFactorChallenges;
use crate::password::{PasswordHash, VerifyOutcome, constant_time_eq, dummy_verify};
use crate::totp::{TotpEnrollment, TotpSecret};
use crate::{AuthUser, Error, Result};

/// What a backend knows about the request doing the authenticating.
///
/// Passed to [`AuthBackend::authenticate`] so a backend can rate-limit per
/// address, record the attempt, or refuse an unusual one — without the
/// signature growing a parameter every time somebody needs another field.
///
/// ```
/// use moso_auth::AuthCtx;
///
/// let ctx = AuthCtx::new().with_ip("203.0.113.7").with_identity("ada@example.com");
/// assert_eq!(ctx.ip(), Some("203.0.113.7"));
/// ```
#[derive(Clone, Debug, Default)]
pub struct AuthCtx {
    /// The caller's address, as the trusted-proxy configuration resolved it.
    ip: Option<String>,
    /// The caller's user agent, truncated.
    user_agent: Option<String>,
    /// The request's correlation id, so an attempt joins to its request.
    request_id: Option<String>,
    /// The identity being attempted, for a per-identity rate limit. Lowercased.
    identity: Option<String>,
}

impl AuthCtx {
    /// An empty context. For a backend called outside a request.
    ///
    /// ```
    /// use moso_auth::AuthCtx;
    ///
    /// assert_eq!(AuthCtx::new().ip(), None);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the caller's address.
    ///
    /// ```
    /// use moso_auth::AuthCtx;
    ///
    /// assert_eq!(AuthCtx::new().with_ip("203.0.113.7").ip(), Some("203.0.113.7"));
    /// ```
    #[must_use]
    pub fn with_ip(mut self, ip: impl Into<String>) -> Self {
        self.ip = Some(ip.into());
        self
    }

    /// Set the caller's user agent.
    ///
    /// ```
    /// use moso_auth::AuthCtx;
    ///
    /// assert!(AuthCtx::new().with_user_agent("curl/8.4.0").user_agent().is_some());
    /// ```
    #[must_use]
    pub fn with_user_agent(mut self, agent: impl Into<String>) -> Self {
        self.user_agent = Some(agent.into());
        self
    }

    /// Set the identity being attempted, for the per-identity rate limit.
    ///
    /// Normalised on the way in, so a throttle keyed on it cannot be evaded by
    /// changing the capitalisation of an address.
    ///
    /// ```
    /// use moso_auth::AuthCtx;
    ///
    /// let ctx = AuthCtx::new().with_identity("  Ada@Example.COM ");
    /// assert_eq!(ctx.identity(), Some("ada@example.com"));
    /// ```
    #[must_use]
    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(crate::lifecycle::normalise(&identity.into()));
        self
    }

    /// Set the request's correlation id.
    ///
    /// ```
    /// use moso_auth::AuthCtx;
    ///
    /// assert!(AuthCtx::new().with_request_id("01J…").request_id().is_some());
    /// ```
    #[must_use]
    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }

    /// The caller's address.
    ///
    /// ```
    /// use moso_auth::AuthCtx;
    ///
    /// assert_eq!(AuthCtx::new().ip(), None);
    /// ```
    #[must_use]
    pub fn ip(&self) -> Option<&str> {
        self.ip.as_deref()
    }

    /// The caller's user agent.
    ///
    /// ```
    /// use moso_auth::AuthCtx;
    ///
    /// assert_eq!(AuthCtx::new().user_agent(), None);
    /// ```
    #[must_use]
    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    /// The identity being attempted.
    ///
    /// ```
    /// use moso_auth::AuthCtx;
    ///
    /// assert_eq!(AuthCtx::new().identity(), None);
    /// ```
    #[must_use]
    pub fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }

    /// The request's correlation id.
    ///
    /// ```
    /// use moso_auth::AuthCtx;
    ///
    /// assert_eq!(AuthCtx::new().request_id(), None);
    /// ```
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }
}

/// An identity and a secret, as a password login presents them.
///
/// The identity is whatever
/// [`DatabaseBackend::identity_column`] names — an email in most applications,
/// a username in some. The secret is a [`Password`], so it is length-bounded
/// before it reaches a hasher: an unbounded password field is a
/// denial-of-service vector against an intentionally slow function.
///
/// ```
/// use moso_auth::PasswordCredentials;
/// use moso_schema::Password;
///
/// let password = Password::new("wharf-lentil-oxide").unwrap();
/// let credentials = PasswordCredentials::new("ada@example.com", password);
/// assert_eq!(credentials.identity(), "ada@example.com");
/// assert_eq!(credentials.totp(), None);
/// ```
///
/// # The second request carries two more fields
///
/// A login that stopped for a second factor comes back with the same identity
/// and password *plus* the code and the challenge the first answer handed out.
/// The password is presented again rather than remembered, so a partial
/// authentication is never a credential on its own: losing the challenge token
/// buys an attacker nothing they did not already need the password for.
#[derive(Debug)]
pub struct PasswordCredentials {
    /// Who is claiming to sign in.
    identity: String,
    /// What they presented.
    password: Password,
    /// The TOTP code, when a second factor was requested and supplied.
    totp: Option<String>,
    /// The challenge token the first request handed out, echoed back.
    challenge: Option<String>,
}

impl PasswordCredentials {
    /// An identity and a password.
    ///
    /// ```
    /// use moso_auth::PasswordCredentials;
    /// use moso_schema::Password;
    ///
    /// let _ = PasswordCredentials::new(
    ///     "ada@example.com",
    ///     Password::new("wharf-lentil-oxide").unwrap(),
    /// );
    /// ```
    #[must_use]
    pub fn new(identity: impl Into<String>, password: Password) -> Self {
        Self {
            identity: identity.into(),
            password,
            totp: None,
            challenge: None,
        }
    }

    /// Attach a TOTP code.
    ///
    /// ```
    /// use moso_auth::PasswordCredentials;
    /// use moso_schema::Password;
    ///
    /// let credentials =
    ///     PasswordCredentials::new("ada@example.com", Password::new("wharf-lentil-oxide").unwrap())
    ///         .with_totp("123456");
    /// assert_eq!(credentials.totp(), Some("123456"));
    /// ```
    #[must_use]
    pub fn with_totp(mut self, code: impl Into<String>) -> Self {
        self.totp = Some(code.into());
        self
    }

    /// Attach the challenge token the first request was answered with.
    ///
    /// A code without one is refused: the code alone says somebody holds an
    /// authenticator, and the challenge is what says they are the same caller
    /// who just proved the password for *this* account.
    ///
    /// ```
    /// use moso_auth::PasswordCredentials;
    /// use moso_schema::Password;
    ///
    /// let credentials =
    ///     PasswordCredentials::new("ada@example.com", Password::new("wharf-lentil-oxide").unwrap())
    ///         .with_totp("123456")
    ///         .with_challenge("opaque-token");
    /// assert_eq!(credentials.challenge(), Some("opaque-token"));
    /// ```
    #[must_use]
    pub fn with_challenge(mut self, challenge: impl Into<String>) -> Self {
        self.challenge = Some(challenge.into());
        self
    }

    /// Who is claiming to sign in.
    ///
    /// ```no_run
    /// # use moso_auth::PasswordCredentials;
    /// # fn f(c: &PasswordCredentials) { let _: &str = c.identity(); }
    /// ```
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// What they presented.
    ///
    /// ```no_run
    /// # use moso_auth::PasswordCredentials;
    /// # use moso_schema::Password;
    /// # fn f(c: &PasswordCredentials) { let _: &Password = c.password(); }
    /// ```
    #[must_use]
    pub fn password(&self) -> &Password {
        &self.password
    }

    /// The TOTP code, when one was supplied.
    ///
    /// ```no_run
    /// # use moso_auth::PasswordCredentials;
    /// # fn f(c: &PasswordCredentials) { let _: Option<&str> = c.totp(); }
    /// ```
    #[must_use]
    pub fn totp(&self) -> Option<&str> {
        self.totp.as_deref()
    }

    /// The challenge token, when one was echoed back.
    ///
    /// ```no_run
    /// # use moso_auth::PasswordCredentials;
    /// # fn f(c: &PasswordCredentials) { let _: Option<&str> = c.challenge(); }
    /// ```
    #[must_use]
    pub fn challenge(&self) -> Option<&str> {
        self.challenge.as_deref()
    }
}

/// How a principal is loaded and how credentials are checked.
///
/// Dyn-compatible so the application's chosen backend can live in the provider
/// map. The boxed futures are decision D4; one allocation per login is not a
/// number anyone will ever measure next to a password hash.
///
/// ```no_run
/// use moso_auth::{AuthBackend, AuthCtx, AuthUser, DefaultUser, Result};
/// use moso_core::BoxFuture;
///
/// /// Accepts one hard-coded account. For a test.
/// pub struct Fixed;
///
/// impl AuthBackend for Fixed {
///     type User = DefaultUser;
///     type Credentials = String;
///
///     fn authenticate<'a>(
///         &'a self,
///         credentials: String,
///         _ctx: &'a AuthCtx,
///     ) -> BoxFuture<'a, Result<Option<DefaultUser>>> {
///         Box::pin(async move {
///             Ok((credentials == "open sesame")
///                 .then(|| DefaultUser::new("usr_1", b"epoch".to_vec())))
///         })
///     }
///
///     fn load<'a>(&'a self, id: &'a String) -> BoxFuture<'a, Result<Option<DefaultUser>>> {
///         let id = id.clone();
///         Box::pin(async move { Ok(Some(DefaultUser::new(id, b"epoch".to_vec()))) })
///     }
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not an authentication backend",
    label = "not an auth backend",
    note = "an auth backend implements `authenticate` (credentials to a principal) and `load` \
            (an identifier to a principal)",
    note = "help: most applications never write one — `DatabaseBackend::<User>::new()` works \
            against any entity with an identity column and a password column",
    note = "help: write your own for LDAP, SAML, or an existing identity service"
)]
pub trait AuthBackend: Send + Sync + 'static {
    /// The principal this backend produces.
    type User: AuthUser;

    /// What it checks. A password pair, a signed assertion, an API key.
    type Credentials: Send + 'static;

    /// Check credentials, returning the principal when they are good.
    ///
    /// `Ok(None)` is "no match", not an error, so the caller decides what a
    /// failure looks like — which is how the timing-equalisation in
    /// [`routes`](crate::routes()) can run a dummy verify on the `None` path.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the store cannot
    /// be reached. Never for a bad password.
    ///
    /// [`Error::SecondFactorRequired`] when the first factor succeeded and a
    /// second one is enrolled. It is an `Err` rather than a third `Ok` variant
    /// because it is the one outcome a caller must not be able to ignore by
    /// pattern-matching on `Some`: `let Some(user) = authenticate(..)? else` is
    /// the shape everybody writes, and it would have signed the user in.
    fn authenticate<'a>(
        &'a self,
        credentials: Self::Credentials,
        ctx: &'a AuthCtx,
    ) -> BoxFuture<'a, Result<Option<Self::User>>>;

    /// Load a principal by identifier, for a session that is already valid.
    ///
    /// Runs on **every authenticated request**, so it is the one method whose
    /// cost matters. Cache it — the `auth_hash` check makes a stale cache safe,
    /// because a credential change invalidates the session anyway.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn load<'a>(
        &'a self,
        id: &'a <Self::User as AuthUser>::Id,
    ) -> BoxFuture<'a, Result<Option<Self::User>>>;
}

/// Loads a principal by identifier, with the credentials half erased away.
///
/// [`CurrentUser`](crate::CurrentUser) needs only this half, and only this half
/// is dyn-compatible without naming `Credentials` — which the extractor cannot,
/// because a request carries a session, not credentials.
///
/// ```no_run
/// use moso_auth::{DefaultUser, UserStore};
///
/// async fn load(store: &dyn UserStore<DefaultUser>, id: &String)
///     -> moso_auth::Result<Option<DefaultUser>>
/// {
///     store.load_user(id).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot load a `{U}`",
    label = "not a user store",
    note = "a user store implements `load_user(&self, id)`",
    note = "help: every `AuthBackend` is one automatically — register the backend with \
            `.provide_dyn::<dyn UserStore<User>>(backend)` so `CurrentUser<User>` can resolve",
    note = "help: `CurrentUser` needs this half and not the whole backend, because a request \
            carries a session and not credentials"
)]
pub trait UserStore<U: AuthUser>: Send + Sync + 'static {
    /// Load a principal by identifier.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn load_user<'a>(&'a self, id: &'a U::Id) -> BoxFuture<'a, Result<Option<U>>>;
}

// Every backend is a user store. `do_not_recommend` keeps a failed `AuthBackend`
// bound from being reported as "consider implementing `UserStore`", which is the
// internal half nobody writes.
#[diagnostic::do_not_recommend]
impl<B: AuthBackend> UserStore<B::User> for B {
    fn load_user<'a>(
        &'a self,
        id: &'a <B::User as AuthUser>::Id,
    ) -> BoxFuture<'a, Result<Option<B::User>>> {
        self.load(id)
    }
}

/// The backend most applications use: an entity, a password column, an identity
/// column.
///
/// Nobody should have to implement a trait to log a user in. This works against
/// any [`Entity`] that has somewhere to put a password hash and something to
/// look a user up by.
///
/// ```text
/// App::new(cfg)
///     .provide(db)
///     .with_auth(
///         DatabaseBackend::<User>::new(db)
///             .identity_column(User::EMAIL)
///             .password_column(User::PASSWORD_HASH)
///             .active_column(User::IS_ACTIVE),
///     )
/// ```
///
/// # The timing equalisation is not optional
///
/// When the identity does not exist, this still runs a password verify against
/// a fixed dummy hash before returning `None`. Without it, "no such account"
/// returns in microseconds and "wrong password" returns in ~250 ms, and the
/// difference is a user-enumeration oracle any script can read. The acceptance
/// criterion is under 10 ms of difference at p95.
///
/// # How the identity is matched
///
/// Exactly, against the normalised form **and** against the trimmed form the
/// caller typed — two `=` comparisons, either of which an index on the column
/// serves. Deliberately not `ilike`: an address like `first_last@example.com`
/// contains a `LIKE` wildcard, and `_` matching any character would let one
/// account's password sign in as another's.
///
/// A deployment that stores mixed-case addresses and wants
/// `Ada@Example.com` to match `ada@EXAMPLE.com` should add a unique index on
/// `lower(email)` and store the address normalised, which is the answer at the
/// database level and does not turn every login into a sequential scan.
///
/// # The second factor, when there is one
///
/// [`second_factor`](DatabaseBackend::second_factor) is one call taking all
/// three things a two-step login needs — where the secret is, where the last
/// accepted period is, and where the partial authentications wait. One call
/// rather than three, because a backend holding two of the three is a backend
/// that either cannot check a code or cannot refuse a replayed one, and neither
/// half-state should be constructible.
pub struct DatabaseBackend<U: Entity + AuthUser> {
    /// Where the rows are.
    db: Db,
    /// The column a user is looked up by. Compared case-insensitively.
    identity: Option<Column<U, String>>,
    /// The column holding the PHC hash.
    password: Option<Column<U, String>>,
    /// The column that says whether the account may sign in.
    active: Option<Column<U, bool>>,
    /// Whether to upgrade a hash whose parameters have aged, on next login.
    rehash_on_login: bool,
    /// Where the second factor's state lives, when one is configured.
    second_factor: Option<SecondFactor<U>>,
}

/// Everything a two-step login needs, so that none of it can be missing alone.
struct SecondFactor<U: Entity> {
    /// The base32 TOTP secret. `NULL` — or empty — means this account has no
    /// second factor, which is the overwhelming majority of rows.
    secret: Column<U, Option<String>>,
    /// The last period a code was accepted from.
    ///
    /// Written back inside the login, and the login fails if the write does:
    /// without it a code observed on the wire is replayable for the rest of its
    /// thirty seconds, which is the window an attacker on the same network has.
    last_period: Column<U, Option<i64>>,
    /// Where a password that checked out waits for its code.
    pending: SecondFactorChallenges,
}

impl<U: Entity + AuthUser> DatabaseBackend<U> {
    /// A backend over `db`.
    ///
    /// The columns are set afterwards; [`validate`](DatabaseBackend::validate)
    /// turns a missing one into a boot error naming the entity and the column
    /// rather than a runtime surprise on the first login.
    ///
    /// ```no_run
    /// # use moso_auth::{AuthUser, DatabaseBackend};
    /// # use moso_orm::{Db, Entity};
    /// # fn f<U: Entity + AuthUser>(db: Db) { let _ = DatabaseBackend::<U>::new(db); }
    /// ```
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self {
            db,
            identity: None,
            password: None,
            active: None,
            rehash_on_login: true,
            second_factor: None,
        }
    }

    /// The column a user is looked up by — an email or a username.
    ///
    /// See the type's own documentation for exactly how the comparison is done
    /// and why it is not `ilike`.
    ///
    /// ```no_run
    /// # use moso_auth::{AuthUser, DatabaseBackend};
    /// # use moso_orm::{Column, Entity};
    /// # fn f<U: Entity + AuthUser>(b: DatabaseBackend<U>, c: Column<U, String>) {
    /// let _ = b.identity_column(c);
    /// # }
    /// ```
    #[must_use]
    pub fn identity_column(mut self, column: Column<U, String>) -> Self {
        self.identity = Some(column);
        self
    }

    /// The column holding the PHC hash.
    ///
    /// ```no_run
    /// # use moso_auth::{AuthUser, DatabaseBackend};
    /// # use moso_orm::{Column, Entity};
    /// # fn f<U: Entity + AuthUser>(b: DatabaseBackend<U>, c: Column<U, String>) {
    /// let _ = b.password_column(c);
    /// # }
    /// ```
    #[must_use]
    pub fn password_column(mut self, column: Column<U, String>) -> Self {
        self.password = Some(column);
        self
    }

    /// The column that says whether the account may sign in.
    ///
    /// Optional: without it, [`AuthUser::is_active`] decides. With it, the
    /// check also happens in SQL — but *after* the password has been verified,
    /// so an attacker cannot use the response to learn which accounts are
    /// suspended.
    ///
    /// ```no_run
    /// # use moso_auth::{AuthUser, DatabaseBackend};
    /// # use moso_orm::{Column, Entity};
    /// # fn f<U: Entity + AuthUser>(b: DatabaseBackend<U>, c: Column<U, bool>) {
    /// let _ = b.active_column(c);
    /// # }
    /// ```
    #[must_use]
    pub fn active_column(mut self, column: Column<U, bool>) -> Self {
        self.active = Some(column);
        self
    }

    /// Whether to re-hash a password whose parameters have aged, on next login.
    ///
    /// On by default. It is the only way a deployment that raises its argon2
    /// parameters ever actually gets the stronger hashes — a migration cannot,
    /// because it does not have the passwords.
    ///
    /// ```no_run
    /// # use moso_auth::{AuthUser, DatabaseBackend};
    /// # use moso_orm::Entity;
    /// # fn f<U: Entity + AuthUser>(b: DatabaseBackend<U>) { let _ = b.rehash_on_login(false); }
    /// ```
    #[must_use]
    pub fn rehash_on_login(mut self, enabled: bool) -> Self {
        self.rehash_on_login = enabled;
        self
    }

    /// Require a time-based second factor from every account that has one.
    ///
    /// `secret` holds the base32 TOTP secret and is `NULL` for an account that
    /// has not enrolled; `last_period` holds the period the last accepted code
    /// came from, and is what makes a code single-use; `pending` is where a
    /// verified password waits for its code.
    ///
    /// With this configured, [`authenticate`](AuthBackend::authenticate)
    /// answers an enrolled account's correct password with
    /// [`Error::SecondFactorRequired`] rather than a user, and only signs in
    /// once the code and the challenge come back together. An account with no
    /// secret is unaffected, so enrolment can be rolled out one user at a time.
    ///
    /// ```no_run
    /// # use moso_auth::{AuthUser, DatabaseBackend};
    /// # use moso_auth::mfa::SecondFactorChallenges;
    /// # use moso_orm::{Column, Entity};
    /// # fn f<U: Entity + AuthUser>(
    /// #     backend: DatabaseBackend<U>,
    /// #     secret: Column<U, Option<String>>,
    /// #     period: Column<U, Option<i64>>,
    /// #     kv: moso_kv::Kv,
    /// # ) {
    /// let _ = backend.second_factor(secret, period, SecondFactorChallenges::new(kv));
    /// # }
    /// ```
    #[must_use]
    pub fn second_factor(
        mut self,
        secret: Column<U, Option<String>>,
        last_period: Column<U, Option<i64>>,
        pending: SecondFactorChallenges,
    ) -> Self {
        self.second_factor = Some(SecondFactor {
            secret,
            last_period,
            pending,
        });
        self
    }

    /// Check the configuration before the first request.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming the entity and the
    /// missing column, with the line to add.
    ///
    /// ```no_run
    /// # use moso_auth::{AuthUser, DatabaseBackend};
    /// # use moso_orm::Entity;
    /// # fn f<U: Entity + AuthUser>(b: &DatabaseBackend<U>) -> moso_auth::Result<()> {
    /// b.validate()
    /// # }
    /// ```
    pub fn validate(&self) -> Result<()> {
        if self.identity.is_none() {
            return Err(Error::Config(
                format!(
                    "`DatabaseBackend<{name}>` has no identity column, so it cannot look anybody \
                     up; help: add `.identity_column({name}::EMAIL)`",
                    name = U::NAME
                )
                .into(),
            ));
        }

        if self.password.is_none() {
            return Err(Error::Config(
                format!(
                    "`DatabaseBackend<{name}>` has no password column, so it cannot verify \
                     anything; help: add `.password_column({name}::PASSWORD_HASH)`",
                    name = U::NAME
                )
                .into(),
            ));
        }

        Ok(())
    }

    /// A database failure, as an authentication failure.
    ///
    /// Always [`Error::Unavailable`]: a query that
    /// could not run has not said the credentials are wrong, and reporting it
    /// as a 401 would log every user out whenever the database blinked.
    ///
    /// Written out rather than left to `?` so that it does not depend on the
    /// `From<moso_orm::Error>` conversion in `error.rs`.
    fn db_failed(error: moso_orm::Error) -> Error {
        Error::Unavailable {
            component: "user store",
            detail: error.to_string(),
            source: Some(Box::new(error)),
        }
    }

    /// The predicate that matches `identity`, however it was capitalised.
    fn identity_matches(&self, identity: &str) -> Result<moso_orm::Predicate> {
        let column = self.identity.ok_or_else(|| {
            Error::Config(
                format!(
                    "`DatabaseBackend<{}>` has no identity column; help: call \
                     `validate()` at boot so this is a boot error and not a login one",
                    U::NAME
                )
                .into(),
            )
        })?;

        let normalised = crate::lifecycle::normalise(identity);
        let typed = identity.trim().to_owned();

        Ok(if normalised == typed {
            column.eq(normalised)
        } else {
            column.eq(normalised).or(column.eq(typed))
        })
    }

    /// The second request of a two-step login: claim the challenge, check the
    /// code, and remember the period it came from.
    ///
    /// Returns whether the login may complete. Every refusal is `Ok(false)` and
    /// not an error, so a missing challenge, a challenge minted for somebody
    /// else, a replayed challenge, an expired one and a wrong code are one
    /// answer — the same answer a wrong password gets.
    ///
    /// The order is deliberate. The challenge is claimed **before** the code is
    /// checked, so a caller cannot use an unlimited number of code guesses
    /// against one challenge; and the period is written back **before** the
    /// login is allowed to succeed, so a store that refuses the write refuses
    /// the login rather than leaving the code replayable.
    async fn second_step(
        &self,
        factor: &SecondFactor<U>,
        subject: &str,
        secret: &str,
        last_period: Option<i64>,
        credentials: &PasswordCredentials,
        matches: Predicate,
    ) -> Result<bool> {
        let (Some(token), Some(code)) = (credentials.challenge(), credentials.totp()) else {
            return Ok(false);
        };

        let Some(claimed) = factor.pending.redeem(token).await? else {
            return Ok(false);
        };
        if !constant_time_eq(claimed.as_bytes(), subject.as_bytes()) {
            tracing::warn!(
                target: "moso_auth::audit",
                event = "second_factor_challenge_misbound",
                "a second-factor challenge was presented for a different account"
            );
            return Ok(false);
        }

        let mut enrolment = TotpEnrollment::resume(
            TotpSecret::from_base32(secret)?,
            last_period.and_then(|period| u64::try_from(period).ok()),
        );
        if !enrolment.check(code)? {
            return Ok(false);
        }

        let period = enrolment
            .last_period()
            .and_then(|period| i64::try_from(period).ok());
        Update::<U>::all()
            .filter(matches)
            .set(factor.last_period, period)
            .execute(&self.db)
            .await
            .map_err(Self::db_failed)?;
        Ok(true)
    }

    /// Upgrade an aged hash, now that the login has actually succeeded.
    ///
    /// A failure here must not fail the login: the user's password is correct
    /// either way, and a store that cannot take the stronger hash today can
    /// take it at the next sign-in.
    async fn upgrade_hash(
        &self,
        password_column: Column<U, String>,
        password: &Password,
        matches: Predicate,
    ) {
        match PasswordHash::new(password).await {
            Ok(upgraded) => {
                let updated = Update::<U>::all()
                    .filter(matches)
                    .set(password_column, upgraded.as_str().to_owned())
                    .execute(&self.db)
                    .await;

                if let Err(error) = updated {
                    tracing::warn!(
                        target: "moso.auth",
                        %error,
                        entity = U::NAME,
                        "could not upgrade an aged password hash on login"
                    );
                }
            }
            Err(error) => tracing::warn!(
                target: "moso.auth",
                %error,
                "could not compute an upgraded password hash on login"
            ),
        }
    }
}

impl<U: Entity + AuthUser> core::fmt::Debug for DatabaseBackend<U> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DatabaseBackend")
            .field("entity", &U::NAME)
            .field("identity", &self.identity.map(|c| c.name()))
            .field("password", &self.password.map(|c| c.name()))
            .field("active", &self.active.map(|c| c.name()))
            .field("rehash_on_login", &self.rehash_on_login)
            .field(
                "second_factor",
                &self
                    .second_factor
                    .as_ref()
                    .map(|factor| factor.secret.name()),
            )
            .finish()
    }
}

// `AuthUser::Id` and `Entity::Pk` are independent associated types, and this
// backend has to turn one into the other to load a row from a session. The
// bound is `Into`, which the blanket `T: Into<T>` satisfies for the overwhelming
// case where they are the same type — so an application whose `auth_id()`
// returns the primary key writes nothing, and one whose identifiers differ gets
// a compile error at the backend rather than a surprise at the first login.
impl<U> AuthBackend for DatabaseBackend<U>
where
    U: Entity + AuthUser,
    <U as AuthUser>::Id: Send + Sync + Into<<U as Entity>::Pk>,
{
    type User = U;
    type Credentials = PasswordCredentials;

    fn authenticate<'a>(
        &'a self,
        credentials: PasswordCredentials,
        ctx: &'a AuthCtx,
    ) -> BoxFuture<'a, Result<Option<U>>> {
        Box::pin(async move {
            let _ = ctx;

            let password_column = self.password.ok_or_else(|| {
                Error::Config(
                    format!(
                        "`DatabaseBackend<{}>` has no password column; help: call `validate()` \
                         at boot",
                        U::NAME
                    )
                    .into(),
                )
            })?;

            let matches = self.identity_matches(credentials.identity())?;

            // One projected read: what it takes to check the credentials, and
            // nothing else. The row is only fetched in full once the password
            // has checked out, so a failed login never loads a user's profile.
            // The two shapes differ by the second factor's two columns, which
            // are read in the same round trip rather than in a second one.
            let stored = match self.second_factor.as_ref() {
                None => Select::<U>::new()
                    .filter(matches.clone())
                    .select((password_column,))
                    .fetch_optional(&self.db)
                    .await
                    .map_err(Self::db_failed)?
                    .map(|(phc,)| (phc, None, None)),
                Some(factor) => Select::<U>::new()
                    .filter(matches.clone())
                    .select((password_column, factor.secret, factor.last_period))
                    .fetch_optional(&self.db)
                    .await
                    .map_err(Self::db_failed)?,
            };

            let Some((phc, secret, last_period)) = stored else {
                // The whole point: this costs what a real verify costs, so the
                // clock does not say whether the account exists.
                dummy_verify().await?;
                return Ok(None);
            };

            let hash = PasswordHash::parse(&phc)?;
            let outcome = hash.verify(credentials.password()).await?;

            if !outcome.is_valid() {
                return Ok(None);
            }

            let mut load = Select::<U>::new().filter(matches.clone());
            if let Some(active) = self.active {
                // After the password, never before: an attacker must not be
                // able to tell a suspended account from a wrong password.
                load = load.filter(active.eq(true));
            }

            let Some(user) = load
                .fetch_optional(&self.db)
                .await
                .map_err(Self::db_failed)?
            else {
                return Ok(None);
            };

            if !user.is_active() {
                return Ok(None);
            }

            // After the account is known to be usable, so a suspended account
            // never earns a challenge and the challenge store is never written
            // to on behalf of a login that could not have completed anyway.
            if let Some(factor) = self.second_factor.as_ref()
                && let Some(secret) = secret.filter(|secret| !secret.trim().is_empty())
            {
                let subject = crate::session::encode_subject(&user.auth_id())?;

                if credentials.totp().is_none() {
                    let challenge = factor.pending.issue(&subject).await?;
                    return Err(Error::SecondFactorRequired {
                        challenge: challenge.token().expose().to_owned(),
                    });
                }

                if !self
                    .second_step(
                        factor,
                        &subject,
                        &secret,
                        last_period,
                        &credentials,
                        matches.clone(),
                    )
                    .await?
                {
                    return Ok(None);
                }
            }

            // The one moment the plaintext is in hand and the parameters are
            // known to be stale — and it is *here*, past the second factor,
            // because a partial authentication is not a login and must not be
            // able to drive a write.
            if self.rehash_on_login && outcome == VerifyOutcome::OkNeedsRehash {
                self.upgrade_hash(password_column, credentials.password(), matches)
                    .await;
            }

            Ok(Some(user))
        })
    }

    fn load<'a>(&'a self, id: &'a <U as AuthUser>::Id) -> BoxFuture<'a, Result<Option<U>>> {
        Box::pin(async move {
            // One statement, on the primary key: this runs on every
            // authenticated request, so it is the one query whose cost matters.
            Select::<U>::find(id.clone().into())
                .fetch_optional(&self.db)
                .await
                .map_err(Self::db_failed)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use moso_orm::descriptor::EntityDescriptor;
    use moso_orm::prelude::TableRef;
    use moso_orm::{ColumnDef, DecodeError, RawQuery, Row, SqlType};

    use super::*;
    use crate::{HashParams, Totp};

    /// The account entity the tests authenticate against.
    ///
    /// Written by hand rather than derived: `#[derive(Entity)]` emits
    /// `::moso::__private::*` paths (decision D6) and the facade is not a
    /// dependency of this crate. `moso-auth` also does not depend on
    /// `moso-sql`, which is why the column kinds come from
    /// `<T as SqlType>::KIND` rather than from `ValueKind` by name.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Account {
        /// The primary key.
        id: i64,
        /// The address it signs in with.
        email: String,
        /// Its PHC hash.
        password_hash: String,
        /// Whether it may sign in.
        is_active: bool,
        /// Bumped by "log out everywhere".
        epoch: i64,
        /// The base32 TOTP secret, when this account enrolled one.
        totp_secret: Option<String>,
        /// The period the last accepted code came from.
        totp_last_period: Option<i64>,
    }

    impl Account {
        const EMAIL: Column<Account, String> = Column::new("email");
        const PASSWORD_HASH: Column<Account, String> = Column::new("password_hash");
        const IS_ACTIVE: Column<Account, bool> = Column::new("is_active");
        const TOTP_SECRET: Column<Account, Option<String>> = Column::new("totp_secret");
        const TOTP_LAST_PERIOD: Column<Account, Option<i64>> = Column::new("totp_last_period");
    }

    impl Entity for Account {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("accounts");
        const NAME: &'static str = "Account";
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", <i64 as SqlType>::KIND).primary_key(),
            ColumnDef::new("email", <String as SqlType>::KIND),
            ColumnDef::new("password_hash", <String as SqlType>::KIND),
            ColumnDef::new("is_active", <bool as SqlType>::KIND),
            ColumnDef::new("epoch", <i64 as SqlType>::KIND),
            ColumnDef::new("totp_secret", <Option<String> as SqlType>::KIND),
            ColumnDef::new("totp_last_period", <Option<i64> as SqlType>::KIND),
        ];

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
                email: row.get_string(1)?,
                password_hash: row.get_string(2)?,
                is_active: row.get_bool(3)?,
                epoch: row.get_i64(4)?,
                totp_secret: row.get_opt::<String>(5)?,
                totp_last_period: row.get_opt::<i64>(6)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| EntityDescriptor::builder("Account", Self::TABLE).build())
        }
    }

    impl AuthUser for Account {
        type Id = i64;

        fn auth_id(&self) -> i64 {
            self.id
        }

        fn auth_hash(&self) -> Vec<u8> {
            let mut material = self.password_hash.as_bytes().to_vec();
            material.extend_from_slice(&self.epoch.to_le_bytes());
            material
        }

        fn is_active(&self) -> bool {
            self.is_active
        }
    }

    /// A SQLite database with the `accounts` table and one row per fixture.
    async fn fixture() -> (Db, PasswordHash) {
        let db = Db::connect_url("sqlite://:memory:").await.unwrap();

        RawQuery::new(ACCOUNTS_DDL).execute(&db).await.unwrap();

        let password = Password::new("wharf-lentil-oxide-77").unwrap();
        let hash = PasswordHash::with_params(&password, HashParams::new(8, 1, 1))
            .await
            .unwrap();

        for (id, email, active) in [
            (1_i64, "ada@example.com", true),
            (2, "Grace@Example.com", true),
            (3, "suspended@example.com", false),
        ] {
            RawQuery::new(
                "insert into accounts (id, email, password_hash, is_active, epoch) \
                 values (?, ?, ?, ?, 0)",
            )
            .bind(id)
            .bind_text(email)
            .bind_text(hash.as_str())
            .bind(active)
            .execute(&db)
            .await
            .unwrap();
        }

        (db, hash)
    }

    /// The table every fixture in this module authenticates against.
    const ACCOUNTS_DDL: &str = "create table accounts (
             id integer primary key,
             email text not null,
             password_hash text not null,
             is_active boolean not null,
             epoch integer not null,
             totp_secret text,
             totp_last_period integer
         )";

    fn backend(db: Db) -> DatabaseBackend<Account> {
        DatabaseBackend::<Account>::new(db)
            .identity_column(Account::EMAIL)
            .password_column(Account::PASSWORD_HASH)
            .active_column(Account::IS_ACTIVE)
    }

    fn credentials(identity: &str, password: &str) -> PasswordCredentials {
        PasswordCredentials::new(identity, Password::new(password).unwrap())
    }

    #[tokio::test]
    async fn the_right_password_authenticates_and_the_wrong_one_does_not() {
        let (db, _) = fixture().await;
        let backend = backend(db);
        let ctx = AuthCtx::new();

        let user = backend
            .authenticate(
                credentials("ada@example.com", "wharf-lentil-oxide-77"),
                &ctx,
            )
            .await
            .unwrap()
            .expect("the right password");
        assert_eq!(user.id, 1);

        assert!(
            backend
                .authenticate(credentials("ada@example.com", "not-the-password"), &ctx)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_nonexistent_account_is_the_same_answer_as_a_wrong_password() {
        let (db, _) = fixture().await;
        let backend = backend(db);

        assert!(
            backend
                .authenticate(
                    credentials("nobody@example.com", "wharf-lentil-oxide-77"),
                    &AuthCtx::new()
                )
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_suspended_account_cannot_sign_in_even_with_the_right_password() {
        let (db, _) = fixture().await;
        let backend = backend(db);

        assert!(
            backend
                .authenticate(
                    credentials("suspended@example.com", "wharf-lentil-oxide-77"),
                    &AuthCtx::new()
                )
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_capitalisation_difference_does_not_stop_a_login() {
        let (db, _) = fixture().await;
        let backend = backend(db);
        let ctx = AuthCtx::new();

        // Stored as `Grace@Example.com`, typed exactly.
        assert!(
            backend
                .authenticate(
                    credentials("Grace@Example.com", "wharf-lentil-oxide-77"),
                    &ctx
                )
                .await
                .unwrap()
                .is_some()
        );

        // Stored lowercase, typed with capitals.
        assert!(
            backend
                .authenticate(
                    credentials("  ADA@example.com  ", "wharf-lentil-oxide-77"),
                    &ctx
                )
                .await
                .unwrap()
                .is_some(),
            "an address is trimmed and lowercased before it is matched"
        );
    }

    /// `first_last@example.com` contains a `LIKE` wildcard. A backend matching
    /// with `ilike` would let this sign in as somebody else.
    #[tokio::test]
    async fn a_wildcard_in_an_address_matches_nothing_but_itself() {
        let (db, _) = fixture().await;
        let backend = backend(db);

        for probe in ["ada%", "_da@example.com", "ada@example_com", "%"] {
            assert!(
                backend
                    .authenticate(credentials(probe, "wharf-lentil-oxide-77"), &AuthCtx::new())
                    .await
                    .unwrap()
                    .is_none(),
                "`{probe}` must not match any account"
            );
        }
    }

    #[tokio::test]
    async fn an_aged_hash_is_upgraded_on_login() {
        let (db, _) = fixture().await;
        let backend = backend(db.clone());

        let before: Vec<(String,)> = Select::<Account>::find(1)
            .select((Account::PASSWORD_HASH,))
            .fetch_all(&db)
            .await
            .unwrap();

        backend
            .authenticate(
                credentials("ada@example.com", "wharf-lentil-oxide-77"),
                &AuthCtx::new(),
            )
            .await
            .unwrap()
            .unwrap();

        let after: Vec<(String,)> = Select::<Account>::find(1)
            .select((Account::PASSWORD_HASH,))
            .fetch_all(&db)
            .await
            .unwrap();

        assert_ne!(before[0].0, after[0].0, "the hash was recomputed");
        let upgraded = PasswordHash::parse(&after[0].0).unwrap();
        assert!(!upgraded.needs_rehash(), "and at the current parameters");
    }

    #[tokio::test]
    async fn upgrading_can_be_turned_off() {
        let (db, _) = fixture().await;
        let backend = backend(db.clone()).rehash_on_login(false);

        let before: Vec<(String,)> = Select::<Account>::find(1)
            .select((Account::PASSWORD_HASH,))
            .fetch_all(&db)
            .await
            .unwrap();

        backend
            .authenticate(
                credentials("ada@example.com", "wharf-lentil-oxide-77"),
                &AuthCtx::new(),
            )
            .await
            .unwrap()
            .unwrap();

        let after: Vec<(String,)> = Select::<Account>::find(1)
            .select((Account::PASSWORD_HASH,))
            .fetch_all(&db)
            .await
            .unwrap();

        assert_eq!(before[0].0, after[0].0);
    }

    // ── the second factor ─────────────────────────────────────────────────

    /// A database whose first account has enrolled a second factor, the backend
    /// that knows about it, and the secret an authenticator would hold.
    async fn enrolled() -> (Db, DatabaseBackend<Account>, TotpSecret) {
        enrolled_with(SecondFactorChallenges::new(
            moso_kv::Kv::in_memory("backend-second-factor").expect("an in-memory kv"),
        ))
        .await
    }

    /// [`enrolled`], with the challenge store supplied — for the expiry test,
    /// which needs a window it can outlive.
    async fn enrolled_with(
        pending: SecondFactorChallenges,
    ) -> (Db, DatabaseBackend<Account>, TotpSecret) {
        let (db, _) = fixture().await;
        let secret = TotpSecret::generate().expect("a secret");

        RawQuery::new("update accounts set totp_secret = ? where id in (1, 2)")
            .bind_text(secret.as_secret().expose())
            .execute(&db)
            .await
            .unwrap();

        // Upgrading is off in these fixtures on purpose. The fixture hash is at
        // `HashParams::new(8, 1, 1)`, so every successful login here would
        // otherwise recompute one at the installed parameters — a 19 MiB argon2
        // per login, several per test, on the same bounded pool as the timing
        // test in this module. `an_aged_hash_is_upgraded_on_login` is where that
        // behaviour is asserted; these tests are about the second factor.
        let backend = backend(db.clone()).rehash_on_login(false).second_factor(
            Account::TOTP_SECRET,
            Account::TOTP_LAST_PERIOD,
            pending,
        );
        (db, backend, secret)
    }

    /// The first request of a two-step login, and the challenge it answers
    /// with.
    async fn first_step(backend: &DatabaseBackend<Account>, identity: &str) -> String {
        match backend
            .authenticate(
                credentials(identity, "wharf-lentil-oxide-77"),
                &AuthCtx::new(),
            )
            .await
        {
            Err(Error::SecondFactorRequired { challenge }) => challenge,
            other => panic!("the first step of a two-step login was {other:?}"),
        }
    }

    /// The second request: the same identity and password, plus the code and
    /// the challenge.
    fn second_step(identity: &str, code: &str, challenge: &str) -> PasswordCredentials {
        credentials(identity, "wharf-lentil-oxide-77")
            .with_totp(code)
            .with_challenge(challenge)
    }

    #[tokio::test]
    async fn an_enrolled_account_is_asked_for_a_code_rather_than_signed_in() {
        let (_db, backend, _secret) = enrolled().await;

        let challenge = first_step(&backend, "ada@example.com").await;
        assert!(!challenge.is_empty(), "the client has to be given a token");

        // An account that has not enrolled is unaffected, which is what makes a
        // gradual rollout possible.
        assert!(
            backend
                .authenticate(
                    credentials("suspended@example.com", "wharf-lentil-oxide-77"),
                    &AuthCtx::new()
                )
                .await
                .unwrap()
                .is_none(),
            "and a suspended account never earns a challenge"
        );
    }

    #[tokio::test]
    async fn a_code_and_its_challenge_finish_the_login_and_record_the_period() {
        let (db, backend, secret) = enrolled().await;
        let challenge = first_step(&backend, "ada@example.com").await;
        let code = Totp::default().current(&secret).expect("a code");

        let user = backend
            .authenticate(
                second_step("ada@example.com", &code, &challenge),
                &AuthCtx::new(),
            )
            .await
            .expect("no store failure")
            .expect("the second step signs in");
        assert_eq!(user.id, 1);

        let period: Vec<(Option<i64>,)> = Select::<Account>::find(1)
            .select((Account::TOTP_LAST_PERIOD,))
            .fetch_all(&db)
            .await
            .unwrap();
        assert!(
            period[0].0.is_some(),
            "the period the code came from has to survive the request that used it"
        );
    }

    /// The binding. A challenge earned against one account must not complete a
    /// login for another, even when the code itself is valid for both.
    #[tokio::test]
    async fn a_challenge_cannot_complete_a_login_for_a_different_account() {
        let (_db, backend, secret) = enrolled().await;

        let graces = first_step(&backend, "Grace@Example.com").await;
        let code = Totp::default().current(&secret).expect("a code");

        assert!(
            backend
                .authenticate(
                    second_step("ada@example.com", &code, &graces),
                    &AuthCtx::new()
                )
                .await
                .expect("no store failure")
                .is_none(),
            "one account's challenge is not another's"
        );

        // …and it is spent either way, so the misdirected attempt did not leave
        // Grace a usable challenge behind.
        assert!(
            backend
                .authenticate(
                    second_step("Grace@Example.com", &code, &graces),
                    &AuthCtx::new()
                )
                .await
                .expect("no store failure")
                .is_none()
        );
    }

    /// Single use, isolated from the code check: a wrong code claims the
    /// challenge, and the right code afterwards has nothing left to present.
    #[tokio::test]
    async fn a_challenge_is_claimed_by_the_first_attempt_right_or_wrong() {
        let (_db, backend, secret) = enrolled().await;
        let challenge = first_step(&backend, "ada@example.com").await;

        assert!(
            backend
                .authenticate(
                    second_step("ada@example.com", "000000", &challenge),
                    &AuthCtx::new()
                )
                .await
                .expect("no store failure")
                .is_none(),
            "a wrong code is a refusal"
        );

        let code = Totp::default().current(&secret).expect("a code");
        assert!(
            backend
                .authenticate(
                    second_step("ada@example.com", &code, &challenge),
                    &AuthCtx::new()
                )
                .await
                .expect("no store failure")
                .is_none(),
            "one challenge buys one attempt, so it cannot be brute-forced"
        );
    }

    /// It expires. The window here is milliseconds so the test does not have to
    /// wait five minutes; the mechanism is the one production uses.
    #[tokio::test]
    async fn an_expired_challenge_cannot_finish_a_login() {
        let (_db, backend, secret) = enrolled_with(
            SecondFactorChallenges::new(
                moso_kv::Kv::in_memory("backend-second-factor").expect("an in-memory kv"),
            )
            .ttl(std::time::Duration::from_millis(20)),
        )
        .await;

        let challenge = first_step(&backend, "ada@example.com").await;
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let code = Totp::default().current(&secret).expect("a code");

        assert!(
            backend
                .authenticate(
                    second_step("ada@example.com", &code, &challenge),
                    &AuthCtx::new()
                )
                .await
                .expect("no store failure")
                .is_none(),
            "a partial authentication that never died would be a password with extra steps"
        );
    }

    #[tokio::test]
    async fn a_code_without_a_challenge_is_refused() {
        let (_db, backend, secret) = enrolled().await;
        let code = Totp::default().current(&secret).expect("a code");

        assert!(
            backend
                .authenticate(
                    credentials("ada@example.com", "wharf-lentil-oxide-77").with_totp(&code),
                    &AuthCtx::new()
                )
                .await
                .expect("no store failure")
                .is_none(),
            "holding an authenticator is not the same as having just proved the password"
        );
    }

    /// The replay rule, end to end: the same code cannot sign in twice inside
    /// its own period, however many challenges are minted for it.
    #[tokio::test]
    async fn a_code_cannot_be_replayed_inside_its_own_period() {
        let (_db, backend, secret) = enrolled().await;
        let code = Totp::default().current(&secret).expect("a code");

        let first = first_step(&backend, "ada@example.com").await;
        assert!(
            backend
                .authenticate(
                    second_step("ada@example.com", &code, &first),
                    &AuthCtx::new()
                )
                .await
                .expect("no store failure")
                .is_some()
        );

        let second = first_step(&backend, "ada@example.com").await;
        assert!(
            backend
                .authenticate(
                    second_step("ada@example.com", &code, &second),
                    &AuthCtx::new()
                )
                .await
                .expect("no store failure")
                .is_none(),
            "a code observed on the wire must not be usable for the rest of its window"
        );
    }

    #[tokio::test]
    async fn an_unreachable_challenge_store_refuses_the_login_rather_than_skipping_the_factor() {
        let kv = moso_kv::Kv::builder("backend-second-factor")
            .store(crate::throttle::tests::DownStore)
            .build()
            .expect("built");
        let (_db, backend, _secret) = enrolled_with(SecondFactorChallenges::new(kv)).await;

        match backend
            .authenticate(
                credentials("ada@example.com", "wharf-lentil-oxide-77"),
                &AuthCtx::new(),
            )
            .await
        {
            Err(Error::Unavailable { component, .. }) => {
                assert_eq!(component, "second-factor challenge store");
            }
            other => panic!("an unreachable challenge store answered {other:?}"),
        }
    }

    #[tokio::test]
    async fn loading_by_identifier_finds_the_row_a_session_recorded() {
        let (db, _) = fixture().await;
        let backend = backend(db);

        assert_eq!(backend.load(&1).await.unwrap().unwrap().id, 1);
        assert!(backend.load(&999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn every_backend_is_a_user_store() {
        let (db, _) = fixture().await;
        let backend = backend(db);

        let store: &dyn UserStore<Account> = &backend;
        assert_eq!(store.load_user(&1).await.unwrap().unwrap().id, 1);
    }

    #[tokio::test]
    async fn a_backend_missing_a_column_says_which_one_and_how_to_fix_it() {
        let identity_only =
            DatabaseBackend::<Account>::new(idle_db().await).identity_column(Account::EMAIL);
        let error = identity_only.validate().unwrap_err();
        assert!(error.to_string().contains("password_column"), "{error}");
        assert!(error.to_string().contains("Account"), "{error}");

        let password_only = DatabaseBackend::<Account>::new(idle_db().await)
            .password_column(Account::PASSWORD_HASH);
        let error = password_only.validate().unwrap_err();
        assert!(error.to_string().contains("identity_column"), "{error}");
    }

    #[tokio::test]
    async fn a_configured_backend_validates() {
        backend(idle_db().await).validate().unwrap();
    }

    /// A `Db` the test never issues a statement against.
    ///
    /// An in-memory SQLite is the cheapest real handle there is, and building a
    /// real one keeps these tests honest about what `DatabaseBackend::new`
    /// takes.
    async fn idle_db() -> Db {
        Db::connect_url("sqlite://:memory:").await.unwrap()
    }

    #[tokio::test]
    async fn the_identity_predicate_is_two_exact_comparisons() {
        let backend = backend(idle_db().await);

        let one = backend.identity_matches("ada@example.com").unwrap();
        assert_eq!(one.entities(), ["Account"]);

        // A typed form that differs from the normalised one produces both.
        let two = backend.identity_matches("Ada@Example.com").unwrap();
        assert_ne!(format!("{one:?}"), format!("{two:?}"));
    }

    #[test]
    fn an_auth_context_carries_what_a_throttle_needs() {
        let ctx = AuthCtx::new()
            .with_ip("203.0.113.7")
            .with_user_agent("curl/8.4.0")
            .with_identity("  Ada@Example.COM ")
            .with_request_id("01J");

        assert_eq!(ctx.ip(), Some("203.0.113.7"));
        assert_eq!(ctx.user_agent(), Some("curl/8.4.0"));
        assert_eq!(
            ctx.identity(),
            Some("ada@example.com"),
            "a per-identity quota must not be evadable by pressing shift"
        );
        assert_eq!(ctx.request_id(), Some("01J"));
    }

    /// Acceptance criterion 3: login on a nonexistent account and login with a
    /// wrong password must differ by under 10 ms at p95.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missing_account_and_a_wrong_password_are_indistinguishable_by_the_clock() {
        use std::time::Instant;

        // The floor, because the real parameters are what an attacker times.
        let previous = crate::password::install_params(HashParams::OWASP_MINIMUM);

        let db = Db::connect_url("sqlite://:memory:").await.unwrap();
        RawQuery::new(ACCOUNTS_DDL).execute(&db).await.unwrap();

        let real = PasswordHash::new(&Password::new("wharf-lentil-oxide-77").unwrap())
            .await
            .unwrap();
        RawQuery::new(
            "insert into accounts (id, email, password_hash, is_active, epoch, totp_secret, \
             totp_last_period) values (1, 'ada@example.com', ?, 1, 0, null, null)",
        )
        .bind_text(real.as_str())
        .execute(&db)
        .await
        .unwrap();

        let backend = backend(db);
        let ctx = AuthCtx::new();

        // Warm both paths: the dummy hash is built on first use, and SQLite
        // prepares statements lazily.
        backend
            .authenticate(credentials("ada@example.com", "wrong-password-here"), &ctx)
            .await
            .unwrap();
        backend
            .authenticate(
                credentials("nobody@example.com", "wrong-password-here"),
                &ctx,
            )
            .await
            .unwrap();

        let mut absent = Vec::new();
        let mut wrong = Vec::new();

        for index in 0..21 {
            let started = Instant::now();
            backend
                .authenticate(
                    credentials(&format!("nobody{index}@example.com"), "wrong-password-here"),
                    &ctx,
                )
                .await
                .unwrap();
            absent.push(started.elapsed());

            let started = Instant::now();
            backend
                .authenticate(credentials("ada@example.com", "wrong-password-here"), &ctx)
                .await
                .unwrap();
            wrong.push(started.elapsed());
        }

        absent.sort_unstable();
        wrong.sort_unstable();
        let p95 = |samples: &[std::time::Duration]| samples[samples.len() * 95 / 100];

        let difference = p95(&absent).abs_diff(p95(&wrong));
        assert!(
            difference < std::time::Duration::from_millis(10),
            "p95 differed by {difference:?}: nonexistent {:?}, wrong password {:?}. That gap is \
             a user-enumeration oracle any script can read.",
            p95(&absent),
            p95(&wrong)
        );

        crate::password::install_params(previous);
    }
}
