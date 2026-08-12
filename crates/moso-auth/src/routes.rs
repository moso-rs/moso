//! The mountable authentication routes, and the DTOs they speak.
//!
//! # Read this before mounting it
//!
//! `moso new --auth` **copies these handlers into your project**. This mounted
//! version exists for prototyping and is documented as such, because auth flows
//! always need customisation — an extra profile field, a different email, an
//! audit hook, a tenant to resolve — and a framework that hides them behind a
//! configuration object gets abandoned at the first requirement it did not
//! anticipate. Generated code you can edit outlives a configuration surface
//! nobody can extend.
//!
//! What the mounted version *is* good for: getting a working login in a
//! prototype in one line, and being the reference implementation the generated
//! code is copied from — so the two cannot drift.
//!
//! # What gets mounted
//!
//! Nothing, until a flag asks for it. [`AuthRoutes`] is a set of switches and
//! [`AuthRoutes::build`] turns the ones that are on into a [`Router`]:
//!
//! | Flag | Routes |
//! | --- | --- |
//! | [`password`](AuthRoutes::password) | `POST /auth/register`, `/auth/login`, `/auth/logout`, `/auth/logout-all`; `GET /auth/me`; `POST /auth/verify-email` and `/auth/verify-email/resend`; `POST /auth/password/{forgot,reset,change}`; `POST /auth/email/change` and `/auth/email/change/confirm` |
//! | [`sessions`](AuthRoutes::sessions) | `GET`, `POST` and `DELETE /auth/sessions`, and `DELETE /auth/sessions/{handle}` |
//! | [`api_keys`](AuthRoutes::api_keys) | `GET`, `POST` and `DELETE /auth/api-keys`, and `DELETE /auth/api-keys/{prefix}` |
//! | [`oauth`](AuthRoutes::oauth) | `GET /auth/oauth/{provider}` and `GET /auth/oauth/{provider}/callback` |
//! | `passkeys` (behind the `passkeys` feature) | `POST /auth/passkeys/{register,login}/{start,finish}` |
//! | [`totp`](AuthRoutes::totp) | `POST /auth/totp/{setup,confirm,disable}` |
//! | [`magic_link`](AuthRoutes::magic_link) | `POST /auth/magic-link` and `GET /auth/magic-link/{token}` |
//! | [`jwks`](AuthRoutes::jwks) | `GET /.well-known/jwks.json`, at the **root** |
//!
//! # How a handler reaches its dependencies
//!
//! One provider: [`AuthState`], taken as `Inject<AuthState>`. One struct rather
//! than a dozen providers, because these handlers are generated into an
//! application and a generated file that has to be edited whenever a dependency
//! is added is a generated file that rots.
//!
//! The two things that belong to the *router* rather than to the application —
//! the OAuth providers and the redirect allowlist — are captured by the
//! handlers [`build`](AuthRoutes::build) creates, so there is still exactly one
//! place each is written down.
//!
//! # The mounted routes speak [`DefaultUser`]
//!
//! [`AuthRoutes::build`] has no type parameter, so the handlers it registers
//! are one concrete instantiation and the principal type has to be fixed. It is
//! fixed to [`DefaultUser`] — the same type
//! [`CurrentUser`](crate::CurrentUser) defaults to — and the account store is
//! taken as `Arc<dyn AccountStore<User = DefaultUser>>`. The copied version is
//! where an application's own `User` goes: it owns its handlers, so it can name
//! its own types in them.
//!
//! # What the OpenAPI document says about these operations
//!
//! Honestly: less than a `#[endpoint]` would say. A handler registered through
//! [`Router::post`] rather than through the macro carries
//! [`UndocumentedEndpoint`](moso_core::UndocumentedEndpoint), so its request and
//! response **bodies** are stamped `x-moso-undocumented` instead of being
//! described. `moso-auth` cannot use `#[endpoint]`, `routes!` or `ep!`: those
//! expand to `::moso::__private::…` and this crate sits below the facade.
//!
//! What *is* documented is documented because a person wrote it down and it is
//! true of every route in the group: the `auth` tag, the 429 the throttle
//! produces, the 503 an unreachable store produces, and the 401 an
//! authenticated route produces. None of it is inferred, and no request or
//! response schema is invented to fill the gap — a document that is confidently
//! wrong is worse than one that admits a hole.

use std::borrow::Cow;
use std::sync::Arc;

use moso_core::Router;
use moso_openapi::builder::ResponseSpec;
use moso_schema::{Email, Password, Url};
use serde::{Deserialize, Serialize};

use crate::{AccountStore, DefaultUser, Error, IssuedToken, Provider, Result};

// ---------------------------------------------------------------------------
// The DTO boilerplate
// ---------------------------------------------------------------------------

/// Implement [`Validate`](moso_schema::Validate) and [`Schema`](moso_schema::Schema)
/// for a body this module speaks.
///
/// `moso-auth` sits below the facade, so `#[derive(Schema)]` — whose expansion
/// names `::moso::__private::…` — is not available here. This macro is the
/// hand-written equivalent for the one shape every DTO in this file has: a flat
/// struct whose constraints all live in its field *types*.
///
/// `validate` walks the fields rather than answering `Ok(())`, so a field whose
/// type does carry a runtime check reports under its own JSON Pointer, and
/// every failing field is reported rather than the first.
macro_rules! dto {
    (
        $name:ident, $description:literal,
        $( $field:ident : $ty:ty = $doc:literal, $required:literal );+ $(;)?
    ) => {
        impl moso_schema::Validate for $name {
            fn validate(
                &self,
                ctx: &mut moso_schema::ValidationCtx,
            ) -> core::result::Result<(), moso_schema::ValidationErrors> {
                let mut errors = moso_schema::ValidationErrors::new();
                $(
                    ctx.push_field(stringify!($field));
                    if let Err(found) = moso_schema::Validate::validate(&self.$field, ctx) {
                        errors.merge(found);
                    }
                    ctx.pop();
                )+
                if errors.is_empty() { Ok(()) } else { Err(errors) }
            }
        }

        impl moso_schema::Schema for $name {
            fn schema_name() -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(stringify!($name))
            }

            fn json_schema(
                generator: &mut moso_schema::json_schema::SchemaGenerator,
            ) -> moso_schema::json_schema::SchemaNode {
                moso_schema::json_schema::ObjectBuilder::named(stringify!($name))
                    .description($description)
                    $( .property(
                        stringify!($field),
                        moso_schema::schema::schema_of::<$ty>(generator)
                            .with_description($doc),
                        $required,
                    ) )+
                    .build()
            }

            const HAS_CONSTRAINTS: bool =
                false $( || <$ty as moso_schema::Schema>::HAS_CONSTRAINTS )+;
        }
    };
}

mod api_keys;
mod jwks;
mod magic_link;
mod oauth;
#[cfg(feature = "passkeys")]
mod passkeys;
mod password;
mod sessions;
mod support;
mod token;
mod totp;

/// The prefix the routes are mounted under.
///
/// ```
/// assert_eq!(moso_auth::routes::AUTH_PREFIX, "/auth");
/// ```
pub const AUTH_PREFIX: &str = "/auth";

/// The OpenAPI tag every operation this module mounts carries.
///
/// ```
/// assert_eq!(moso_auth::routes::AUTH_TAG, "auth");
/// ```
pub const AUTH_TAG: &str = "auth";

// ---------------------------------------------------------------------------
// AuthRoutes
// ---------------------------------------------------------------------------

/// Builds the auth routes, one flow at a time.
///
/// Nothing is on by default. An application mounts what it uses, which is what
/// keeps the OpenAPI document — and the attack surface — honest.
///
/// ```
/// use moso_auth::routes;
///
/// let router = routes().password().sessions().build();
/// assert!(router.describe().iter().any(|route| route.path == "/auth/login"));
/// ```
#[derive(Default)]
pub struct AuthRoutes {
    /// Whether password login, registration and reset are mounted.
    password: bool,
    /// Whether the session listing and revocation routes are mounted.
    sessions: bool,
    /// Whether the API-key routes are mounted.
    api_keys: bool,
    /// Which OAuth providers are mounted.
    oauth: Vec<Provider>,
    /// Whether the passkey ceremonies are mounted.
    #[cfg(feature = "passkeys")]
    passkeys: bool,
    /// Whether the TOTP routes are mounted.
    totp: bool,
    /// Whether magic-link login is mounted.
    magic_link: bool,
    /// Whether the bearer-token routes — `POST /auth/token` and
    /// `POST /auth/refresh` — are mounted.
    bearer: bool,
    /// Whether the JWKS document is served.
    jwks: bool,
    /// Where a `next` parameter may point after login.
    ///
    /// Empty means "same origin only". Anything else is an explicit allowlist —
    /// an unvalidated `next` is an open redirect, and an open redirect on a
    /// login page is a phishing primitive.
    redirect_allowlist: Vec<String>,
    /// Allowlist entries that were refused, kept until [`AuthRoutes::build`].
    ///
    /// [`AuthRoutes::redirect_allowlist`] returns `Self` and cannot report a
    /// problem, and silently dropping a bad entry would turn "this origin is
    /// allowed" into "this origin is not, and nothing said so". The rejection is
    /// recorded here instead and refused at build time, where it stops a boot
    /// rather than a request.
    rejected: Vec<String>,
}

/// Start building the auth routes.
///
/// ```
/// use moso_auth::routes;
///
/// assert!(routes().password().build().len() > 0);
/// ```
#[must_use]
pub fn routes() -> AuthRoutes {
    AuthRoutes::default()
}

impl AuthRoutes {
    /// Mount password login, registration, verification and reset.
    ///
    /// | Route | Purpose |
    /// | --- | --- |
    /// | `POST /auth/register` | create an account and send verification |
    /// | `POST /auth/login` | password, and a TOTP step when one is enrolled |
    /// | `POST /auth/logout` | destroy the current session |
    /// | `POST /auth/logout-all` | bump the session epoch |
    /// | `GET /auth/me` | the current principal |
    /// | `POST /auth/verify-email` and `/resend` | double opt-in |
    /// | `POST /auth/password/forgot` and `/reset` | reset by emailed token |
    /// | `POST /auth/password/change` | requires the current password; bumps the epoch |
    /// | `POST /auth/email/change` and `/change/confirm` | double opt-in on the new address |
    ///
    /// ```
    /// # use moso_auth::routes;
    /// let router = routes().password().build();
    /// assert_eq!(router.len(), 12);
    /// ```
    #[must_use]
    pub fn password(mut self) -> Self {
        self.password = true;
        self
    }

    /// Mount `GET`, `POST` and `DELETE` on `/auth/sessions`.
    ///
    /// The "your devices" listing, and revoking one. A feature users expect and
    /// almost nobody builds.
    ///
    /// The listing hands out an opaque [`handle`](SessionSummary::handle) rather
    /// than a session id, so revoking one is `DELETE /auth/sessions/{handle}`,
    /// which is mounted alongside the three the table promises. `POST` re-keys
    /// the session making the request — the operation a user who suspects their
    /// cookie leaked actually wants, and the only `POST` on this collection that
    /// is not a second spelling of `/auth/login`.
    ///
    /// ```
    /// # use moso_auth::routes;
    /// let router = routes().sessions().build();
    /// assert_eq!(router.len(), 4);
    /// ```
    #[must_use]
    pub fn sessions(mut self) -> Self {
        self.sessions = true;
        self
    }

