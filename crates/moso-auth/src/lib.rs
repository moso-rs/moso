#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = "Moso's authentication battery: sessions, passwords, tokens, keys, OAuth2 and passkeys."]
//!
//! Authentication answers "who is making this request". Authorization — "may
//! they do this" — is `moso-authz`, and keeping them apart is deliberate:
//! conflating them is why most frameworks ship a login form and call it
//! security.
//!
//! ```text
//! App::new(cfg)
//!     .provide(db)
//!     .with_auth(
//!         DatabaseBackend::<User>::new(db)
//!             .identity_column(User::EMAIL)
//!             .password_column(User::PASSWORD_HASH)
//!             .active_column(User::IS_ACTIVE),
//!     )
//!     .mount(moso::auth::routes().password().sessions().totp())
//! ```
//!
//! # The map
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`mod@user`] | [`AuthUser`], [`DefaultUser`] |
//! | [`mod@backend`] | [`AuthBackend`], [`UserStore`], [`DatabaseBackend`], [`AuthCtx`] |
//! | [`mod@session`] | [`Session`], [`SessionStore`], [`SessionConfig`], [`SessionLayer`] |
//! | [`mod@password`] | [`PasswordHash`], [`VerifyOutcome`], [`HashParams`], [`calibrate`] |
//! | [`mod@jwt`] | [`Jwt`], [`Claims`], [`RefreshToken`], [`RefreshStore`], [`RemoteJwks`] |
//! | [`mod@jwks`] | [`Jwk`](jwks::Jwk), [`JwkSet`](jwks::JwkSet) |
//! | [`mod@apikey`] | [`ApiKey`], [`ApiKeyStore`], [`ApiKeyAuthenticator`] |
//! | [`mod@oauth`] | [`Provider`], [`OAuthProfile`], [`Pkce`], [`LinkPolicy`] |
//! | `webauthn` (behind `passkeys`) | `WebAuthn`, `PasskeyCredential`, `PasskeyStore` |
//! | [`mod@totp`] | [`Totp`], [`TotpSecret`], [`TotpEnrollment`], [`RecoveryCodes`] |
//! | [`mod@mfa`] | [`MagicLink`], [`SecondFactorChallenge`](mfa::SecondFactorChallenge) |
//! | [`mod@throttle`] | [`LoginThrottle`], [`ThrottleDecision`], [`CaptchaVerifier`], [`SecurityNotice`] |
//! | [`mod@captcha`] | [`HttpCaptchaVerifier`], [`CaptchaProvider`] |
//! | [`mod@extract`] | [`CurrentUser`], [`MaybeUser`], [`AuthSession`], [`Principal`], [`Csrf`] |
//! | [`mod@routes`] | [`AuthRoutes`] and the DTOs |
//! | [`mod@config`] | [`AuthConfig`], [`AuthHealthCheck`] |
//! | [`mod@error`] | [`Error`], and the rule that failures do not distinguish themselves |
//!
//! # What you get without configuring anything
//!
//! | Threat | Default mitigation |
//! | --- | --- |
//! | Session fixation | The id is cycled on login and on privilege change, by the framework |
//! | CSRF | Double-submit token on cookie-authenticated unsafe requests, plus `SameSite=Lax` |
//! | Credential stuffing | Per-address **and** per-identity quotas, a breach check, a notification email |
//! | Timing oracle on account existence | A dummy verify on the miss path; constant-time compare |
//! | Password denial of service | Hashing on a bounded blocking pool |
//! | Token replay | Refresh-token reuse revokes the whole family |
//! | Enumeration through reset | Identical response and timing whether or not the account exists |
//! | Open redirect after login | `next` validated against an allowlist |
//! | Stale sessions after a password change | `auth_hash` invalidates every session at the next request |
//! | Cross-site scripting stealing tokens | The session is an `HttpOnly` cookie; no token in `localStorage` |
//!
//! # Four decisions worth knowing before reading the code
//!
//! **Failures do not distinguish themselves.** "No such account" and "wrong
//! password" are the same response with the same timing, and
//! [`Error::client_facing`] is what enforces it. The variants exist so the
//! *server* can log precisely.
//!
//! **`auth_hash` is what makes "log out everywhere" free.** A hash of the
//! password hash plus a per-user epoch is stored on the session and compared on
//! every load. A mismatch drops the session — no scan, no fan-out, no index.
//!
//! **Password hashing runs off the async runtime.** argon2id is deliberately
//! slow and deliberately memory-hungry; running one per request on the runtime
//! means a login flood stops the server. [`moso_core::task::blocking`] with a
//! bounded pool turns that from an outage into backpressure.
//!
//! **The routes are meant to be copied.** `moso new --auth` writes
//! [`routes()`] into the application. Auth flows always need customisation, and
//! a configuration surface nobody can extend is abandoned at the first
//! requirement it did not anticipate.

