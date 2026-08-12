//! [`AuthUser`] — the application's own type, opting in to being a principal.

use core::hash::Hash;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// A type that can be authenticated.
///
/// The application's `User` entity implements this. Nothing here dictates what
/// a user *is* — only what authentication needs to know about one.
///
/// ```no_run
/// use moso_auth::AuthUser;
///
/// /// The application's account record.
/// #[derive(Clone)]
/// pub struct User {
///     /// Its key.
///     pub id: u64,
///     /// The hash of the current password.
///     pub password_hash: String,
///     /// Bumped by "log out everywhere".
///     pub session_epoch: i32,
///     /// Whether the account may sign in at all.
///     pub is_active: bool,
/// }
///
/// impl AuthUser for User {
///     type Id = u64;
///
///     fn auth_id(&self) -> u64 {
///         self.id
///     }
///
///     fn auth_hash(&self) -> Vec<u8> {
///         let mut bytes = self.password_hash.as_bytes().to_vec();
///         bytes.extend_from_slice(&self.session_epoch.to_le_bytes());
///         bytes
///     }
///
///     fn is_active(&self) -> bool {
///         self.is_active
///     }
/// }
/// ```
///
/// # What `auth_hash` buys
///
/// "Log out everywhere" without scanning the session store. The hash is
/// recorded on the session when it is created and checked on every load; a
/// mismatch drops the session. A password change or an epoch bump therefore
/// invalidates every session that user has, on every device, at the next
/// request — with no index, no fan-out and no delay.
///
/// Include the password hash *and* a per-user epoch counter. The password hash
/// alone covers a reset; the epoch covers "log me out of my other devices"
/// without a password change.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be authenticated",
    label = "not an authenticatable user",
    note = "an authenticatable user is `Clone + Send + Sync + 'static` and implements `auth_id` \
            and `auth_hash`",
    note = "help: write `impl AuthUser for {Self}` — `auth_id` returns the primary key, and \
            `auth_hash` returns the password hash plus a session epoch, which is what makes \
            \"log out everywhere\" work",
    note = "help: `Id` must round-trip through the session cookie, so it needs \
            `Serialize + DeserializeOwned + Eq + Hash`"
)]
pub trait AuthUser: Clone + Send + Sync + 'static {
    /// The identifier stored in the session and in a token's `sub`.
    ///
    /// Round-trips through the session store, so it has to serialise.
    type Id: Serialize + DeserializeOwned + Clone + Eq + Hash + Send + Sync + 'static;

    /// This user's identifier.
    fn auth_id(&self) -> Self::Id;

    /// Changes when the user's credentials or session epoch change.
    ///
    /// Compared on every session load. A mismatch drops the session, which is
    /// what makes "log out everywhere" a single `UPDATE` rather than a scan.
    ///
    /// Must not be the password hash *alone*: including a per-user epoch lets a
    /// user end their other sessions without changing their password.
    fn auth_hash(&self) -> Vec<u8>;

    /// Whether this account may authenticate at all.
    ///
    /// `false` for a suspended or unverified account. Checked *after* the
    /// password is verified, so an attacker cannot use the response to learn
    /// which accounts are suspended.
    fn is_active(&self) -> bool {
        true
    }
}

/// The user type `CurrentUser` and `MaybeUser` default to.
///
/// A minimal principal for an application that has not written its own: an
/// identifier and an epoch, and nothing else. Most applications replace it with
/// their own entity and never name this type; it exists so the extractors have
/// a default parameter and so `moso new` produces something that compiles
/// before the `User` entity is written.
///
/// ```no_run
/// use moso_auth::{AuthUser, DefaultUser};
///
/// # fn f(u: &DefaultUser) {
/// let _ = u.auth_id();
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DefaultUser {
    /// The account's identifier.
    id: String,
    /// The material `auth_hash` returns.
    epoch: Vec<u8>,
    /// Whether the account may sign in.
    active: bool,
}

impl DefaultUser {
    /// A user with an identifier and an epoch.
    ///
    /// ```no_run
    /// use moso_auth::DefaultUser;
    ///
    /// let _ = DefaultUser::new("usr_1", b"epoch-0".to_vec());
    /// ```
    #[must_use]
    pub fn new(id: impl Into<String>, epoch: Vec<u8>) -> Self {
        Self {
            id: id.into(),
            epoch,
            active: true,
        }
    }

    /// Mark the account inactive.
    ///
    /// ```no_run
    /// # use moso_auth::DefaultUser;
    /// # fn f(u: DefaultUser) { let _ = u.inactive(); }
    /// ```
    #[must_use]
    pub fn inactive(mut self) -> Self {
        self.active = false;
        self
    }
}

impl AuthUser for DefaultUser {
    type Id = String;

    fn auth_id(&self) -> String {
        self.id.clone()
    }

    fn auth_hash(&self) -> Vec<u8> {
        self.epoch.clone()
    }

    fn is_active(&self) -> bool {
        self.active
    }
}