    /// Mount `GET`, `POST` and `DELETE` on `/auth/api-keys`.
    ///
    /// `DELETE /auth/api-keys` revokes every key the caller owns;
    /// `DELETE /auth/api-keys/{prefix}` revokes the one the listing names.
    ///
    /// ```
    /// # use moso_auth::routes;
    /// let router = routes().api_keys().build();
    /// assert_eq!(router.len(), 4);
    /// ```
    #[must_use]
    pub fn api_keys(mut self) -> Self {
        self.api_keys = true;
        self
    }

    /// Mount `GET /auth/oauth/{provider}` and its callback for each provider.
    ///
    /// One pair of routes, not one pair per provider: the provider is a path
    /// parameter, and a name that is not in this list is a 404 rather than a
    /// route that exists and fails later.
    ///
    /// ```
    /// # use moso_auth::{routes, OAuthConfig, Provider};
    /// let google = Provider::google(OAuthConfig::new(
    ///     "client-id",
    ///     moso_core::SecretString::new("client-secret"),
    ///     "https://app.example.com/auth/oauth/google/callback",
    /// ));
    /// let router = routes().oauth([google]).build();
    /// assert_eq!(router.len(), 2);
    /// ```
    #[must_use]
    pub fn oauth(mut self, providers: impl IntoIterator<Item = Provider>) -> Self {
        self.oauth.extend(providers);
        self
    }

    /// Mount the passkey registration and authentication ceremonies.
    ///
    /// `POST /auth/passkeys/register/{start,finish}` and
    /// `POST /auth/passkeys/login/{start,finish}`.
    ///
    /// ```
    /// # use moso_auth::routes;
    /// let router = routes().passkeys().build();
    /// assert_eq!(router.len(), 4);
    /// ```
    #[cfg(feature = "passkeys")]
    #[cfg_attr(docsrs, doc(cfg(feature = "passkeys")))]
    #[must_use]
    pub fn passkeys(mut self) -> Self {
        self.passkeys = true;
        self
    }

    /// Mount `POST /auth/totp/{setup,confirm,disable}`.
    ///
    /// ```
    /// # use moso_auth::routes;
    /// let router = routes().totp().build();
    /// assert_eq!(router.len(), 3);
    /// ```
    #[must_use]
    pub fn totp(mut self) -> Self {
        self.totp = true;
        self
    }

    /// Mount `POST /auth/magic-link` and `GET /auth/magic-link/{token}`.
    ///
    /// ```
    /// # use moso_auth::routes;
    /// let router = routes().magic_link().build();
    /// assert_eq!(router.len(), 2);
    /// ```
    #[must_use]
    pub fn magic_link(mut self) -> Self {
        self.magic_link = true;
        self
    }

    /// Mount `POST /auth/token` and `POST /auth/refresh`, the bearer-token flow.
    ///
    /// | Route | Purpose |
    /// | --- | --- |
    /// | `POST /auth/token` | exchange a password (and a TOTP code, when one is enrolled) for a signed access token and an opaque refresh token |
    /// | `POST /auth/refresh` | rotate a refresh token for the next pair, revoking the whole family on reuse |
    ///
    /// This is opt-in and stateless: it sets **no** cookie and touches no
    /// session. It needs an account store ([`AuthState::accounts`]), a signer
    /// ([`AuthState::jwt`]) and a refresh store ([`AuthState::refresh`]); the
    /// [`TableRefreshStore`](crate::store::TableRefreshStore) makes the reuse
    /// detection a compare-and-set, and [`MemoryRefreshStore`](crate::MemoryRefreshStore)
    /// the single-process equivalent.
    ///
    /// ```
    /// # use moso_auth::routes;
    /// let router = routes().bearer().build();
    /// assert_eq!(router.len(), 2);
    /// ```
    #[must_use]
    pub fn bearer(mut self) -> Self {
        self.bearer = true;
        self
    }

    /// Serve the JWKS document at `/.well-known/jwks.json`.
    ///
    /// Mounted at the root, not under `/auth`: the path is well-known and
    /// consumers will not look anywhere else.
    ///
    /// ```
    /// # use moso_auth::routes;
    /// let router = routes().jwks().build();
    /// assert_eq!(router.describe()[0].path, "/.well-known/jwks.json");
    /// ```
    #[must_use]
    pub fn jwks(mut self) -> Self {
        self.jwks = true;
        self
    }

    /// Where a `next` parameter may point after login.
    ///
    /// Without this, `next` may only be a path on the same origin. With it,
    /// only these origins are additionally allowed. There is no "anything"
    /// setting, because an open redirect on a login page is how a phishing page
    /// borrows a real domain.
    ///
    /// An entry containing `*` is **refused**, and the refusal is remembered
    /// rather than applied: this method returns `Self` and has nowhere to put an
    /// error, so the entry is recorded and [`build`](AuthRoutes::build) panics
    /// naming it. Reading `*.example.com` as a pattern is how `evil-example.com`
    /// gets accepted, and dropping the entry quietly would leave an allowlist
    /// that looks configured and allows nothing.
    ///
    /// The complete origin check — absolute, no credentials, no path — lives in
    /// [`AuthConfig::validate`](crate::AuthConfig::validate), which is where an
    /// application's configured list is checked. Here only the wildcard is
    /// caught, because a wildcard is the one bad entry that reads as if it
    /// worked.
    ///
    /// ```
    /// # use moso_auth::routes;
    /// let _ = routes().redirect_allowlist(["https://app.example.com"]);
    /// ```
    #[must_use]
    pub fn redirect_allowlist<S: Into<String>>(
        mut self,
        origins: impl IntoIterator<Item = S>,
    ) -> Self {
        for origin in origins {
            let origin = origin.into();
            if origin.contains('*') {
                self.rejected.push(origin);
                continue;
            }
            self.redirect_allowlist.push(origin);
        }
        self
    }

    /// Check what [`redirect_allowlist`](AuthRoutes::redirect_allowlist) refused.
    ///
    /// Called by [`build`](AuthRoutes::build); public so a composition root that
    /// would rather report than panic can ask first.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming every refused allowlist entry.
    ///
    /// ```
    /// # use moso_auth::routes;
    /// assert!(routes().redirect_allowlist(["https://app.example.com"]).validate().is_ok());
    /// assert!(routes().redirect_allowlist(["https://*.example.com"]).validate().is_err());
    /// ```
    pub fn validate(&self) -> Result<()> {
        if self.rejected.is_empty() {
            return Ok(());
        }

        let listed: String = self
            .rejected
            .iter()
            .map(|entry| format!("\n  - {entry:?}"))
            .collect();
        Err(Error::Config(
            format!(
                "{} redirect-allowlist entr{} contain a wildcard, and this list is compared \
                 origin by origin rather than as a pattern:{listed}\nhelp: list each origin in \
                 full, e.g. \"https://app.example.com\"",
                self.rejected.len(),
                if self.rejected.len() == 1 { "y" } else { "ies" }
            )
            .into(),
        ))
    }

    /// Build the router.
    ///
    /// Every route is tagged [`AUTH_TAG`] and documents the responses its group
    /// can produce — including the 429 the throttle produces on the credential
    /// routes, which is checked inside the handler rather than by a guard,
    /// because the per-identity tier keys on a field of the request *body* and a
    /// [`Guard`](moso_core::Guard) only ever sees the parts.
    ///
    /// # Panics
    ///
    /// When [`redirect_allowlist`](AuthRoutes::redirect_allowlist) was given a
    /// wildcard. This runs in the composition root, before anything is served,
    /// and an open redirect that was configured by accident must not become a
    /// live route. Call [`validate`](AuthRoutes::validate) first to report it
    /// instead.
    ///
    /// ```
    /// # use moso_auth::routes;
    /// let router: moso_core::Router = routes().password().build();
    /// assert!(router.describe().iter().all(|route| route.path.starts_with("/auth/")));
    /// ```
    #[must_use]
    pub fn build(self) -> Router {
        if let Err(error) = self.validate() {
            panic!("moso-auth: {error}");
        }

        let allowlist: Arc<[String]> = Arc::from(self.redirect_allowlist);
        let mut router = Router::new();

        if self.password {
            router = router.merge(password::mount(Arc::clone(&allowlist)));
        }
        if self.sessions {
            router = router.merge(sessions::mount());
        }
        if self.api_keys {
            router = router.merge(api_keys::mount());
        }
        if !self.oauth.is_empty() {
            router = router.merge(oauth::mount(self.oauth, Arc::clone(&allowlist)));
        }
        #[cfg(feature = "passkeys")]
        if self.passkeys {
            router = router.merge(passkeys::mount());
        }
        if self.totp {
            router = router.merge(totp::mount());
        }
        if self.magic_link {
            router = router.merge(magic_link::mount(allowlist));
        }
        if self.bearer {
            router = router.merge(token::mount());
        }
        if self.jwks {
            router = router.merge(jwks::mount());
        }

        router
    }
}

impl From<AuthRoutes> for Router {
    fn from(routes: AuthRoutes) -> Self {
        routes.build()
    }
}

impl core::fmt::Debug for AuthRoutes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthRoutes")
            .field("password", &self.password)
            .field("oauth", &self.oauth.len())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// next
// ---------------------------------------------------------------------------

/// Validate a `next` parameter against the allowlist.
///
/// A relative path with no scheme and no authority is always allowed; anything
/// else must match an allowlisted origin exactly. Protocol-relative URLs
/// (`//evil.example`) are refused, because they are the form people forget.
///
/// Three refusals are about what a *browser* does rather than what a parser
/// does, and they are the reason this function exists at all:
///
/// * a backslash, because browsers normalise `\` to `/` and therefore read
///   `\\evil.example` as `//evil.example`;
/// * a control character, because `/\tevil` is stripped back to `//evil` before
///   the URL is resolved;
/// * a value that means one thing literally and another once percent-decoded,
///   because `/%2f%2fevil.example` parses here as a path and navigates there as
///   an origin.
///
/// # Errors
///
/// [`Error::Ceremony`] when the target is not allowed.
///
/// ```
/// use moso_auth::routes::validate_next;
///
/// assert!(validate_next("/dashboard", &[]).is_ok());
/// assert!(validate_next("//evil.example", &[]).is_err());
/// assert!(validate_next("https://evil.example", &[]).is_err());
///
/// let allowed = ["https://app.example.com".to_owned()];
/// assert!(validate_next("https://app.example.com/welcome", &allowed).is_ok());
/// ```
pub fn validate_next(next: &str, allowlist: &[String]) -> Result<()> {
    check_shape(next)?;

    let decoded = percent_decoded(next);
    if decoded.as_ref() != next {
        check_shape(&decoded)?;
    }

    if is_same_origin_path(next) && is_same_origin_path(&decoded) {
        return Ok(());
    }

    let origin = origin_of(next)?;
    if origin_of(&decoded)? != origin {
        return Err(refused(
            "the target names one origin literally and another once percent-decoded",
        ));
    }

    if allowlist
        .iter()
        .any(|entry| origin_of(entry).is_ok_and(|allowed| allowed == origin))
    {
        return Ok(());
    }

    Err(refused(
        "the target is not a path on this origin and is not an allowlisted origin",
    ))
}