pub mod apikey;
pub mod backend;
pub mod captcha;
pub mod config;
pub mod error;
pub mod extract;
pub mod jwks;
pub mod jwt;
pub mod lifecycle;
pub mod mfa;
pub mod oauth;
pub mod password;
pub mod routes;
pub mod session;
pub mod store;
pub mod throttle;
pub mod totp;
pub mod user;
#[cfg(feature = "passkeys")]
#[cfg_attr(docsrs, doc(cfg(feature = "passkeys")))]
pub mod webauthn;

pub use crate::apikey::{
    ApiKey, ApiKeyAuthenticator, ApiKeyStore, KeyEnvironment, MemoryApiKeyStore, NewApiKey,
};
pub use crate::backend::{AuthBackend, AuthCtx, DatabaseBackend, PasswordCredentials, UserStore};
pub use crate::captcha::{CaptchaProvider, HttpCaptchaVerifier};
pub use crate::config::{AuthConfig, AuthHealthCheck};
pub use crate::error::{BoxError, Error, Result};
pub use crate::extract::{
    AuthSession, Csrf, CsrfConfig, CurrentUser, MaybeUser, Principal, PrincipalKind, RequireKind,
    UserType,
};
pub use crate::jwt::{
    Claims, Jwt, JwtAlgorithm, JwtConfig, MemoryRefreshStore, RefreshOutcome, RefreshStore,
    RefreshToken, RemoteJwks,
};
pub use crate::lifecycle::{
    AccountStore, Accounts, EmailChange, IssuedToken, LifecycleConfig, LifecycleTokens, NewAccount,
    Registration, TokenPurpose,
};
pub use crate::mfa::MagicLink;
pub use crate::oauth::{
    AuthorizationRequest, CallbackParams, LinkPolicy, OAuthConfig, OAuthProfile, Pkce, Provider,
    ProviderId, TokenSet, check_link,
};
pub use crate::password::{
    BreachCheck, HashParams, PasswordHash, PasswordPolicy, Strength, TARGET_HASH_TIME,
    VerifyOutcome, calibrate, dummy_verify,
};
pub use crate::routes::{AuthRoutes, AuthState, routes};
pub use crate::session::{
    CookieConfig, DeviceInfo, KvSessionStore, SameSite, Session, SessionConfig, SessionId,
    SessionLayer, SessionRecord, SessionStore,
};
pub use crate::store::{MemorySessionStore, TableSessionStore};
pub use crate::throttle::{
    AttemptRecord, CaptchaVerifier, LoginThrottle, NoticeSink, SecurityNotice, SecurityNoticeKind,
    ThrottleConfig, ThrottleDecision,
};
pub use crate::totp::{RecoveryCodes, Totp, TotpEnrollment, TotpSecret, TotpState};
pub use crate::user::{AuthUser, DefaultUser};
#[cfg(feature = "passkeys")]
#[cfg_attr(docsrs, doc(cfg(feature = "passkeys")))]
pub use crate::webauthn::{PasskeyCredential, PasskeyStore, WebAuthn, WebAuthnChallenge};

/// The version of this crate, for `moso doctor` and the boot log.
///
/// ```
/// assert!(!moso_auth::VERSION.is_empty());
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything an application that authenticates imports.
///
/// ```
/// use moso_auth::prelude::*;
///
/// fn who(principal: &Principal) -> Option<&str> {
///     principal.subject()
/// }
///
/// assert_eq!(who(&Principal::anonymous()), None);
/// ```
pub mod prelude {
    pub use crate::{
        ApiKey, AuthBackend, AuthCtx, AuthSession, AuthUser, CurrentUser, DatabaseBackend, Error,
        MaybeUser, PasswordHash, Principal, PrincipalKind, Result, Session, VerifyOutcome,
    };
}