/// The refusal every failed `next` produces.
///
/// One sentence for the log and one 401 for the client: `Error::Ceremony`
/// collapses to [`Error::InvalidCredentials`](crate::Error::InvalidCredentials),
/// so a probe learns nothing about which rule it tripped.
fn refused(reason: &'static str) -> Error {
    Error::ceremony("redirect", reason)
}

/// Refuse the characters a browser rewrites before it resolves a URL.
fn check_shape(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(refused("the target is empty"));
    }
    if value.contains('\\') {
        return Err(refused(
            "the target contains a backslash, which a browser reads as `/`",
        ));
    }
    if value.contains(' ') || value.chars().any(char::is_control) {
        return Err(refused(
            "the target contains a space or a control character, which a browser strips",
        ));
    }
    Ok(())
}

/// Whether `value` is a path on the origin that served the request.
///
/// One leading slash and not two: `//host` is a protocol-relative URL, which is
/// an *absolute* target wearing a path's clothes.
fn is_same_origin_path(value: &str) -> bool {
    value.starts_with('/') && !value.starts_with("//")
}

/// The `scheme://host[:port]` of an absolute `http(s)` target.
///
/// Everything after the authority — path, query, fragment — is dropped, which
/// is what makes the comparison an *origin* comparison: an allowlist entry
/// permits an origin, not a page.
fn origin_of(value: &str) -> Result<String> {
    let parsed = Url::parse_http(value)
        .map_err(|_| refused("the target is not a path and not an absolute http(s) URL"))?;
    let url = parsed.as_url();
    if !url.username().is_empty() || url.password().is_some() {
        return Err(refused("the target carries credentials in its authority"));
    }
    Ok(url.origin().ascii_serialization())
}

/// Percent-decode `value`, leaving a malformed escape exactly as it was.
///
/// Deliberately *not* the `application/x-www-form-urlencoded` decoding: a `+`
/// in a path is a plus sign, and turning it into a space here would make this
/// function disagree with the browser it is modelling.
fn percent_decoded(value: &str) -> Cow<'_, str> {
    if !value.contains('%') {
        return Cow::Borrowed(value);
    }

    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        let escape = (bytes[index] == b'%' && index + 2 < bytes.len())
            .then(|| {
                let high = char::from(bytes[index + 1]).to_digit(16)?;
                let low = char::from(bytes[index + 2]).to_digit(16)?;
                u8::try_from(high * 16 + low).ok()
            })
            .flatten();

        match escape {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }

    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// `POST /auth/register`.
///
/// ```
/// use moso_auth::routes::RegisterRequest;
///
/// let body: RegisterRequest = serde_json::from_str(
///     r#"{"email":"ada@example.com","password":"correct horse battery"}"#,
/// )?;
/// assert_eq!(body.email.as_str(), "ada@example.com");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct RegisterRequest {
    /// The address to register.
    pub email: Email,
    /// The password, length-bounded before it reaches a hasher.
    pub password: Password,
    /// A display name, when the application collects one.
    pub name: Option<String>,
}

dto! {
    RegisterRequest, "A new account.",
    email: Email = "The address to register.", true;
    password: Password = "The password.", true;
    name: Option<String> = "A display name, when the application collects one.", false;
}

/// `POST /auth/login`.
///
/// ```
/// use moso_auth::routes::LoginRequest;
///
/// let body: LoginRequest = serde_json::from_str(
///     r#"{"identity":"ada@example.com","password":"correct horse battery"}"#,
/// )?;
/// assert_eq!(body.identity, "ada@example.com");
/// assert!(body.totp.is_none());
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct LoginRequest {
    /// The address or username.
    pub identity: String,
    /// The password.
    pub password: Password,
    /// The TOTP code, on the second request of a two-step login.
    pub totp: Option<String>,
    /// The challenge from the first step, which binds the two requests.
    pub challenge: Option<String>,
    /// Where to go afterwards. Validated by [`validate_next`].
    pub next: Option<String>,
}

dto! {
    LoginRequest, "A password login, with an optional second factor.",
    identity: String = "The address or username.", true;
    password: Password = "The password.", true;
    totp: Option<String> = "The TOTP code, on the second request of a two-step login.", false;
    challenge: Option<String> = "The challenge from the first step.", false;
    next: Option<String> = "Where to go afterwards.", false;
}

/// What a successful login returns.
///
/// No token by default: the session is in an `HttpOnly` cookie, and putting a
/// copy in the body invites a client to store it where JavaScript can read it.
/// `access_token` and `refresh_token` are populated only for a client that asked
/// for token authentication — the bearer flow at
/// [`POST /auth/token`](AuthRoutes::bearer), never the cookie login.
///
/// ```
/// use moso_auth::routes::LoginResponse;
///
/// let body: LoginResponse = serde_json::from_str(
///     r#"{"requires_second_factor":false,"challenge":null,"access_token":null,
///         "refresh_token":null,"next":"/x"}"#,
/// )?;
/// assert!(!body.requires_second_factor);
/// assert_eq!(body.next.as_deref(), Some("/x"));
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Serialize)]
#[non_exhaustive]
pub struct LoginResponse {
    /// Whether a second factor is still needed.
    pub requires_second_factor: bool,
    /// The challenge to send back with the code, when it is.
    pub challenge: Option<String>,
    /// The access token, for a token-authenticated client only. A short-lived
    /// signed JWT; present only on the bearer flow, and only once every factor
    /// has been cleared.
    pub access_token: Option<String>,
    /// The refresh token, for a token-authenticated client only. Opaque and
    /// high-entropy; present alongside `access_token`, and redeemed at
    /// `POST /auth/refresh` for the next pair.
    pub refresh_token: Option<String>,
    /// Where to go, after validation.
    pub next: Option<String>,
}

impl<'de> Deserialize<'de> for LoginResponse {
    /// Present so `LoginResponse` can satisfy [`Schema`](moso_schema::Schema),
    /// which is `DeserializeOwned` because one type describes both directions.
    /// Nothing in this crate parses one; a generated client does.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> core::result::Result<Self, D::Error> {
        /// The wire shape, so the hand-written impl cannot drift from the
        /// serialised one.
        #[derive(Deserialize)]
        struct Wire {
            requires_second_factor: bool,
            challenge: Option<String>,
            access_token: Option<String>,
            refresh_token: Option<String>,
            next: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            requires_second_factor: wire.requires_second_factor,
            challenge: wire.challenge,
            access_token: wire.access_token,
            refresh_token: wire.refresh_token,
            next: wire.next,
        })
    }
}

dto! {
    LoginResponse, "The outcome of a login.",
    requires_second_factor: bool = "Whether a second factor is still needed.", true;
    challenge: Option<String> = "The challenge to send back with the code.", false;
    access_token: Option<String> = "The access token, for a token client only.", false;
    refresh_token: Option<String> = "The refresh token, for a token client only.", false;
    next: Option<String> = "Where to go, after validation.", false;
}

/// `POST /auth/token` — a password, and a TOTP code when one is enrolled.
///
/// The bearer counterpart of [`LoginRequest`]. It carries no `next` and no
/// `challenge`: the flow is stateless and single-shot, so a client that has a
/// second factor sends the code in the same request rather than in a second one
/// bound by a cookie.
///
/// ```
/// use moso_auth::routes::TokenIssueRequest;
///
/// let body: TokenIssueRequest = serde_json::from_str(
///     r#"{"identity":"ada@example.com","password":"correct horse battery"}"#,
/// )?;
/// assert_eq!(body.identity, "ada@example.com");
/// assert!(body.totp.is_none());
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct TokenIssueRequest {
    /// The address or username.
    pub identity: String,
    /// The password.
    pub password: Password,
    /// The TOTP code, required only for an account that has one enrolled.
    pub totp: Option<String>,
}

dto! {
    TokenIssueRequest, "A bearer-token login, with an optional second factor.",
    identity: String = "The address or username.", true;
    password: Password = "The password.", true;
    totp: Option<String> = "The TOTP code, for an account that has one enrolled.", false;
}

/// `POST /auth/refresh` — the opaque refresh token to rotate.
///
/// ```
/// use moso_auth::routes::RefreshRequest;
///
/// let body: RefreshRequest = serde_json::from_str(r#"{"refresh_token":"abc"}"#)?;
/// assert_eq!(body.refresh_token, "abc");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct RefreshRequest {
    /// The refresh token issued by `POST /auth/token` or the previous rotation.
    pub refresh_token: String,
}

dto! {
    RefreshRequest, "A refresh token to rotate for the next pair.",
    refresh_token: String = "The refresh token to rotate.", true;
}

/// `POST /auth/password/forgot`.
///
/// The response is **always** 202 with the same body and the same timing,
/// whether or not the address exists. Anything else is an enumeration oracle
/// with a friendly error message.
///
/// ```
/// use moso_auth::routes::ForgotPasswordRequest;
///
/// let body: ForgotPasswordRequest =
///     serde_json::from_str(r#"{"email":"ada@example.com"}"#)?;
/// assert_eq!(body.email.domain(), "example.com");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct ForgotPasswordRequest {
    /// The address to send a reset to.
    pub email: Email,
}

dto! {
    ForgotPasswordRequest, "An address to send a password reset to.",
    email: Email = "The address to send a reset to.", true;
}

/// `POST /auth/password/reset`.
///
/// ```
/// use moso_auth::routes::ResetPasswordRequest;
///
/// let body: ResetPasswordRequest = serde_json::from_str(
///     r#"{"token":"abc","password":"correct horse battery"}"#,
/// )?;
/// assert_eq!(body.token, "abc");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct ResetPasswordRequest {
    /// The emailed token.
    pub token: String,
    /// The new password.
    pub password: Password,
}

dto! {
    ResetPasswordRequest, "A password reset, redeeming an emailed token.",
    token: String = "The emailed token.", true;
    password: Password = "The new password.", true;
}

/// `POST /auth/password/change`.
///
/// Requires the current password even inside an authenticated session: without
/// it, an unattended browser is a password change, and a password change is
/// everything.
///
/// ```
/// use moso_auth::routes::ChangePasswordRequest;
///
/// let body: ChangePasswordRequest = serde_json::from_str(
///     r#"{"current_password":"correct horse battery","new_password":"a longer passphrase"}"#,
/// )?;
/// assert!(body.logout_other_sessions.is_none());
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct ChangePasswordRequest {
    /// The password in force now.
    pub current_password: Password,
    /// The one to replace it with.
    pub new_password: Password,
    /// Whether to end every other session. Defaults to true.
    pub logout_other_sessions: Option<bool>,
}

dto! {
    ChangePasswordRequest, "A password change, given the current password.",
    current_password: Password = "The password in force now.", true;
    new_password: Password = "The one to replace it with.", true;
    logout_other_sessions: Option<bool> = "Whether to end every other session.", false;
}

/// One entry in the "your devices" listing.
///
/// ```
/// use moso_auth::routes::SessionSummary;
///
/// let summary: SessionSummary = serde_json::from_str(
///     r#"{"handle":"3f2a","label":"Firefox on macOS","ip":null,
///         "created_at":"2026-01-01T00:00:00Z","last_seen_at":"2026-01-01T00:00:00Z",
///         "current":true}"#,
/// )?;
/// assert!(summary.current);
/// assert_ne!(summary.handle, "");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct SessionSummary {
    /// An opaque handle for revoking this one. **Not** the session id: handing
    /// a client a list of live session ids is handing it a list of credentials.
    pub handle: String,
    /// A coarse device label.
    pub label: Option<String>,
    /// The address it was created from.
    pub ip: Option<String>,
    /// When it was created, RFC 3339.
    pub created_at: String,
    /// When it was last used, RFC 3339.
    pub last_seen_at: String,
    /// Whether this is the session making the request.
    pub current: bool,
}

dto! {
    SessionSummary, "One live session.",
    handle: String = "An opaque handle for revoking this session.", true;
    label: Option<String> = "A coarse device label.", false;
    ip: Option<String> = "The address it was created from.", false;
    created_at: String = "When it was created, RFC 3339.", true;
    last_seen_at: String = "When it was last used, RFC 3339.", true;
    current: bool = "Whether this is the session making the request.", true;
}

/// What creating an API key returns. The only time the secret exists.
///
/// ```
/// use moso_auth::routes::CreatedApiKey;
///
/// let created: CreatedApiKey = serde_json::from_str(
///     r#"{"key":"mso_live_abcdefgh_x","prefix":"abcdefgh","name":"deploy bot",
///         "expires_at":null}"#,
/// )?;
/// assert!(created.key.starts_with("mso_"));
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct CreatedApiKey {
    /// The full key: `mso_live_<prefix>_<secret>`. Shown once, never again.
    pub key: String,
    /// The public prefix, which is what the listing shows afterwards.
    pub prefix: String,
    /// The label.
    pub name: String,
    /// When it expires, RFC 3339.
    pub expires_at: Option<String>,
}

dto! {
    CreatedApiKey, "A newly minted API key. The secret is shown once.",
    key: String = "The full key. Shown once, never again.", true;
    prefix: String = "The public prefix the listing shows afterwards.", true;
    name: String = "The label.", true;
    expires_at: Option<String> = "When it expires, RFC 3339.", false;
}

/// A single-use token, redeemed by posting it back.
///
/// ```
/// use moso_auth::routes::TokenRequest;
///
/// let body: TokenRequest = serde_json::from_str(r#"{"token":"abc"}"#)?;
/// assert_eq!(body.token, "abc");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct TokenRequest {
    /// The token that arrived by email.
    pub token: String,
}

dto! {
    TokenRequest, "A single-use token, redeemed by posting it back.",
    token: String = "The token that arrived by email.", true;
}

/// An address, for a flow that must not say whether it is registered.
///
/// ```
/// use moso_auth::routes::AddressRequest;
///
/// let body: AddressRequest = serde_json::from_str(r#"{"email":"ada@example.com"}"#)?;
/// assert_eq!(body.email.local_part(), "ada");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct AddressRequest {
    /// The address.
    pub email: Email,
}

dto! {
    AddressRequest, "An address, for a flow that must not say whether it is registered.",
    email: Email = "The address.", true;
}

/// `POST /auth/email/change`.
///
/// The current password is required: an address change is a takeover primitive,
/// because the new address can then request a password reset.
///
/// ```
/// use moso_auth::routes::ChangeEmailRequest;
///
/// let body: ChangeEmailRequest = serde_json::from_str(
///     r#"{"new_email":"new@example.com","current_password":"correct horse battery"}"#,
/// )?;
/// assert_eq!(body.new_email.as_str(), "new@example.com");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct ChangeEmailRequest {
    /// The address to move the account to, once it confirms.
    pub new_email: Email,
    /// The password in force now.
    pub current_password: Password,
}

dto! {
    ChangeEmailRequest, "A change of address, pending confirmation by the new one.",
    new_email: Email = "The address to move the account to.", true;
    current_password: Password = "The password in force now.", true;
}

/// The body every "we will send something if there is anything to send" route
/// answers with.
///
/// One constant sentence, so the response is byte-identical whether or not the
/// address exists.
///
/// ```
/// use moso_auth::routes::AcknowledgedResponse;
///
/// assert_eq!(AcknowledgedResponse::new().message, AcknowledgedResponse::MESSAGE);
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct AcknowledgedResponse {
    /// The one sentence this response ever carries.
    pub message: String,
}

impl AcknowledgedResponse {
    /// The sentence, written once so no route can vary it by accident.
    pub const MESSAGE: &'static str = "If that address has an account, we have sent it an email.";

    /// The acknowledgement.
    ///
    /// ```
    /// use moso_auth::routes::AcknowledgedResponse;
    ///
    /// assert!(AcknowledgedResponse::new().message.starts_with("If that address"));
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            message: Self::MESSAGE.to_owned(),
        }
    }
}

impl Default for AcknowledgedResponse {
    fn default() -> Self {
        Self::new()
    }
}

dto! {
    AcknowledgedResponse, "An acknowledgement that says nothing about whether an account exists.",
    message: String = "The one sentence this response ever carries.", true;
}

/// `GET /auth/me`.
///
/// ```
/// use moso_auth::routes::MeResponse;
///
/// let me: MeResponse =
///     serde_json::from_str(r#"{"subject":"usr_1","kind":"session","scopes":[]}"#)?;
/// assert_eq!(me.kind, "session");
/// assert!(me.scopes.is_empty());
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct MeResponse {
    /// Who is acting.
    pub subject: Option<String>,
    /// What kind of credential authenticated them.
    pub kind: String,
    /// The scopes the credential carries. Empty for a session.
    pub scopes: Vec<String>,
}

dto! {
    MeResponse, "The principal making this request.",
    subject: Option<String> = "Who is acting.", false;
    kind: String = "What kind of credential authenticated them.", true;
    scopes: Vec<String> = "The scopes the credential carries.", true;
}

/// `POST /auth/api-keys`.
///
/// ```
/// use moso_auth::routes::CreateApiKeyRequest;
///
/// let body: CreateApiKeyRequest = serde_json::from_str(r#"{"name":"deploy bot"}"#)?;
/// assert_eq!(body.name, "deploy bot");
/// assert!(body.test_key.is_none());
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct CreateApiKeyRequest {
    /// The label, which is all the listing shows.
    pub name: String,
    /// The scopes the key carries.
    pub scopes: Option<Vec<String>>,
    /// How long it lives, in days. Absent means "until it is revoked".
    pub expires_in_days: Option<u32>,
    /// Whether to mint a `test` key rather than a `live` one.
    pub test_key: Option<bool>,
}

dto! {
    CreateApiKeyRequest, "A request to mint an API key.",
    name: String = "The label, which is all the listing shows.", true;
    scopes: Option<Vec<String>> = "The scopes the key carries.", false;
    expires_in_days: Option<u32> = "How long it lives, in days.", false;
    test_key: Option<bool> = "Whether to mint a test key rather than a live one.", false;
}

/// One entry in the API-key listing. Never the secret.
///
/// ```
/// use moso_auth::routes::ApiKeySummary;
///
/// let summary: ApiKeySummary = serde_json::from_str(
///     r#"{"prefix":"abcdefgh","name":"deploy bot","environment":"live","scopes":[],
///         "created_at":"2026-01-01T00:00:00Z","expires_at":null,"last_used_at":null,
///         "revoked":false}"#,
/// )?;
/// assert!(!summary.revoked);
/// assert_eq!(summary.environment, "live");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct ApiKeySummary {
    /// The public prefix, which is how a key is named after it is created.
    pub prefix: String,
    /// The label.
    pub name: String,
    /// `live` or `test`.
    pub environment: String,
    /// The scopes it carries.
    pub scopes: Vec<String>,
    /// When it was created, RFC 3339.
    pub created_at: String,
    /// When it expires, RFC 3339.
    pub expires_at: Option<String>,
    /// When it was last used, RFC 3339.
    pub last_used_at: Option<String>,
    /// Whether it has been revoked.
    pub revoked: bool,
}

dto! {
    ApiKeySummary, "One API key, without its secret.",
    prefix: String = "The public prefix.", true;
    name: String = "The label.", true;
    environment: String = "`live` or `test`.", true;
    scopes: Vec<String> = "The scopes it carries.", true;
    created_at: String = "When it was created, RFC 3339.", true;
    expires_at: Option<String> = "When it expires, RFC 3339.", false;
    last_used_at: Option<String> = "When it was last used, RFC 3339.", false;
    revoked: bool = "Whether it has been revoked.", true;
}

/// `POST /auth/totp/setup`.
///
/// Carries the shared secret, because enrolling a second factor is the one
/// moment the secret has to leave the server. The route requires an
/// authenticated session for exactly that reason.
///
/// ```
/// use moso_auth::routes::TotpSetupResponse;
///
/// let setup: TotpSetupResponse = serde_json::from_str(
///     r#"{"secret":"GEZDGNBVGY3TQOJQ","provisioning_uri":"otpauth://totp/Example:ada"}"#,
/// )?;
/// assert!(setup.provisioning_uri.starts_with("otpauth://"));
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct TotpSetupResponse {
    /// The base32 secret, for a user who types it in by hand.
    pub secret: String,
    /// The `otpauth://` URI to render as a QR code.
    pub provisioning_uri: String,
}

dto! {
    TotpSetupResponse, "A pending TOTP enrolment.",
    secret: String = "The base32 secret, for manual entry.", true;
    provisioning_uri: String = "The otpauth:// URI to render as a QR code.", true;
}

/// A TOTP code, on its own.
///
/// ```
/// use moso_auth::routes::TotpCodeRequest;
///
/// let body: TotpCodeRequest = serde_json::from_str(r#"{"code":"123456"}"#)?;
/// assert_eq!(body.code, "123456");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct TotpCodeRequest {
    /// The code from the authenticator app.
    pub code: String,
}

dto! {
    TotpCodeRequest, "A TOTP code.",
    code: String = "The code from the authenticator app.", true;
}

/// A JSON value this crate deliberately does not model.
///
/// The WebAuthn ceremony payloads are defined by the browser's own API and
/// change with it. Modelling them here would be a second, always-slightly-wrong
/// copy of a specification somebody else owns, so the schema says `any` and
/// means it.
///
/// ```
/// use moso_auth::routes::OpaqueJson;
///
/// let value = OpaqueJson(serde_json::json!({"id": "abc"}));
/// assert_eq!(value.0["id"], "abc");
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(transparent)]
pub struct OpaqueJson(pub serde_json::Value);