#[cfg(test)]
mod tests {
    /// The public surface resolves from the crate root, so an application
    /// writes `moso_auth::CurrentUser` and not `moso_auth::extract::CurrentUser`.
    #[test]
    fn the_frozen_surface_resolves_from_the_root() {
        fn exists<T>() {}

        exists::<crate::ApiKey>();
        exists::<crate::AuthConfig>();
        exists::<crate::AuthCtx>();
        exists::<crate::Claims>();
        exists::<crate::CookieConfig>();
        exists::<crate::CsrfConfig>();
        exists::<crate::DefaultUser>();
        exists::<crate::DeviceInfo>();
        exists::<crate::Error>();
        exists::<crate::HashParams>();
        exists::<crate::JwtConfig>();
        exists::<crate::KeyEnvironment>();
        exists::<crate::LinkPolicy>();
        #[cfg(feature = "passkeys")]
        exists::<crate::PasskeyCredential>();
        exists::<crate::PasswordPolicy>();
        exists::<crate::Principal>();
        exists::<crate::PrincipalKind>();
        exists::<crate::ProviderId>();
        exists::<crate::SameSite>();
        exists::<crate::SessionConfig>();
        exists::<crate::SessionRecord>();
        exists::<crate::ThrottleConfig>();
        exists::<crate::ThrottleDecision>();
        exists::<crate::Totp>();
        exists::<crate::VerifyOutcome>();
        #[cfg(feature = "passkeys")]
        exists::<crate::WebAuthn>();
        exists::<crate::CurrentUser<crate::DefaultUser>>();
        exists::<crate::MaybeUser<crate::DefaultUser>>();

        fn dyn_compatible(
            _: &dyn crate::SessionStore,
            _: &dyn crate::ApiKeyStore,
            _: &dyn crate::RefreshStore,
            _: &dyn crate::CaptchaVerifier,
            _: &dyn crate::UserStore<crate::DefaultUser>,
        ) {
        }
        let _ = dyn_compatible;

        #[cfg(feature = "passkeys")]
        {
            fn passkey_store_is_dyn_compatible(_: &dyn crate::PasskeyStore) {}
            let _ = passkey_store_is_dyn_compatible;
        }
    }

    /// The security defaults the documentation promises are the ones the types
    /// actually carry. A default that silently changed would be the worst kind
    /// of regression: invisible, and only in production.
    #[test]
    fn the_cookie_defaults_are_the_documented_ones() {
        use crate::{CookieConfig, SameSite};

        let cookie = CookieConfig::default();
        assert!(
            cookie.http_only,
            "a session cookie must not be readable from JavaScript"
        );
        assert!(
            cookie.secure,
            "a session cookie must not travel in the clear"
        );
        assert!(
            cookie.host_prefix,
            "the __Host- prefix stops a subdomain setting it"
        );
        assert_eq!(cookie.same_site, SameSite::Lax);
        assert!(cookie.domain.is_none(), "a host-only cookie by default");
        assert_eq!(cookie.path, "/");
    }

    /// The session timeouts are the ones `docs/03-batteries/30-auth.md` quotes.
    #[test]
    fn the_session_timeouts_are_the_documented_ones() {
        use std::time::Duration;

        let config = crate::SessionConfig::default();
        assert_eq!(config.idle_timeout, Duration::from_secs(14 * 24 * 3600));
        assert_eq!(config.absolute_timeout, Duration::from_secs(90 * 24 * 3600));
        assert!(
            config.idle_timeout < config.absolute_timeout,
            "an idle timeout above the absolute one would never fire"
        );
    }

    /// The password floor is OWASP's, and nothing may quietly go below it.
    #[test]
    fn the_hash_floor_is_owasps_minimum() {
        use crate::HashParams;

        let floor = HashParams::OWASP_MINIMUM;
        assert_eq!(floor.memory_kib, 19 * 1024);
        assert_eq!(floor.iterations, 2);
        assert_eq!(floor.parallelism, 1);
        assert_eq!(HashParams::default(), floor);
        assert!(floor.at_least(floor));
    }

    /// The password policy is length-and-breach, not composition rules — the
    /// current NIST position, and the one the documentation argues for.
    #[test]
    fn the_password_policy_follows_current_guidance() {
        let policy = crate::PasswordPolicy::default();
        assert_eq!(
            policy.min_length, 12,
            "NIST SP 800-63B: length over composition"
        );
        assert!(
            policy.breach_check,
            "the embedded breach list is free and offline"
        );
        assert!(
            !policy.breach_api,
            "a network call in the signup path is opt-in"
        );
    }

    /// JWT defaults: asymmetric, short-lived, and symmetric algorithms off.
    #[test]
    fn the_jwt_defaults_are_asymmetric_and_short() {
        use std::time::Duration;

        use crate::{JwtAlgorithm, JwtConfig};

        let config = JwtConfig::default();
        assert_eq!(config.algorithm, JwtAlgorithm::EdDSA);
        assert!(!config.algorithm.is_symmetric());
        assert!(
            !config.allow_symmetric,
            "HS256 must be an explicit decision"
        );
        assert_eq!(config.access_ttl, Duration::from_secs(900));
    }

    /// Linking on an unverified address is a documented account-takeover path,
    /// so the default must refuse it.
    #[test]
    fn account_linking_refuses_unverified_addresses_by_default() {
        use crate::LinkPolicy;

        assert_eq!(LinkPolicy::default(), LinkPolicy::VerifiedEmailOrSession);
        assert_ne!(LinkPolicy::default(), LinkPolicy::AnyEmail);
    }
}