impl moso_schema::Validate for OpaqueJson {
    fn validate(
        &self,
        ctx: &mut moso_schema::ValidationCtx,
    ) -> core::result::Result<(), moso_schema::ValidationErrors> {
        let _ = ctx;
        Ok(())
    }
}

impl moso_schema::Schema for OpaqueJson {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("")
    }

    fn json_schema(
        generator: &mut moso_schema::json_schema::SchemaGenerator,
    ) -> moso_schema::json_schema::SchemaNode {
        let _ = generator;
        moso_schema::json_schema::SchemaNode::any()
            .with_description("A WebAuthn ceremony payload, as the browser's own API defines it.")
    }

    fn schema_ref() -> moso_schema::json_schema::SchemaRef {
        moso_schema::inline_schema_ref::<Self>()
    }
}

/// What a passkey ceremony hands to the browser.
///
/// ```
/// use moso_auth::routes::PasskeyChallengeResponse;
///
/// let challenge: PasskeyChallengeResponse =
///     serde_json::from_str(r#"{"options":{"challenge":"abc"}}"#)?;
/// assert_eq!(challenge.options.0["challenge"], "abc");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[cfg(feature = "passkeys")]
#[cfg_attr(docsrs, doc(cfg(feature = "passkeys")))]
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct PasskeyChallengeResponse {
    /// The options to hand to `navigator.credentials`.
    pub options: OpaqueJson,
}

#[cfg(feature = "passkeys")]
dto! {
    PasskeyChallengeResponse, "The options for a WebAuthn ceremony.",
    options: OpaqueJson = "The options to hand to `navigator.credentials`.", true;
}

/// What the browser hands back at the end of a passkey ceremony.
///
/// ```
/// use moso_auth::routes::PasskeyFinishRequest;
///
/// let body: PasskeyFinishRequest =
///     serde_json::from_str(r#"{"response":{"id":"abc"},"label":"YubiKey"}"#)?;
/// assert_eq!(body.label.as_deref(), Some("YubiKey"));
/// # Ok::<(), serde_json::Error>(())
/// ```
#[cfg(feature = "passkeys")]
#[cfg_attr(docsrs, doc(cfg(feature = "passkeys")))]
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct PasskeyFinishRequest {
    /// The credential the browser produced.
    pub response: OpaqueJson,
    /// A label for the credential, on registration.
    pub label: Option<String>,
}

#[cfg(feature = "passkeys")]
dto! {
    PasskeyFinishRequest, "The credential a WebAuthn ceremony produced.",
    response: OpaqueJson = "The credential the browser produced.", true;
    label: Option<String> = "A label for the credential, on registration.", false;
}

/// One registered passkey.
///
/// ```
/// use moso_auth::routes::PasskeySummary;
///
/// let summary: PasskeySummary = serde_json::from_str(
///     r#"{"credential_id":"abc","label":null,"created_at":"2026-01-01T00:00:00Z"}"#,
/// )?;
/// assert!(summary.label.is_none());
/// # Ok::<(), serde_json::Error>(())
/// ```
#[cfg(feature = "passkeys")]
#[cfg_attr(docsrs, doc(cfg(feature = "passkeys")))]
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct PasskeySummary {
    /// The credential's identifier, as the browser spells it.
    pub credential_id: String,
    /// The label it was registered under.
    pub label: Option<String>,
    /// When it was registered, RFC 3339.
    pub created_at: String,
}

#[cfg(feature = "passkeys")]
dto! {
    PasskeySummary, "One registered passkey.",
    credential_id: String = "The credential's identifier.", true;
    label: Option<String> = "The label it was registered under.", false;
    created_at: String = "When it was registered, RFC 3339.", true;
}

/// `POST /auth/magic-link`.
///
/// ```
/// use moso_auth::routes::MagicLinkRequest;
///
/// let body: MagicLinkRequest = serde_json::from_str(r#"{"email":"ada@example.com"}"#)?;
/// assert!(body.next.is_none());
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[non_exhaustive]
pub struct MagicLinkRequest {
    /// The address to send the link to.
    pub email: Email,
    /// Where to go once the link is followed. Validated by [`validate_next`]
    /// **before** it is stored, so a tampered store cannot produce an open
    /// redirect either.
    pub next: Option<String>,
}

dto! {
    MagicLinkRequest, "A request for a one-time login link.",
    email: Email = "The address to send the link to.", true;
    next: Option<String> = "Where to go once the link is followed.", false;
}

// ---------------------------------------------------------------------------
// AuthState
// ---------------------------------------------------------------------------

/// An [`AccountStore`] given as a trait object, made `Sized`.
///
/// [`Accounts<S>`](crate::Accounts) holds an `Arc<S>` and `S` carries the
/// implicit `Sized` bound every type parameter has, so
/// `Accounts<dyn AccountStore<..>>` does not exist. This newtype is the one line
/// that bridges the two: an application configures a trait object, and the
/// lifecycle flows get a concrete type to be generic over.
pub(crate) struct ErasedAccountStore(Arc<dyn AccountStore<User = DefaultUser>>);

impl AccountStore for ErasedAccountStore {
    type User = DefaultUser;

    fn find_by_identity<'a>(
        &'a self,
        identity: &'a str,
    ) -> moso_core::BoxFuture<'a, Result<Option<DefaultUser>>> {
        self.0.find_by_identity(identity)
    }

    fn find_by_id<'a>(
        &'a self,
        id: &'a String,
    ) -> moso_core::BoxFuture<'a, Result<Option<DefaultUser>>> {
        self.0.find_by_id(id)
    }

    fn create<'a>(
        &'a self,
        account: &'a crate::NewAccount,
    ) -> moso_core::BoxFuture<'a, Result<DefaultUser>> {
        self.0.create(account)
    }

    fn password_hash<'a>(
        &'a self,
        id: &'a String,
    ) -> moso_core::BoxFuture<'a, Result<Option<crate::PasswordHash>>> {
        self.0.password_hash(id)
    }

    fn set_password_hash<'a>(
        &'a self,
        id: &'a String,
        hash: &'a crate::PasswordHash,
    ) -> moso_core::BoxFuture<'a, Result<()>> {
        self.0.set_password_hash(id, hash)
    }

    fn set_identity<'a>(
        &'a self,
        id: &'a String,
        identity: &'a str,
    ) -> moso_core::BoxFuture<'a, Result<()>> {
        self.0.set_identity(id, identity)
    }

    fn mark_verified<'a>(&'a self, id: &'a String) -> moso_core::BoxFuture<'a, Result<()>> {
        self.0.mark_verified(id)
    }

    fn bump_epoch<'a>(&'a self, id: &'a String) -> moso_core::BoxFuture<'a, Result<()>> {
        self.0.bump_epoch(id)
    }
}

/// The account lifecycle, as the mounted routes hold it.
pub(crate) type MountedAccounts = crate::Accounts<ErasedAccountStore>;

/// Which email a [`Delivery`] is.
///
/// Not [`TokenPurpose`](crate::TokenPurpose), and deliberately: that enum says
/// what a token *redeems*, and a magic link redeems nothing it names. Issuing a
/// magic link under `TokenPurpose::VerifyEmail` to reuse the enum would make a
/// verification link redeemable as a login — the two vocabularies are kept
/// apart because collapsing them is a privilege escalation.
///
/// ```
/// use moso_auth::routes::DeliveryPurpose;
///
/// assert_eq!(DeliveryPurpose::MagicLink.as_str(), "magic_link");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DeliveryPurpose {
    /// "Confirm your address."
    VerifyEmail,
    /// "Here is a link to set a new password."
    ResetPassword,
    /// "Confirm this is your new address."
    ChangeEmail,
    /// "Here is a link that signs you in."
    MagicLink,
}

impl DeliveryPurpose {
    /// The name a template or a log line uses.
    ///
    /// ```
    /// use moso_auth::routes::DeliveryPurpose;
    ///
    /// assert_eq!(DeliveryPurpose::VerifyEmail.as_str(), "verify_email");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VerifyEmail => "verify_email",
            Self::ResetPassword => "reset_password",
            Self::ChangeEmail => "change_email",
            Self::MagicLink => "magic_link",
        }
    }
}

/// One thing to send, once.
///
/// `Debug` is hand-written and prints no token: a `Delivery` reaches whatever
/// the application does with it, and a derived `Debug` in a mailer's log line
/// would be a live credential in a log aggregator.
///
/// ```
/// use moso_auth::routes::{Delivery, DeliveryPurpose};
///
/// # fn f(delivery: &Delivery) {
/// assert_eq!(delivery.purpose(), DeliveryPurpose::MagicLink);
/// # }
/// ```
pub struct Delivery {
    /// Which email this is.
    purpose: DeliveryPurpose,
    /// Where it goes.
    destination: String,
    /// When the token inside it stops working.
    expires_at: chrono::DateTime<chrono::Utc>,
    /// The token itself.
    token: moso_core::SecretString,
}

impl Delivery {
    /// A delivery carrying `token`.
    ///
    /// ```
    /// use chrono::Utc;
    /// use moso_auth::routes::{Delivery, DeliveryPurpose};
    ///
    /// let delivery = Delivery::new(
    ///     DeliveryPurpose::MagicLink,
    ///     "ada@example.com",
    ///     Utc::now(),
    ///     "the-token",
    /// );
    /// assert_eq!(delivery.destination(), "ada@example.com");
    /// assert_eq!(delivery.expose(), "the-token");
    /// ```
    #[must_use]
    pub fn new(
        purpose: DeliveryPurpose,
        destination: impl Into<String>,
        expires_at: chrono::DateTime<chrono::Utc>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            purpose,
            destination: destination.into(),
            expires_at,
            token: moso_core::SecretString::new(token.into()),
        }
    }

    /// The delivery a lifecycle token asks for.
    ///
    /// ```
    /// use moso_auth::routes::{Delivery, DeliveryPurpose};
    /// use moso_auth::{IssuedToken, TokenPurpose};
    ///
    /// # fn f(token: IssuedToken) {
    /// let delivery = Delivery::from_issued(&token);
    /// assert_eq!(delivery.purpose(), DeliveryPurpose::VerifyEmail);
    /// # }
    /// ```
    #[must_use]
    pub fn from_issued(token: &IssuedToken) -> Self {
        let purpose = match token.purpose {
            crate::TokenPurpose::VerifyEmail => DeliveryPurpose::VerifyEmail,
            crate::TokenPurpose::ResetPassword => DeliveryPurpose::ResetPassword,
            crate::TokenPurpose::ChangeEmail => DeliveryPurpose::ChangeEmail,
        };
        Self::new(
            purpose,
            token.destination.clone(),
            token.expires_at,
            token.expose(),
        )
    }

    /// Which email this is.
    #[must_use]
    pub const fn purpose(&self) -> DeliveryPurpose {
        self.purpose
    }

    /// Where it goes.
    #[must_use]
    pub fn destination(&self) -> &str {
        &self.destination
    }

    /// When the token inside it stops working.
    #[must_use]
    pub const fn expires_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.expires_at
    }

    /// The token, to put in the link.
    ///
    /// The one place it is readable, and the reason it is a method rather than
    /// a field: `.expose()` is greppable in a review.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.token.expose()
    }
}

impl core::fmt::Debug for Delivery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Delivery")
            .field("purpose", &self.purpose)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Where a token this crate minted is handed to whoever sends the email.
///
/// `moso-auth` does not depend on `moso-mail`, and it should not: which mail
/// provider an application uses, what its templates say and whether the send
/// goes through a job queue are all decisions the application owns. What the
/// battery owes it is the token, once, with the address it was minted for —
/// which is exactly what a [`Delivery`] is.
///
/// Registering none is legal and is what a prototype does; the routes then log
/// a warning saying the token was minted and dropped, because a reset flow that
/// silently sends nothing is worse than one that says so.
///
/// ```
/// use std::sync::Arc;
///
/// use moso_auth::routes::TokenSink;
///
/// let sink: TokenSink = Arc::new(|delivery| {
///     Box::pin(async move {
///         // Hand `delivery.expose()` to the mailer here.
///         let _ = delivery.destination().to_owned();
///     })
/// });
/// let _ = sink;
/// ```
pub type TokenSink =
    Arc<dyn Fn(Delivery) -> moso_core::BoxFuture<'static, ()> + Send + Sync + 'static>;

/// Where the auth routes get everything they need.
///
/// One struct rather than eight providers, because the routes are generated
/// into the application and a generated file that has to be edited whenever a
/// dependency is added is a generated file that rots.
///
/// [`new`](AuthState::new) takes the one thing every flow needs — a session
/// store — and every other dependency is added by a builder method. A route
/// whose dependency was never added answers 500 with a sentence naming the
/// builder call that fixes it: the alternative is a route that is mounted and
/// pretends to work.
///
/// ```
/// use std::sync::Arc;
///
/// use moso_auth::{AuthState, MemorySessionStore, SessionStore};
///
/// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
/// let state = AuthState::new(store);
/// assert!(state.login_throttle().is_none(), "nothing is configured by default");
/// ```
#[derive(Clone)]
pub struct AuthState {
    /// Where sessions live.
    sessions: Arc<dyn crate::SessionStore>,
    /// How sessions behave.
    session_config: crate::SessionConfig,
    /// The login throttle, when one is configured.
    throttle: Option<crate::LoginThrottle>,
    /// The password policy.
    password_policy: crate::PasswordPolicy,
    /// Where API keys live.
    api_keys: Option<Arc<dyn crate::ApiKeyStore>>,
    /// Where passkeys live.
    #[cfg(feature = "passkeys")]
    passkeys: Option<Arc<dyn crate::PasskeyStore>>,
    /// Where refresh-token families live.
    refresh: Option<Arc<dyn crate::RefreshStore>>,
    /// The CAPTCHA verifier, when one is configured.
    captcha: Option<Arc<dyn crate::CaptchaVerifier>>,
    /// The account lifecycle, when an account store is configured.
    accounts: Option<Arc<MountedAccounts>>,
    /// The relying party, for the passkey ceremonies.
    #[cfg(feature = "passkeys")]
    webauthn: Option<Arc<crate::WebAuthn>>,
    /// The signer whose public keys `/.well-known/jwks.json` publishes.
    jwt: Option<Arc<crate::Jwt>>,
    /// Where the small pieces of state the mounted routes keep — magic-link
    /// tokens and TOTP enrolments — live.
    kv: Option<moso_kv::Kv>,
    /// Where a minted token is handed over for delivery.
    tokens: Option<TokenSink>,
    /// The name the TOTP provisioning URI is issued under.
    issuer: String,
}

/// The issuer an authenticator app shows when nothing else was configured.
const DEFAULT_ISSUER: &str = "Moso";

impl AuthState {
    /// The minimum: a session store and every default.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// let state = AuthState::new(store);
    /// assert_eq!(state.password_rules().min_length, 12);
    /// ```
    #[must_use]
    pub fn new(sessions: Arc<dyn crate::SessionStore>) -> Self {
        Self {
            sessions,
            session_config: crate::SessionConfig::default(),
            throttle: None,
            password_policy: crate::PasswordPolicy::default(),
            api_keys: None,
            #[cfg(feature = "passkeys")]
            passkeys: None,
            refresh: None,
            captcha: None,
            accounts: None,
            #[cfg(feature = "passkeys")]
            webauthn: None,
            jwt: None,
            kv: None,
            tokens: None,
            issuer: DEFAULT_ISSUER.to_owned(),
        }
    }

    /// Add the login throttle.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, LoginThrottle, MemorySessionStore, SessionStore};
    /// # use moso_auth::ThrottleConfig;
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// let throttle = LoginThrottle::new(
    ///     moso_kv::Kv::in_memory("shop").expect("an in-memory kv"),
    ///     ThrottleConfig::default(),
    /// );
    /// assert!(AuthState::new(store).throttle(throttle).login_throttle().is_some());
    /// ```
    #[must_use]
    pub fn throttle(mut self, throttle: crate::LoginThrottle) -> Self {
        self.throttle = Some(throttle);
        self
    }

    /// Add the API-key store.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{ApiKeyStore, AuthState, MemoryApiKeyStore};
    /// # use moso_auth::{MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// let keys = Arc::new(MemoryApiKeyStore::new()) as Arc<dyn ApiKeyStore>;
    /// assert!(AuthState::new(store).api_keys(keys).api_key_store().is_some());
    /// ```
    #[must_use]
    pub fn api_keys(mut self, store: Arc<dyn crate::ApiKeyStore>) -> Self {
        self.api_keys = Some(store);
        self
    }

    /// Add the passkey store.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, PasskeyStore, SessionStore};
    /// # fn f(store: Arc<dyn SessionStore>, passkeys: Arc<dyn PasskeyStore>) {
    /// assert!(AuthState::new(store).passkeys(passkeys).passkey_store().is_some());
    /// # }
    /// ```
    #[cfg(feature = "passkeys")]
    #[cfg_attr(docsrs, doc(cfg(feature = "passkeys")))]
    #[must_use]
    pub fn passkeys(mut self, store: Arc<dyn crate::PasskeyStore>) -> Self {
        self.passkeys = Some(store);
        self
    }

    /// Add the refresh-token store.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, RefreshStore, SessionStore};
    /// # fn f(store: Arc<dyn SessionStore>, refresh: Arc<dyn RefreshStore>) {
    /// assert!(AuthState::new(store).refresh(refresh).refresh_store().is_some());
    /// # }
    /// ```
    #[must_use]
    pub fn refresh(mut self, store: Arc<dyn crate::RefreshStore>) -> Self {
        self.refresh = Some(store);
        self
    }

    /// Add the CAPTCHA verifier.
    ///
    /// Without one, a [`ThrottleDecision::Challenge`](crate::ThrottleDecision)
    /// is treated as a refusal. That is the safe reading and the one the
    /// trait's own diagnostic promises.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, CaptchaVerifier, SessionStore};
    /// # fn f(store: Arc<dyn SessionStore>, captcha: Arc<dyn CaptchaVerifier>) {
    /// assert!(AuthState::new(store).captcha(captcha).captcha_verifier().is_some());
    /// # }
    /// ```
    #[must_use]
    pub fn captcha(mut self, verifier: Arc<dyn crate::CaptchaVerifier>) -> Self {
        self.captcha = Some(verifier);
        self
    }

    /// Add the account store and the token store the lifecycle flows need.
    ///
    /// The [`PasswordPolicy`](crate::PasswordPolicy) this state carries is
    /// handed to the lifecycle here, so the policy is written down once.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::lifecycle::KvLifecycleTokens;
    /// # use moso_auth::{AccountStore, AuthState, DefaultUser, SessionStore};
    /// # fn f(sessions: Arc<dyn SessionStore>, store: Arc<dyn AccountStore<User = DefaultUser>>) {
    /// let kv = moso_kv::Kv::in_memory("shop").expect("an in-memory kv");
    /// let state = AuthState::new(sessions).accounts(store, KvLifecycleTokens::shared(kv));
    /// assert!(state.has_accounts());
    /// # }
    /// ```
    #[must_use]
    pub fn accounts(
        mut self,
        store: Arc<dyn AccountStore<User = DefaultUser>>,
        tokens: Arc<dyn crate::LifecycleTokens>,
    ) -> Self {
        let accounts = crate::Accounts::new(
            Arc::new(ErasedAccountStore(store)),
            tokens,
            Arc::clone(&self.sessions),
        )
        .policy(self.password_policy.clone());
        self.accounts = Some(Arc::new(accounts));
        self
    }

    /// Use this password policy rather than the default.
    ///
    /// Set it **before** [`accounts`](AuthState::accounts), which copies it into
    /// the lifecycle.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, PasswordPolicy, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// let mut policy = PasswordPolicy::default();
    /// policy.min_length = 16;
    /// assert_eq!(AuthState::new(store).password_policy(policy).password_rules().min_length, 16);
    /// ```
    #[must_use]
    pub fn password_policy(mut self, policy: crate::PasswordPolicy) -> Self {
        self.password_policy = policy;
        self
    }

    /// Use this session configuration rather than the default.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionConfig, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// let state = AuthState::new(store).session_config(SessionConfig::default());
    /// assert!(state.session_settings().cookie.http_only);
    /// ```
    #[must_use]
    pub fn session_config(mut self, config: crate::SessionConfig) -> Self {
        self.session_config = config;
        self
    }

    /// Add the relying party the passkey ceremonies run against.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore, WebAuthn};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// let relying_party = WebAuthn::new("example.com", "https://example.com", "Example");
    /// assert!(AuthState::new(store).webauthn(relying_party).has_webauthn());
    /// ```
    #[cfg(feature = "passkeys")]
    #[cfg_attr(docsrs, doc(cfg(feature = "passkeys")))]
    #[must_use]
    pub fn webauthn(mut self, relying_party: crate::WebAuthn) -> Self {
        self.webauthn = Some(Arc::new(relying_party));
        self
    }

    /// Add the signer whose public keys `/.well-known/jwks.json` publishes.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, Jwt, MemorySessionStore, SessionStore};
    /// # fn f(store: Arc<dyn SessionStore>, jwt: Jwt) {
    /// assert!(AuthState::new(store).jwt(jwt).has_jwt());
    /// # }
    /// ```
    #[must_use]
    pub fn jwt(mut self, jwt: crate::Jwt) -> Self {
        self.jwt = Some(Arc::new(jwt));
        self
    }

    /// Add the key-value store the magic-link and TOTP routes keep state in.
    ///
    /// Those two flows are the only ones with state that belongs to neither the
    /// session nor the account store, and [`moso_kv`] is where this workspace
    /// keeps that kind of state — the login throttle already does.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// let kv = moso_kv::Kv::in_memory("shop").expect("an in-memory kv");
    /// assert!(AuthState::new(store).kv(kv).kv_store().is_some());
    /// ```
    #[must_use]
    pub fn kv(mut self, kv: moso_kv::Kv) -> Self {
        self.kv = Some(kv);
        self
    }

    /// Hand every minted token to `sink`, which is what sends the email.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::routes::TokenSink;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// let sink: TokenSink = Arc::new(|_token| Box::pin(async {}));
    /// assert!(AuthState::new(store).token_sink(sink).has_token_sink());
    /// ```
    #[must_use]
    pub fn token_sink(mut self, sink: TokenSink) -> Self {
        self.tokens = Some(sink);
        self
    }

    /// The name an authenticator app shows for this application.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert_eq!(AuthState::new(store).issuer("Shop").issuer_name(), "Shop");
    /// ```
    #[must_use]
    pub fn issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = issuer.into();
        self
    }

    // ── what the handlers read ────────────────────────────────────────────

    /// Where sessions live.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// let state = AuthState::new(store);
    /// assert!(Arc::ptr_eq(state.session_store(), &state.clone().session_store().clone()));
    /// ```
    #[must_use]
    pub fn session_store(&self) -> &Arc<dyn crate::SessionStore> {
        &self.sessions
    }

    /// How sessions behave.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert!(AuthState::new(store).session_settings().track_devices);
    /// ```
    #[must_use]
    pub fn session_settings(&self) -> &crate::SessionConfig {
        &self.session_config
    }

    /// The login throttle, when one is configured.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert!(AuthState::new(store).login_throttle().is_none());
    /// ```
    #[must_use]
    pub fn login_throttle(&self) -> Option<&crate::LoginThrottle> {
        self.throttle.as_ref()
    }

    /// What a password must satisfy.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert!(AuthState::new(store).password_rules().breach_check);
    /// ```
    #[must_use]
    pub fn password_rules(&self) -> &crate::PasswordPolicy {
        &self.password_policy
    }

    /// Where API keys live, when a store is configured.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert!(AuthState::new(store).api_key_store().is_none());
    /// ```
    #[must_use]
    pub fn api_key_store(&self) -> Option<&Arc<dyn crate::ApiKeyStore>> {
        self.api_keys.as_ref()
    }

    /// Where passkeys live, when a store is configured.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert!(AuthState::new(store).passkey_store().is_none());
    /// ```
    #[cfg(feature = "passkeys")]
    #[cfg_attr(docsrs, doc(cfg(feature = "passkeys")))]
    #[must_use]
    pub fn passkey_store(&self) -> Option<&Arc<dyn crate::PasskeyStore>> {
        self.passkeys.as_ref()
    }

    /// Where refresh-token families live, when a store is configured.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert!(AuthState::new(store).refresh_store().is_none());
    /// ```
    #[must_use]
    pub fn refresh_store(&self) -> Option<&Arc<dyn crate::RefreshStore>> {
        self.refresh.as_ref()
    }

    /// The CAPTCHA verifier, when one is configured.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert!(AuthState::new(store).captcha_verifier().is_none());
    /// ```
    #[must_use]
    pub fn captcha_verifier(&self) -> Option<&Arc<dyn crate::CaptchaVerifier>> {
        self.captcha.as_ref()
    }

    /// Whether an account store was configured.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert!(!AuthState::new(store).has_accounts());
    /// ```
    #[must_use]
    pub fn has_accounts(&self) -> bool {
        self.accounts.is_some()
    }

    /// Whether a relying party was configured.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert!(!AuthState::new(store).has_webauthn());
    /// ```
    #[cfg(feature = "passkeys")]
    #[cfg_attr(docsrs, doc(cfg(feature = "passkeys")))]
    #[must_use]
    pub fn has_webauthn(&self) -> bool {
        self.webauthn.is_some()
    }

    /// Whether a signer was configured.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert!(!AuthState::new(store).has_jwt());
    /// ```
    #[must_use]
    pub fn has_jwt(&self) -> bool {
        self.jwt.is_some()
    }

    /// Whether a token sink was configured.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert!(!AuthState::new(store).has_token_sink());
    /// ```
    #[must_use]
    pub fn has_token_sink(&self) -> bool {
        self.tokens.is_some()
    }

    /// The key-value store, when one is configured.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert!(AuthState::new(store).kv_store().is_none());
    /// ```
    #[must_use]
    pub fn kv_store(&self) -> Option<&moso_kv::Kv> {
        self.kv.as_ref()
    }

    /// The name an authenticator app shows for this application.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthState, MemorySessionStore, SessionStore};
    /// let store = Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
    /// assert_eq!(AuthState::new(store).issuer_name(), "Moso");
    /// ```
    #[must_use]
    pub fn issuer_name(&self) -> &str {
        &self.issuer
    }

    /// The lifecycle flows, or the sentence that says how to configure them.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming [`AuthState::accounts`].
    pub(crate) fn require_accounts(&self) -> Result<&MountedAccounts> {
        self.accounts.as_deref().ok_or_else(|| {
            Error::Config(
                "this route needs an account store, and none was configured; help: \
                 `AuthState::accounts(store, tokens)`"
                    .into(),
            )
        })
    }

    /// The relying party, or the sentence that says how to configure it.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming [`AuthState::webauthn`].
    #[cfg(feature = "passkeys")]
    pub(crate) fn require_webauthn(&self) -> Result<&crate::WebAuthn> {
        self.webauthn.as_deref().ok_or_else(|| {
            Error::Config(
                "this route needs a WebAuthn relying party, and none was configured; help: \
                 `AuthState::webauthn(WebAuthn::new(rp_id, origin, rp_name))`"
                    .into(),
            )
        })
    }

    /// The signer, or the sentence that says how to configure it.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming [`AuthState::jwt`].
    pub(crate) fn require_jwt(&self) -> Result<&crate::Jwt> {
        self.jwt.as_deref().ok_or_else(|| {
            Error::Config(
                "this route needs a JWT signer, and none was configured; help: \
                 `AuthState::jwt(Jwt::issuer(config, kid, key)?)`"
                    .into(),
            )
        })
    }

    /// The signer, when one is configured, for a best-effort verifier that must
    /// not turn a missing signer into an error — the [`Principal`](crate::Principal)
    /// extractor, which only records what turned up.
    pub(crate) fn jwt_ref(&self) -> Option<&crate::Jwt> {
        self.jwt.as_deref()
    }

    /// The refresh-token store, or the sentence that says how to configure it.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming [`AuthState::refresh`].
    pub(crate) fn require_refresh(&self) -> Result<&Arc<dyn crate::RefreshStore>> {
        self.refresh.as_ref().ok_or_else(|| {
            Error::Config(
                "this route needs a refresh-token store, and none was configured; help: \
                 `AuthState::refresh(store)` — `MemoryRefreshStore` for one process, \
                 `store::TableRefreshStore` for more"
                    .into(),
            )
        })
    }

    /// The API-key store, or the sentence that says how to configure it.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming [`AuthState::api_keys`].
    pub(crate) fn require_api_keys(&self) -> Result<&Arc<dyn crate::ApiKeyStore>> {
        self.api_keys.as_ref().ok_or_else(|| {
            Error::Config(
                "this route needs an API-key store, and none was configured; help: \
                 `AuthState::api_keys(store)`"
                    .into(),
            )
        })
    }

    /// The passkey store, or the sentence that says how to configure it.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming [`AuthState::passkeys`].
    #[cfg(feature = "passkeys")]
    pub(crate) fn require_passkeys(&self) -> Result<&Arc<dyn crate::PasskeyStore>> {
        self.passkeys.as_ref().ok_or_else(|| {
            Error::Config(
                "this route needs a passkey store, and none was configured; help: \
                 `AuthState::passkeys(store)`"
                    .into(),
            )
        })
    }

    /// The key-value store, or the sentence that says how to configure it.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming [`AuthState::kv`].
    pub(crate) fn require_kv(&self) -> Result<&moso_kv::Kv> {
        self.kv.as_ref().ok_or_else(|| {
            Error::Config(
                "this route keeps state in a key-value store, and none was configured; help: \
                 `AuthState::kv(kv)`"
                    .into(),
            )
        })
    }

    /// Hand a minted token to whoever sends the email.
    ///
    /// With no sink configured this logs a warning and drops the token, which is
    /// the only honest thing to do: the alternative is a reset flow that appears
    /// to work and sends nothing.
    pub(crate) async fn deliver(&self, delivery: Delivery) {
        match self.tokens.as_ref() {
            Some(sink) => sink(delivery).await,
            None => tracing::warn!(
                target: "moso.auth",
                purpose = delivery.purpose().as_str(),
                "a token was minted and dropped because no delivery was configured; help: \
                 `AuthState::token_sink(sink)`"
            ),
        }
    }
}

impl core::fmt::Debug for AuthState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AuthState")
    }
}

/// The 503 every route in this module can produce.
///
/// Written once, applied per flow, because a store outage is the one failure
/// every one of these routes shares.
fn unavailable_response() -> ResponseSpec {
    ResponseSpec::problem("a store this route depends on could not be reached")
}

/// The 429 the throttled routes produce.
fn throttled_response() -> ResponseSpec {
    ResponseSpec::problem(
        "too many attempts from this address or against this identity; `Retry-After` says when \
         to come back",
    )
}

/// The 401 the authenticated routes produce.
fn unauthenticated_response() -> ResponseSpec {
    ResponseSpec::problem("no credentials, or credentials that are no longer valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every route the builder mounts, as `METHOD path`.
    fn mounted(router: &Router) -> Vec<String> {
        let mut paths: Vec<String> = router
            .describe()
            .iter()
            .map(|route| format!("{} {}", route.method.as_str().to_uppercase(), route.path))
            .collect();
        paths.sort();
        paths
    }

    // ── next ──────────────────────────────────────────────────────────────

    #[test]
    fn a_relative_path_on_this_origin_is_always_allowed() {
        validate_next("/dashboard", &[]).expect("a path on this origin");
        validate_next("/a/b?c=d#e", &[]).expect("a path keeps its query and fragment");
    }

    #[test]
    fn a_protocol_relative_url_is_refused_however_much_it_looks_like_a_path() {
        assert!(validate_next("//evil.example", &[]).is_err());
        assert!(validate_next("//evil.example/path", &[]).is_err());
    }

    #[test]
    fn an_absolute_url_outside_the_allowlist_is_refused() {
        assert!(validate_next("https://evil.example", &[]).is_err());
        assert!(validate_next("https://evil.example/welcome", &[]).is_err());

        let allowed = ["https://app.example.com".to_owned()];
        assert!(validate_next("https://evil.example", &allowed).is_err());
    }

    #[test]
    fn an_allowlisted_origin_is_accepted_whatever_path_follows_it() {
        let allowed = ["https://app.example.com".to_owned()];

        validate_next("https://app.example.com", &allowed).expect("the bare origin");
        validate_next("https://app.example.com/welcome?a=1", &allowed).expect("a page on it");
    }

    #[test]
    fn the_allowlist_compares_the_whole_origin_and_not_just_the_host() {
        let allowed = ["https://app.example.com".to_owned()];

        assert!(
            validate_next("http://app.example.com", &allowed).is_err(),
            "a different scheme is a different origin"
        );
        assert!(
            validate_next("https://app.example.com:8443", &allowed).is_err(),
            "a different port is a different origin"
        );
        assert!(
            validate_next("https://evil.app.example.com", &allowed).is_err(),
            "a subdomain is a different origin"
        );
    }

    #[test]
    fn a_backslash_is_refused_because_a_browser_reads_it_as_a_slash() {
        assert!(validate_next("\\\\evil.example", &[]).is_err());
        assert!(validate_next("/\\evil.example", &[]).is_err());
    }

    #[test]
    fn a_control_character_is_refused_because_a_browser_strips_it() {
        assert!(validate_next("/\tevil", &[]).is_err());
        assert!(validate_next("/\nevil", &[]).is_err());
        assert!(validate_next("/\u{0}evil", &[]).is_err());
        assert!(validate_next("/ evil", &[]).is_err());
    }

    #[test]
    fn a_target_that_decodes_into_another_origin_is_refused() {
        assert!(
            validate_next("/%2f%2fevil.example", &[]).is_err(),
            "a path that navigates to another origin"
        );
        assert!(validate_next("/%5c%5cevil.example", &[]).is_err());
    }

    #[test]
    fn an_empty_target_is_refused_rather_than_treated_as_the_root() {
        assert!(validate_next("", &[]).is_err());
    }

    #[test]
    fn a_refused_target_never_tells_a_client_which_rule_it_tripped() {
        let error = validate_next("https://evil.example", &[]).expect_err("refused");

        assert!(matches!(error.client_facing(), Error::InvalidCredentials));
    }

    // ── the allowlist builder ─────────────────────────────────────────────

    #[test]
    fn a_wildcard_allowlist_entry_is_recorded_and_refused_rather_than_dropped() {
        let refused = routes().redirect_allowlist(["https://*.example.com"]);

        let error = refused.validate().expect_err("a wildcard is refused");
        assert!(error.to_string().contains("wildcard"), "{error}");
        assert!(error.to_string().contains("*.example.com"), "{error}");
    }

    #[test]
    fn a_good_allowlist_entry_survives_alongside_a_refused_one() {
        let mixed = routes().redirect_allowlist(["https://app.example.com", "https://*.evil"]);

        assert_eq!(mixed.redirect_allowlist, ["https://app.example.com"]);
        assert_eq!(mixed.rejected.len(), 1);
    }

    #[test]
    #[should_panic(expected = "wildcard")]
    fn building_with_a_wildcard_allowlist_refuses_at_composition_time() {
        let _ = routes()
            .redirect_allowlist(["https://*.example.com"])
            .build();
    }

    // ── what gets mounted ─────────────────────────────────────────────────

    #[test]
    fn nothing_is_mounted_until_a_flag_asks_for_it() {
        let router = routes().build();

        assert!(router.is_empty(), "{:?}", mounted(&router));
    }

    #[test]
    fn the_password_flag_mounts_exactly_the_documented_routes_and_no_others() {
        let router = routes().password().build();

        assert_eq!(
            mounted(&router),
            [
                "GET /auth/me",
                "POST /auth/email/change",
                "POST /auth/email/change/confirm",
                "POST /auth/login",
                "POST /auth/logout",
                "POST /auth/logout-all",
                "POST /auth/password/change",
                "POST /auth/password/forgot",
                "POST /auth/password/reset",
                "POST /auth/register",
                "POST /auth/verify-email",
                "POST /auth/verify-email/resend",
            ]
        );
    }

    #[test]
    fn every_mounted_route_is_tagged_auth() {
        let builder = routes()
            .password()
            .sessions()
            .api_keys()
            .totp()
            .magic_link()
            .bearer()
            .jwks();
        #[cfg(feature = "passkeys")]
        let builder = builder.passkeys();
        let router = builder.build();

        assert!(!router.is_empty());
        for entry in router.entries() {
            assert!(
                entry.metadata.tags.iter().any(|tag| tag == AUTH_TAG),
                "{} {} is untagged",
                entry.method.as_str(),
                entry.path
            );
        }
    }

    #[test]
    fn the_bearer_token_route_is_throttled_but_the_refresh_route_is_not() {
        let router = routes().bearer().build();

        for entry in router.entries() {
            let documents_429 = entry
                .metadata
                .responses
                .iter()
                .any(|(status, _)| *status == 429);
            match entry.path.as_str() {
                "/auth/token" => assert!(
                    documents_429,
                    "the credential route takes an identity and must throttle"
                ),
                "/auth/refresh" => assert!(
                    !documents_429,
                    "the refresh token is 256 bits of entropy; there is nothing to throttle"
                ),
                other => panic!("unexpected bearer route {other}"),
            }
        }
    }

    #[test]
    fn the_credential_routes_document_the_429_the_throttle_produces() {
        let router = routes().password().build();

        for entry in router.entries() {
            let throttled = matches!(
                entry.path.as_str(),
                "/auth/login"
                    | "/auth/register"
                    | "/auth/password/forgot"
                    | "/auth/verify-email/resend"
            );
            let documents_429 = entry
                .metadata
                .responses
                .iter()
                .any(|(status, _)| *status == 429);
            assert_eq!(
                throttled, documents_429,
                "{} documents its 429 exactly when it can produce one",
                entry.path
            );
        }
    }

    #[test]
    fn the_jwks_document_is_served_from_the_root_and_not_from_under_auth() {
        let router = routes().jwks().build();

        assert_eq!(mounted(&router), ["GET /.well-known/jwks.json"]);
    }

    #[test]
    fn each_flag_mounts_only_its_own_flow() {
        assert_eq!(
            mounted(&routes().sessions().build()),
            [
                "DELETE /auth/sessions",
                "DELETE /auth/sessions/{handle}",
                "GET /auth/sessions",
                "POST /auth/sessions",
            ]
        );
        assert_eq!(
            mounted(&routes().totp().build()),
            [
                "POST /auth/totp/confirm",
                "POST /auth/totp/disable",
                "POST /auth/totp/setup",
            ]
        );
        assert_eq!(
            mounted(&routes().magic_link().build()),
            ["GET /auth/magic-link/{token}", "POST /auth/magic-link"]
        );
        assert_eq!(
            mounted(&routes().bearer().build()),
            ["POST /auth/refresh", "POST /auth/token"]
        );
        #[cfg(feature = "passkeys")]
        assert_eq!(
            mounted(&routes().passkeys().build()),
            [
                "POST /auth/passkeys/login/finish",
                "POST /auth/passkeys/login/start",
                "POST /auth/passkeys/register/finish",
                "POST /auth/passkeys/register/start",
            ]
        );
        assert_eq!(
            mounted(&routes().api_keys().build()),
            [
                "DELETE /auth/api-keys",
                "DELETE /auth/api-keys/{prefix}",
                "GET /auth/api-keys",
                "POST /auth/api-keys",
            ]
        );
    }

    #[test]
    fn every_mounted_path_sits_under_the_prefix_or_is_the_well_known_document() {
        let builder = routes()
            .password()
            .sessions()
            .api_keys()
            .totp()
            .magic_link()
            .jwks();
        #[cfg(feature = "passkeys")]
        let builder = builder.passkeys();
        let router = builder.build();

        for entry in router.entries() {
            assert!(
                entry.path.starts_with(AUTH_PREFIX) || entry.path.starts_with("/.well-known/"),
                "{} is neither under {AUTH_PREFIX} nor well-known",
                entry.path
            );
        }
    }

    #[test]
    fn no_route_conflicts_with_another_when_every_flow_is_mounted() {
        let builder = routes()
            .password()
            .sessions()
            .api_keys()
            .totp()
            .magic_link()
            .jwks();
        #[cfg(feature = "passkeys")]
        let builder = builder.passkeys();
        let router = builder.build();

        assert!(router.conflicts().is_empty());
    }

    // ── the DTOs ──────────────────────────────────────────────────────────

    #[test]
    fn the_acknowledgement_is_one_constant_sentence() {
        let first = AcknowledgedResponse::new();
        let second = AcknowledgedResponse::default();

        assert_eq!(
            serde_json::to_string(&first).expect("json"),
            serde_json::to_string(&second).expect("json")
        );
    }

    #[test]
    fn a_request_body_documents_the_constraints_its_field_types_carry() {
        use moso_schema::Schema as _;

        let mut generator = moso_schema::json_schema::SchemaGenerator::default();
        let node = RegisterRequest::json_schema(&mut generator);

        assert_eq!(node.required, ["email", "password"]);
        const { assert!(RegisterRequest::HAS_CONSTRAINTS) };
        assert_eq!(
            node.properties["password"].min_length,
            Some(u64::try_from(Password::MIN_LENGTH).expect("a small number"))
        );
    }

    #[test]
    fn an_opaque_ceremony_payload_documents_itself_as_unmodelled() {
        use moso_schema::Schema as _;

        let mut generator = moso_schema::json_schema::SchemaGenerator::default();
        let node = OpaqueJson::json_schema(&mut generator);

        assert!(node.is_any() || node.description.is_some());
        assert!(
            generator.definitions().is_empty(),
            "nothing named is registered"
        );
    }

    // ── the state ─────────────────────────────────────────────────────────

    #[test]
    fn a_state_prints_nothing_it_holds() {
        let state = AuthState::new(crate::MemorySessionStore::shared());

        assert_eq!(format!("{state:?}"), "AuthState");
    }

    #[test]
    fn a_route_whose_dependency_was_never_configured_says_which_builder_call_fixes_it() {
        let state = AuthState::new(crate::MemorySessionStore::shared());

        #[cfg_attr(not(feature = "passkeys"), allow(unused_mut))]
        let mut messages = vec![
            state.require_accounts().err().map(|e| e.to_string()),
            state.require_jwt().err().map(|e| e.to_string()),
            state.require_api_keys().err().map(|e| e.to_string()),
            state.require_kv().err().map(|e| e.to_string()),
        ];
        #[cfg(feature = "passkeys")]
        messages.extend([
            state.require_webauthn().err().map(|e| e.to_string()),
            state.require_passkeys().err().map(|e| e.to_string()),
        ]);

        for message in messages {
            let message = message.expect("nothing is configured");
            assert!(message.contains("help: `AuthState::"), "{message}");
        }
    }

    #[test]
    fn the_password_policy_reaches_the_lifecycle_flows_from_one_place() {
        let policy = crate::PasswordPolicy {
            min_length: 20,
            ..crate::PasswordPolicy::default()
        };

        let state = AuthState::new(crate::MemorySessionStore::shared()).password_policy(policy);

        assert_eq!(state.password_rules().min_length, 20);
    }
}
