//! The extractors: [`CurrentUser`], [`MaybeUser`], [`AuthSession`] and
//! [`Principal`], plus the CSRF guard.
//!
//! All four are `Dependency` impls, so they are memoised once per request and
//! contribute their security scheme and their 401 to the OpenAPI operation
//! automatically. An endpoint that takes a `CurrentUser` documents itself as
//! requiring authentication without the author saying anything.
//!
//! # What resolution actually does
//!
//! 1. Read the [`Session`] the [`SessionLayer`](crate::SessionLayer) put in the
//!    request's extensions. Nothing is loaded yet.
//! 2. [`Session::load`] — the one round trip, and the reason an endpoint that
//!    names none of these extractors costs none.
//! 3. Take the subject the session records, and load the principal through the
//!    registered [`UserStore`].
//! 4. Compare [`AuthUser::auth_hash`] in constant
//!    time. A mismatch means the password changed or the epoch was bumped, so
//!    the session is destroyed and the request is anonymous.
//! 5. Check [`is_active`](crate::AuthUser::is_active).
//!
//! Any failure is the same 401, whichever step it was.

use core::marker::PhantomData;

use moso_core::BoxFuture;
use moso_core::ctx::RequestCtx;
use moso_core::di::{Dependency, ProviderReq};
use moso_core::middleware::Guard;
use moso_openapi::OperationBuilder;
use moso_openapi::builder::{Param, ResponseSpec};
use moso_openapi::security::SecurityRequirement;
use serde::{Deserialize, Serialize};

use crate::{AuthUser, DefaultUser, Session, UserStore};

/// The name of the cookie security scheme in the OpenAPI document.
///
/// ```
/// assert_eq!(moso_auth::extract::SESSION_SCHEME, "session");
/// ```
pub const SESSION_SCHEME: &str = "session";

/// The name of the bearer-token security scheme in the OpenAPI document.
///
/// ```
/// assert_eq!(moso_auth::extract::BEARER_SCHEME, "bearer");
/// ```
pub const BEARER_SCHEME: &str = "bearer";

/// The name of the API-key security scheme in the OpenAPI document.
///
/// ```
/// assert_eq!(moso_auth::extract::API_KEY_SCHEME, "api_key");
/// ```
pub const API_KEY_SCHEME: &str = "api_key";

/// The authenticated principal. 401 when there is none.
///
/// ```text
/// #[endpoint]
/// async fn me(Depends(CurrentUser(user)): Depends<CurrentUser<User>>) -> Result<UserOut> {
///     Ok(user.into())
/// }
/// ```
///
/// Resolution is: read the session (or the bearer token, or the API key),
/// load the principal through the registered
/// [`UserStore`], compare
/// [`auth_hash`](crate::AuthUser::auth_hash), and check
/// [`is_active`](crate::AuthUser::is_active). Any failure is a 401 — the same
/// 401, whichever step failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentUser<U: AuthUser = DefaultUser>(pub U);

impl<U: AuthUser> CurrentUser<U> {
    /// The principal.
    ///
    /// ```
    /// use moso_auth::{CurrentUser, DefaultUser};
    ///
    /// let user = DefaultUser::new("usr_1", b"epoch".to_vec());
    /// assert_eq!(CurrentUser(user.clone()).into_inner(), user);
    /// ```
    #[must_use]
    pub fn into_inner(self) -> U {
        self.0
    }
}

impl<U: AuthUser> core::ops::Deref for CurrentUser<U> {
    type Target = U;

    fn deref(&self) -> &U {
        &self.0
    }
}

impl<U: AuthUser> Dependency for CurrentUser<U> {
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    fn describe(op: &mut OperationBuilder) {
        // Three requirements rather than one with three schemes: in OpenAPI a
        // list of requirements is a disjunction, and any one of the three
        // credentials is enough.
        op.security(SecurityRequirement::scheme(SESSION_SCHEME));
        op.security(SecurityRequirement::scheme(BEARER_SCHEME));
        op.security(SecurityRequirement::scheme(API_KEY_SCHEME));
        op.response(
            401,
            ResponseSpec::problem("no credentials, or credentials that are no longer valid"),
        );
    }

    async fn resolve(ctx: &RequestCtx) -> moso_core::Result<Self> {
        match resolve_user::<U>(ctx).await? {
            Some(user) => Ok(CurrentUser(user)),
            None => Err(unauthenticated()),
        }
    }
}

/// The authenticated principal, when there is one.
///
/// For an endpoint that behaves differently for a signed-in user without
/// requiring one — a public article that shows an edit button to its author.
/// Contributes an *optional* security requirement to the document, so the
/// generated client does not demand a token.
///
/// ```text
/// #[endpoint]
/// async fn article(Depends(MaybeUser(user)): Depends<MaybeUser<User>>) -> Result<ArticleOut> {
///     let can_edit = user.is_some_and(|u| u.id == article.author_id);
///     …
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaybeUser<U: AuthUser = DefaultUser>(pub Option<U>);

impl<U: AuthUser> MaybeUser<U> {
    /// The principal, when there is one.
    ///
    /// ```
    /// use moso_auth::{DefaultUser, MaybeUser};
    ///
    /// assert_eq!(MaybeUser::<DefaultUser>(None).into_inner(), None);
    /// ```
    #[must_use]
    pub fn into_inner(self) -> Option<U> {
        self.0
    }

    /// Whether anybody is authenticated.
    ///
    /// ```
    /// use moso_auth::{DefaultUser, MaybeUser};
    ///
    /// assert!(!MaybeUser::<DefaultUser>(None).is_authenticated());
    /// ```
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.0.is_some()
    }
}

impl<U: AuthUser> Dependency for MaybeUser<U> {
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    fn describe(op: &mut OperationBuilder) {
        // The empty requirement is what says "and unauthenticated is fine",
        // which is what stops a generated client demanding a token.
        op.security(SecurityRequirement::none());
        op.security(SecurityRequirement::scheme(SESSION_SCHEME));
        op.security(SecurityRequirement::scheme(BEARER_SCHEME));
        op.security(SecurityRequirement::scheme(API_KEY_SCHEME));
    }

    async fn resolve(ctx: &RequestCtx) -> moso_core::Result<Self> {
        Ok(MaybeUser(resolve_user::<U>(ctx).await?))
    }
}

/// Load the principal this request is acting as, if any.
///
/// The whole of the session half of authentication, in one place, so
/// [`CurrentUser`] and [`MaybeUser`] cannot disagree about what "authenticated"
/// means.
async fn resolve_user<U: AuthUser>(ctx: &RequestCtx) -> moso_core::Result<Option<U>> {
    let Some(session) = ctx.extension::<Session>() else {
        // No session layer installed. Not an error: an application may
        // authenticate only by token, and the token extractors are elsewhere.
        return Ok(None);
    };

    session.load().await.map_err(to_core)?;

    let Some(subject) = session.user_id() else {
        return Ok(None);
    };

    let Ok(id) = crate::session::decode_subject::<U::Id>(&subject) else {
        // The user key's type changed under a live session. Dropping the
        // session is the only safe reading: the alternative is authenticating
        // whichever account the old text happens to name now.
        drop_session(&session).await;
        return Ok(None);
    };

    let Some(store) = ctx.try_provider::<dyn UserStore<U>>() else {
        return Err(moso_core::Error::internal_msg(
            "no `UserStore` is registered, so `CurrentUser` cannot load anybody; help: register \
             the auth backend with `.provide_dyn::<dyn UserStore<User>>(backend)` at boot",
        ));
    };

    let Some(user) = store.load_user(&id).await.map_err(to_core)? else {
        // The account was deleted while the session lived.
        drop_session(&session).await;
        return Ok(None);
    };

    let recorded = session.auth_hash().unwrap_or_default();
    if !crate::password::constant_time_eq(&recorded, &user.auth_hash()) {
        // The password changed, or "log out everywhere" bumped the epoch. This
        // comparison is the whole of that feature: no scan, no fan-out.
        tracing::debug!(
            target: "moso.auth",
            "the session's auth hash no longer matches the principal's; dropping it"
        );
        drop_session(&session).await;
        return Ok(None);
    }

    if !user.is_active() {
        drop_session(&session).await;
        return Ok(None);
    }

    Ok(Some(user))
}

/// End a session that turned out not to authenticate anybody.
///
/// A failure here is logged and not propagated: the request is already going to
/// be anonymous, and turning "we could not tidy up" into a 503 would make a
/// store blip look like an outage on every public page.
async fn drop_session(session: &Session) {
    if let Err(error) = session.destroy().await {
        tracing::warn!(target: "moso.auth", %error, "could not drop a stale session");
    }
}

/// The 401 every failed resolution produces, whichever step failed.
fn unauthenticated() -> moso_core::Error {
    moso_core::Error::unauthenticated()
}

/// Turn an authentication failure into the HTTP problem it means.
///
/// Only the two outcomes an extractor can produce are distinguished: a store
/// that could not be reached is a 503, and everything else is the same 401. In
/// particular a missing session, an expired one and a revoked one are one
/// answer, because the difference between them is a user-enumeration oracle.
fn to_core(error: crate::Error) -> moso_core::Error {
    match error {
        crate::Error::Unavailable { component, .. } => {
            moso_core::Error::unavailable(format!("{component} is unavailable"))
        }
        crate::Error::Config(detail) => moso_core::Error::internal_msg(detail),
        _ => unauthenticated(),
    }
}

/// The request's session, whether or not anybody is authenticated.
///
/// A pre-login session is a real session: it holds the OAuth `state`, the PKCE
/// verifier and the CSRF token. Resolving this **does** load the session — an
/// endpoint that asks for the session is reading the session — and an endpoint
/// that does not name it costs nothing.
///
/// ```text
/// #[endpoint]
/// async fn set_locale(
///     Depends(AuthSession(session)): Depends<AuthSession>,
///     Json(body): Json<SetLocale>,
/// ) -> Result<NoContent> {
///     session.insert("locale", body.locale)?;
///     Ok(NoContent)
/// }
/// ```
#[derive(Clone, Debug)]
pub struct AuthSession(pub Session);

impl AuthSession {
    /// The session.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_auth::{AuthSession, Session, SessionConfig};
    /// # use moso_auth::store::MemorySessionStore;
    /// let session = Session::detached(MemorySessionStore::shared(), SessionConfig::default());
    /// let _ = AuthSession(session).into_inner();
    /// ```
    #[must_use]
    pub fn into_inner(self) -> Session {
        self.0
    }
}

impl core::ops::Deref for AuthSession {
    type Target = Session;

    fn deref(&self) -> &Session {
        &self.0
    }
}

impl Dependency for AuthSession {
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    fn describe(op: &mut OperationBuilder) {
        // A session is not a credential requirement: an anonymous request has
        // one too. Nothing is contributed, deliberately.
        let _ = op;
    }

    async fn resolve(ctx: &RequestCtx) -> moso_core::Result<Self> {
        {
            let session = ctx.extension::<Session>().ok_or_else(|| {
                moso_core::Error::internal_msg(
                    "no session in the request extensions; help: install `SessionLayer` in \
                     `Slot::Session` — `AuthSession` cannot invent one, because the cookie it \
                     would have to read has already gone past",
                )
            })?;

            session.load().await.map_err(to_core)?;
            Ok(AuthSession(session))
        }
    }
}

/// What kind of credential authenticated this request.
///
/// ```
/// use moso_auth::PrincipalKind;
///
/// assert!(!PrincipalKind::Anonymous.is_authenticated());
/// assert_eq!(PrincipalKind::ApiKey.as_str(), "api_key");
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PrincipalKind {
    /// Nothing was presented.
    #[default]
    Anonymous,
    /// A session cookie.
    Session,
    /// A bearer token.
    Token,
    /// An API key.
    ApiKey,
    /// Another service, over mutual TLS or a service token.
    Service,
}

impl PrincipalKind {
    /// Whether anything authenticated.
    ///
    /// ```
    /// use moso_auth::PrincipalKind;
    ///
    /// assert!(PrincipalKind::Session.is_authenticated());
    /// ```
    #[must_use]
    pub const fn is_authenticated(self) -> bool {
        !matches!(self, Self::Anonymous)
    }

    /// The name used in audit records and log fields.
    ///
    /// ```
    /// use moso_auth::PrincipalKind;
    ///
    /// assert_eq!(PrincipalKind::Token.as_str(), "token");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::Session => "session",
            Self::Token => "token",
            Self::ApiKey => "api_key",
            Self::Service => "service",
        }
    }
}

/// Who is acting, in the flattest possible form. For audit logging.
///
/// Not generic, so it can be recorded by middleware that knows nothing about the
/// application's user type — and so that an audit record's shape does not change
/// when the user type does.
///
/// ```text
/// #[endpoint]
/// async fn delete(Depends(principal): Depends<Principal>) -> Result<NoContent> {
///     audit.record(principal.subject(), "delete", …);
///     …
/// }
/// ```
///
/// # How the token and API-key kinds get here
///
/// Resolution reads a `Principal` out of the request's extensions first. That
/// is where a bearer-token or API-key layer puts what it authenticated, and it
/// is why this type is not generic and does not consult the
/// [`UserStore`]: an audit record must be cheap enough to
/// take on every request. Only if no layer left one does it fall back to the
/// session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Principal {
    /// What kind of credential.
    pub kind: PrincipalKind,
    /// Who, as a string. `None` when anonymous.
    pub subject: Option<String>,
    /// Which credential, when it has a stable identifier — an API key's prefix,
    /// a session's id hash. Never the credential itself.
    pub credential: Option<String>,
    /// The scopes the credential carries, as permission wire names. Empty for a
    /// session, which carries the user's full set.
    pub scopes: Vec<String>,
}

impl Principal {
    /// The anonymous principal.
    ///
    /// ```
    /// use moso_auth::Principal;
    ///
    /// assert!(Principal::anonymous().subject.is_none());
    /// ```
    #[must_use]
    pub fn anonymous() -> Self {
        Self {
            kind: PrincipalKind::Anonymous,
            subject: None,
            credential: None,
            scopes: Vec::new(),
        }
    }

    /// A principal that authenticated by session cookie.
    ///
    /// ```
    /// use moso_auth::{Principal, PrincipalKind};
    ///
    /// let principal = Principal::session("usr_1");
    /// assert_eq!(principal.kind, PrincipalKind::Session);
    /// assert_eq!(principal.subject(), Some("usr_1"));
    /// ```
    #[must_use]
    pub fn session(subject: impl Into<String>) -> Self {
        Self {
            kind: PrincipalKind::Session,
            subject: Some(subject.into()),
            credential: None,
            scopes: Vec::new(),
        }
    }

    /// A principal that authenticated by bearer token.
    ///
    /// The subject is the token's `sub` claim; the scopes are whatever the token
    /// carries. There is no stable credential identifier — a JWT names no row —
    /// so [`credential`](Principal::credential) stays `None`.
    ///
    /// ```
    /// use moso_auth::{Principal, PrincipalKind};
    ///
    /// let principal = Principal::token("usr_1", ["posts:read"]);
    /// assert_eq!(principal.kind, PrincipalKind::Token);
    /// assert_eq!(principal.subject(), Some("usr_1"));
    /// assert_eq!(principal.scopes, ["posts:read"]);
    /// ```
    #[must_use]
    pub fn token<S: Into<String>>(
        subject: impl Into<String>,
        scopes: impl IntoIterator<Item = S>,
    ) -> Self {
        Self {
            kind: PrincipalKind::Token,
            subject: Some(subject.into()),
            credential: None,
            scopes: scopes.into_iter().map(Into::into).collect(),
        }
    }

    /// A principal that authenticated by API key.
    ///
    /// The subject is the key's owner; the credential is the key's public prefix
    /// — never the secret — so an audit record can name the key without being a
    /// list of live credentials.
    ///
    /// ```
    /// use moso_auth::{Principal, PrincipalKind};
    ///
    /// let principal = Principal::api_key("usr_1", "0123abcd", ["deploy"]);
    /// assert_eq!(principal.kind, PrincipalKind::ApiKey);
    /// assert_eq!(principal.credential.as_deref(), Some("0123abcd"));
    /// ```
    #[must_use]
    pub fn api_key<S: Into<String>>(
        owner: impl Into<String>,
        prefix: impl Into<String>,
        scopes: impl IntoIterator<Item = S>,
    ) -> Self {
        Self {
            kind: PrincipalKind::ApiKey,
            subject: Some(owner.into()),
            credential: Some(prefix.into()),
            scopes: scopes.into_iter().map(Into::into).collect(),
        }
    }

    /// Who is acting.
    ///
    /// ```
    /// use moso_auth::Principal;
    ///
    /// assert_eq!(Principal::anonymous().subject(), None);
    /// ```
    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    /// Whether anything authenticated.
    ///
    /// ```
    /// use moso_auth::Principal;
    ///
    /// assert!(!Principal::anonymous().is_authenticated());
    /// ```
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.kind.is_authenticated()
    }
}

impl Dependency for Principal {
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    fn describe(op: &mut OperationBuilder) {
        // Every scheme, all optional: `Principal` never refuses a request, it
        // only records what turned up.
        op.security(SecurityRequirement::none());
        op.security(SecurityRequirement::scheme(SESSION_SCHEME));
        op.security(SecurityRequirement::scheme(BEARER_SCHEME));
        op.security(SecurityRequirement::scheme(API_KEY_SCHEME));
    }

    async fn resolve(ctx: &RequestCtx) -> moso_core::Result<Self> {
        // Whatever a token or API-key layer already established wins: an
        // application's own middleware may know things — a tenant, a device —
        // that this extractor cannot reproduce.
        if let Some(principal) = ctx.extension::<Principal>() {
            return Ok(principal);
        }

        // A presented bearer credential — an API key or an access token — is the
        // next most specific thing. Resolved here rather than by a global layer
        // so `PrincipalKind::Token` and `ApiKey` can occur without an
        // application writing one, and so an endpoint that names no auth
        // extractor still pays nothing.
        if let Some(principal) = resolve_bearer(ctx).await {
            return Ok(principal);
        }

        let Some(session) = ctx.extension::<Session>() else {
            return Ok(Principal::anonymous());
        };

        session.load().await.map_err(to_core)?;

        Ok(match session.user_id() {
            Some(subject) => Principal::session(subject),
            None => Principal::anonymous(),
        })
    }
}

/// Turn a presented `Authorization: Bearer …` into a [`Principal`], if it checks
/// out against the configured stores.
///
/// Best-effort by contract: a `Principal` never refuses a request, it only
/// records what turned up, so every failure here — no [`AuthState`] registered,
/// no credential presented, a credential that does not verify — returns `None`
/// and lets resolution fall through to the session, then to anonymous. A route
/// that must reject an invalid token does so with [`CurrentUser`] or a
/// [`RequireKind`] guard, which turn "nobody authenticated" into a 401.
///
/// The two bearer shapes are told apart by [`ApiKey::parse`](crate::ApiKey::parse):
/// an `mso_…` value is an API key and is authenticated against the
/// [`ApiKeyStore`](crate::ApiKeyStore); anything else is tried as a signed
/// access token against the configured [`Jwt`](crate::Jwt). Neither path is run
/// against the other's verifier, so a JWT is never fed to a constant-time secret
/// comparison and an API key is never fed to a signature check.
async fn resolve_bearer(ctx: &RequestCtx) -> Option<Principal> {
    let state = ctx.try_provider::<crate::AuthState>()?;
    let presented = bearer_of(ctx.headers())?;

    if crate::ApiKey::parse(&presented).is_ok() {
        let store = state.api_key_store()?;
        let authenticator = crate::ApiKeyAuthenticator::new(std::sync::Arc::clone(store));
        return match authenticator.authenticate(&presented).await {
            Ok(key) => Some(Principal::api_key(key.owner, key.prefix, key.scopes)),
            Err(_) => None,
        };
    }

    let jwt = state.jwt_ref()?;
    match jwt.verify(&presented) {
        Ok(claims) => {
            let scopes = scopes_of(&claims);
            Some(Principal::token(claims.sub, scopes))
        }
        Err(_) => None,
    }
}

/// The token from an `Authorization: Bearer <token>` header, if there is one.
///
/// The scheme is compared case-insensitively, per RFC 9110 § 11.1.
fn bearer_of(headers: &http::HeaderMap) -> Option<String> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let mut parts = value.splitn(2, ' ');
    match (parts.next(), parts.next()) {
        (Some(scheme), Some(token))
            if scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty() =>
        {
            Some(token.trim().to_owned())
        }
        _ => None,
    }
}

/// The scopes an access token carries, read from a `scope`/`scopes` claim.
///
/// OAuth 2.0 (RFC 6749 § 3.3) puts scopes in a single space-delimited `scope`
/// string; some issuers use a `scopes` array instead. Both are read, and a token
/// that carries neither has no scopes rather than an error — an unscoped token is
/// a token, not a malformed one.
fn scopes_of(claims: &crate::Claims) -> Vec<String> {
    if let Some(serde_json::Value::String(scope)) = claims.extra.get("scope") {
        return scope.split_whitespace().map(ToOwned::to_owned).collect();
    }
    if let Some(serde_json::Value::Array(items)) = claims.extra.get("scopes") {
        return items
            .iter()
            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
            .collect();
    }
    Vec::new()
}

/// Requires a specific kind of credential.
///
/// For an endpoint that must not be reachable with a cookie — a webhook
/// receiver, a service-to-service call — because a cookie-authenticated
/// endpoint is a CSRF target and a service endpoint has no business being one.
///
/// ```
/// use moso_auth::{PrincipalKind, RequireKind};
///
/// let guard = RequireKind::new([PrincipalKind::ApiKey, PrincipalKind::Service]);
/// assert!(guard.accepts(PrincipalKind::ApiKey));
/// assert!(!guard.accepts(PrincipalKind::Session));
/// ```
#[derive(Clone, Debug)]
pub struct RequireKind {
    /// Which kinds are acceptable.
    kinds: Vec<PrincipalKind>,
}

impl RequireKind {
    /// Accept only these kinds.
    ///
    /// ```
    /// use moso_auth::{PrincipalKind, RequireKind};
    ///
    /// let _ = RequireKind::new([PrincipalKind::Session]);
    /// ```
    #[must_use]
    pub fn new(kinds: impl IntoIterator<Item = PrincipalKind>) -> Self {
        Self {
            kinds: kinds.into_iter().collect(),
        }
    }

    /// Whether this kind of credential is one of the accepted ones.
    ///
    /// ```
    /// use moso_auth::{PrincipalKind, RequireKind};
    ///
    /// assert!(RequireKind::new([PrincipalKind::Token]).accepts(PrincipalKind::Token));
    /// ```
    #[must_use]
    pub fn accepts(&self, kind: PrincipalKind) -> bool {
        self.kinds.contains(&kind)
    }

    /// The scheme name each accepted kind documents itself as.
    fn schemes(&self) -> Vec<&'static str> {
        self.kinds
            .iter()
            .filter_map(|kind| match kind {
                PrincipalKind::Session => Some(SESSION_SCHEME),
                PrincipalKind::Token => Some(BEARER_SCHEME),
                PrincipalKind::ApiKey | PrincipalKind::Service => Some(API_KEY_SCHEME),
                PrincipalKind::Anonymous => None,
            })
            .collect()
    }
}

impl Guard for RequireKind {
    fn describe(&self, op: &mut OperationBuilder) {
        for scheme in self.schemes() {
            op.security(SecurityRequirement::scheme(scheme));
        }
        op.response(
            403,
            ResponseSpec::problem(format!(
                "this endpoint accepts only: {}",
                self.kinds
                    .iter()
                    .map(|kind| kind.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        );
    }

    fn check<'a>(
        &'a self,
        parts: &'a http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, moso_core::Result<()>> {
        let _ = parts;
        Box::pin(async move {
            let principal = ctx.depends::<Principal>().await?;

            if self.accepts(principal.kind) {
                return Ok(());
            }

            if !principal.is_authenticated() {
                return Err(unauthenticated());
            }

            Err(moso_core::Error::forbidden(format!(
                "this endpoint accepts only {}, and this request presented a {}",
                self.kinds
                    .iter()
                    .map(|kind| kind.as_str())
                    .collect::<Vec<_>>()
                    .join(" or "),
                principal.kind.as_str()
            )))
        })
    }
}

/// A CSRF token, double-submitted.
///
/// `SameSite=Lax` stops most cross-site requests, and "most" is not a security
/// property. The token closes the rest: it lives in the session and is echoed in
/// a header or a form field, and a cross-site attacker can do neither.
///
/// Applied automatically to non-idempotent requests that authenticated **by
/// cookie**. A request authenticated by a bearer token or an API key is not a
/// CSRF target — the browser does not attach those — so it is exempt, which
/// keeps the check off every machine-to-machine call.
///
/// ```
/// use moso_auth::{Csrf, CsrfConfig};
///
/// let guard = Csrf::new(CsrfConfig::default());
/// let _ = guard;
/// ```
#[derive(Clone, Debug)]
pub struct Csrf {
    /// How it behaves.
    config: CsrfConfig,
}

/// How many bytes of entropy a CSRF token carries.
///
/// ```
/// assert_eq!(moso_auth::extract::CSRF_TOKEN_BYTES, 32);
/// ```
pub const CSRF_TOKEN_BYTES: usize = 32;

impl Csrf {
    /// A guard with `config`.
    ///
    /// ```
    /// use moso_auth::{Csrf, CsrfConfig};
    ///
    /// let _ = Csrf::new(CsrfConfig::default());
    /// ```
    #[must_use]
    pub fn new(config: CsrfConfig) -> Self {
        Self { config }
    }

    /// The token for this session, minting one if there is none.
    ///
    /// What a template or a bootstrap endpoint hands to the client. The session
    /// must already be loaded — take `Depends<AuthSession>`.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) when the session has not been
    /// loaded.
    ///
    /// ```
    /// use moso_auth::store::MemorySessionStore;
    /// use moso_auth::{Csrf, CsrfConfig, Session, SessionConfig};
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso_auth::Result<()> {
    /// let session = Session::detached(MemorySessionStore::shared(), SessionConfig::default());
    /// session.load().await?;
    ///
    /// let csrf = Csrf::new(CsrfConfig::default());
    /// let token = csrf.token(&session)?;
    /// assert_eq!(csrf.token(&session)?, token, "minted once, then stable");
    /// # Ok(())
    /// # }
    /// ```
    pub fn token(&self, session: &Session) -> crate::Result<String> {
        if let Some(existing) = session.get::<String>(&self.config.session_key)? {
            return Ok(existing);
        }

        let mut entropy = [0_u8; CSRF_TOKEN_BYTES];
        getrandom::fill(&mut entropy).map_err(|error| crate::Error::Unavailable {
            component: "system random generator",
            detail: error.to_string(),
            source: None,
        })?;

        use base64::Engine as _;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(entropy);
        session.insert(&self.config.session_key, &token)?;
        Ok(token)
    }

    /// How this guard behaves.
    ///
    /// ```
    /// use moso_auth::{Csrf, CsrfConfig};
    ///
    /// assert_eq!(Csrf::new(CsrfConfig::default()).config().header, "x-csrf-token");
    /// ```
    #[must_use]
    pub fn config(&self) -> &CsrfConfig {
        &self.config
    }

    /// Whether a request of this shape needs the check at all.
    ///
    /// Idempotent methods are exempt because they do not change anything, and
    /// requests that presented a bearer token or an API key are exempt because
    /// a browser does not attach those to a cross-site request — the attack
    /// this guard exists to stop is not available against them.
    ///
    /// ```
    /// use moso_auth::{Csrf, CsrfConfig};
    ///
    /// let csrf = Csrf::new(CsrfConfig::default());
    /// let mut headers = http::HeaderMap::new();
    /// headers.insert(http::header::COOKIE, "__Host-id=x".parse().unwrap());
    ///
    /// assert!(csrf.applies(&http::Method::POST, &headers));
    /// assert!(!csrf.applies(&http::Method::GET, &headers));
    /// assert!(!csrf.applies(&http::Method::POST, &http::HeaderMap::new()));
    /// ```
    #[must_use]
    pub fn applies(&self, method: &http::Method, headers: &http::HeaderMap) -> bool {
        let unsafe_method = !matches!(
            *method,
            http::Method::GET | http::Method::HEAD | http::Method::OPTIONS | http::Method::TRACE
        );

        let by_cookie = headers.contains_key(http::header::COOKIE)
            && !headers.contains_key(http::header::AUTHORIZATION);

        unsafe_method && by_cookie
    }

    /// The token the request presented, from the header or the form field.
    ///
    /// The form field is read from the query string only: reading it from the
    /// body would mean buffering the body inside a guard, which runs before the
    /// handler has said how large a body it will accept.
    fn presented(&self, parts: &http::request::Parts) -> Option<String> {
        if let Some(value) = parts
            .headers
            .get(&self.config.header)
            .and_then(|value| value.to_str().ok())
        {
            return Some(value.to_owned());
        }

        let query = parts.uri.query()?;
        form_urlencoded_pairs(query)
            .into_iter()
            .find(|(key, _)| *key == self.config.field)
            .map(|(_, value)| value)
    }

    /// Whether the request's `Origin` or `Referer` agrees with its `Host`.
    ///
    /// A state-changing cross-origin request always carries one of the two, so
    /// their joint absence is itself a signal rather than a compatibility
    /// problem.
    fn origin_agrees(&self, parts: &http::request::Parts) -> bool {
        let host = parts
            .headers
            .get(http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .or_else(|| parts.uri.authority().map(|a| a.to_string()));

        let Some(host) = host else {
            return false;
        };

        let claimed = parts
            .headers
            .get(http::header::ORIGIN)
            .or_else(|| parts.headers.get(http::header::REFERER))
            .and_then(|value| value.to_str().ok());

        let Some(claimed) = claimed else {
            return false;
        };

        claimed
            .split("://")
            .nth(1)
            .map(|rest| rest.split('/').next().unwrap_or(rest))
            .is_some_and(|authority| authority == host)
    }
}

/// The pairs of a `application/x-www-form-urlencoded` string.
fn form_urlencoded_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (decode_component(key), decode_component(value)))
        .collect()
}

/// Percent-decode one form component, `+` included.
///
/// A `%` that is not followed by two hexadecimal digits is left alone rather
/// than swallowing what came after it: the token is compared in constant time
/// against the session's, and silently dropping characters would turn a
/// malformed token into a *different* token.
fn decode_component(value: &str) -> String {
    let bytes = value.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        let byte = bytes[index];
        let decoded = (byte == b'%' && index + 2 < bytes.len())
            .then(|| {
                let high = (bytes[index + 1] as char).to_digit(16)?;
                let low = (bytes[index + 2] as char).to_digit(16)?;
                Some((high * 16 + low) as u8)
            })
            .flatten();

        match decoded {
            Some(value) => {
                out.push(value);
                index += 3;
            }
            None => {
                out.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

impl Guard for Csrf {
    fn describe(&self, op: &mut OperationBuilder) {
        op.parameter(
            Param::header(self.config.header.clone())
                .required(false)
                .description(
                    "The double-submit CSRF token, required on cookie-authenticated \
                     state-changing requests. Read it from the session.",
                ),
        );
        op.response(
            403,
            ResponseSpec::problem("the CSRF token was missing or did not match the session's"),
        );
    }

    fn check<'a>(
        &'a self,
        parts: &'a http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, moso_core::Result<()>> {
        Box::pin(async move {
            if !self.applies(&parts.method, &parts.headers) {
                return Ok(());
            }

            if self.config.check_origin && !self.origin_agrees(parts) {
                return Err(moso_core::Error::forbidden(
                    "this request's Origin does not match its Host; every browser sends one on a \
                     cross-origin state-changing request, so its absence is itself a signal",
                ));
            }

            let Some(session) = ctx.extension::<Session>() else {
                // No session layer: nothing authenticated by cookie, so there
                // is no cross-site request to forge.
                return Ok(());
            };

            session.load().await.map_err(to_core)?;

            let Some(expected) = session
                .get::<String>(&self.config.session_key)
                .map_err(to_core)?
            else {
                return Err(moso_core::Error::forbidden(
                    "this session has no CSRF token; help: read one with `Csrf::token(&session)` \
                     and send it back in the configured header",
                ));
            };

            let Some(presented) = self.presented(parts) else {
                return Err(moso_core::Error::forbidden(format!(
                    "this request carried no CSRF token; help: send the session's token in the \
                     `{}` header",
                    self.config.header
                )));
            };

            if crate::password::constant_time_eq(expected.as_bytes(), presented.as_bytes()) {
                Ok(())
            } else {
                Err(moso_core::Error::forbidden(
                    "the CSRF token did not match this session's",
                ))
            }
        })
    }
}

/// How CSRF protection behaves.
///
/// ```
/// use moso_auth::CsrfConfig;
///
/// let config = CsrfConfig::default();
/// assert_eq!(config.header, "x-csrf-token");
/// assert_eq!(config.field, "csrf_token");
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CsrfConfig {
    /// The header the token may arrive in.
    pub header: String,
    /// The form field the token may arrive in, for a non-JavaScript form post.
    pub field: String,
    /// The session key the token is stored under.
    pub session_key: String,
    /// Whether to also require an `Origin` or `Referer` that matches.
    ///
    /// Belt and braces, on by default. A request with neither header is refused
    /// — every browser sends one on a cross-origin state-changing request, so
    /// their absence is itself a signal.
    pub check_origin: bool,
}

impl Default for CsrfConfig {
    fn default() -> Self {
        Self {
            header: "x-csrf-token".to_owned(),
            field: "csrf_token".to_owned(),
            session_key: "_csrf".to_owned(),
            check_origin: true,
        }
    }
}

/// A type-level marker for the user type the extractors default to.
///
/// `moso new --auth` generates `type User = crate::models::User;` and the
/// extractors are written `CurrentUser<User>`. This exists so a crate that
/// wants the default spelling can alias it once.
///
/// ```
/// use moso_auth::{DefaultUser, UserType};
///
/// let _: UserType<DefaultUser> = UserType::new();
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct UserType<U: AuthUser>(PhantomData<fn() -> U>);

impl<U: AuthUser> UserType<U> {
    /// The marker.
    ///
    /// ```
    /// use moso_auth::{DefaultUser, UserType};
    ///
    /// let _ = UserType::<DefaultUser>::new();
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::store::MemorySessionStore;
    use crate::{SessionConfig, SessionStore};

    fn session() -> Session {
        Session::detached(
            MemorySessionStore::shared() as Arc<dyn SessionStore>,
            SessionConfig::default(),
        )
    }

    fn parts(method: http::Method, uri: &str) -> http::request::Parts {
        let request = http::Request::builder()
            .method(method)
            .uri(uri)
            .body(())
            .unwrap();
        request.into_parts().0
    }

    #[test]
    fn a_principal_kind_names_itself_the_way_an_audit_record_wants() {
        assert_eq!(PrincipalKind::default(), PrincipalKind::Anonymous);
        assert!(!PrincipalKind::Anonymous.is_authenticated());

        for kind in [
            PrincipalKind::Session,
            PrincipalKind::Token,
            PrincipalKind::ApiKey,
            PrincipalKind::Service,
        ] {
            assert!(kind.is_authenticated());
            assert!(!kind.as_str().is_empty());
        }
    }

    #[test]
    fn a_principal_round_trips_through_json() {
        let principal = Principal::session("usr_1");
        let json = serde_json::to_string(&principal).unwrap();
        assert!(json.contains("\"session\""));
        assert_eq!(serde_json::from_str::<Principal>(&json).unwrap(), principal);
    }

    #[test]
    fn require_kind_accepts_only_what_it_was_given() {
        let guard = RequireKind::new([PrincipalKind::ApiKey, PrincipalKind::Service]);

        assert!(guard.accepts(PrincipalKind::ApiKey));
        assert!(guard.accepts(PrincipalKind::Service));
        assert!(!guard.accepts(PrincipalKind::Session));
        assert!(!guard.accepts(PrincipalKind::Anonymous));

        assert_eq!(guard.schemes(), vec![API_KEY_SCHEME, API_KEY_SCHEME]);
    }

    #[tokio::test]
    async fn a_csrf_token_is_minted_once_and_then_stable() {
        let session = session();
        session.load().await.unwrap();

        let csrf = Csrf::new(CsrfConfig::default());
        let first = csrf.token(&session).unwrap();
        let second = csrf.token(&session).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.len(), 43, "256 bits, base64url");
        assert_eq!(
            session.get::<String>("_csrf").unwrap().as_deref(),
            Some(first.as_str())
        );
    }

    #[test]
    fn csrf_applies_only_to_cookie_authenticated_state_changing_requests() {
        let csrf = Csrf::new(CsrfConfig::default());

        let mut cookied = http::HeaderMap::new();
        cookied.insert(http::header::COOKIE, "__Host-id=x".parse().unwrap());

        let mut bearer = http::HeaderMap::new();
        bearer.insert(http::header::COOKIE, "__Host-id=x".parse().unwrap());
        bearer.insert(http::header::AUTHORIZATION, "Bearer x".parse().unwrap());

        assert!(csrf.applies(&http::Method::POST, &cookied));
        assert!(csrf.applies(&http::Method::DELETE, &cookied));
        assert!(csrf.applies(&http::Method::PATCH, &cookied));

        assert!(!csrf.applies(&http::Method::GET, &cookied), "idempotent");
        assert!(!csrf.applies(&http::Method::HEAD, &cookied));
        assert!(!csrf.applies(&http::Method::OPTIONS, &cookied));
        assert!(
            !csrf.applies(&http::Method::POST, &bearer),
            "a browser does not attach a bearer token cross-site"
        );
        assert!(
            !csrf.applies(&http::Method::POST, &http::HeaderMap::new()),
            "nothing authenticated by cookie, so nothing to forge"
        );
    }

    #[test]
    fn csrf_reads_the_token_from_the_header_or_the_query() {
        let csrf = Csrf::new(CsrfConfig::default());

        let mut from_header = parts(http::Method::POST, "/things");
        from_header
            .headers
            .insert("x-csrf-token", "abc123".parse().unwrap());
        assert_eq!(csrf.presented(&from_header).as_deref(), Some("abc123"));

        let from_query = parts(http::Method::POST, "/things?csrf_token=abc%2D123&x=1");
        assert_eq!(csrf.presented(&from_query).as_deref(), Some("abc-123"));

        assert_eq!(csrf.presented(&parts(http::Method::POST, "/things")), None);
    }

    #[test]
    fn csrf_compares_the_origin_against_the_host() {
        let csrf = Csrf::new(CsrfConfig::default());

        let mut same = parts(http::Method::POST, "/things");
        same.headers
            .insert(http::header::HOST, "app.example.com".parse().unwrap());
        same.headers.insert(
            http::header::ORIGIN,
            "https://app.example.com".parse().unwrap(),
        );
        assert!(csrf.origin_agrees(&same));

        let mut cross = parts(http::Method::POST, "/things");
        cross
            .headers
            .insert(http::header::HOST, "app.example.com".parse().unwrap());
        cross.headers.insert(
            http::header::ORIGIN,
            "https://evil.example".parse().unwrap(),
        );
        assert!(!csrf.origin_agrees(&cross));

        let mut referer = parts(http::Method::POST, "/things");
        referer
            .headers
            .insert(http::header::HOST, "app.example.com".parse().unwrap());
        referer.headers.insert(
            http::header::REFERER,
            "https://app.example.com/form".parse().unwrap(),
        );
        assert!(csrf.origin_agrees(&referer), "Referer is the fallback");

        let mut neither = parts(http::Method::POST, "/things");
        neither
            .headers
            .insert(http::header::HOST, "app.example.com".parse().unwrap());
        assert!(
            !csrf.origin_agrees(&neither),
            "their joint absence is itself a signal"
        );
    }

    #[test]
    fn form_components_are_decoded() {
        assert_eq!(decode_component("abc%2D123"), "abc-123");
        assert_eq!(decode_component("a+b"), "a b");
        assert_eq!(decode_component("plain"), "plain");
        assert_eq!(decode_component("%zz"), "%zz");
    }

    #[test]
    fn an_authentication_failure_becomes_the_http_problem_it_means() {
        let unavailable = to_core(crate::Error::Unavailable {
            component: "session store",
            detail: "connection refused".to_owned(),
            source: None,
        });
        assert_eq!(
            unavailable.kind().status(),
            http::StatusCode::SERVICE_UNAVAILABLE,
            "an unreachable store is not a logout"
        );

        for collapsed in [
            crate::Error::InvalidCredentials,
            crate::Error::Unauthenticated,
            crate::Error::Expired { kind: "session" },
            crate::Error::Revoked { kind: "session" },
        ] {
            assert_eq!(
                to_core(collapsed).kind().status(),
                http::StatusCode::UNAUTHORIZED,
                "every credential failure is the same 401"
            );
        }
    }

    #[test]
    fn the_user_type_marker_is_zero_sized() {
        assert_eq!(core::mem::size_of::<UserType<DefaultUser>>(), 0);
        let _: UserType<DefaultUser> = UserType::new();
    }

    #[test]
    fn current_user_derefs_to_the_principal() {
        let user = DefaultUser::new("usr_1", b"epoch".to_vec());
        let current = CurrentUser(user.clone());

        assert_eq!(current.auth_id(), "usr_1");
        assert_eq!(current.into_inner(), user);
    }

    #[test]
    fn maybe_user_says_whether_anybody_signed_in() {
        assert!(!MaybeUser::<DefaultUser>(None).is_authenticated());
        assert!(MaybeUser(Some(DefaultUser::new("usr_1", b"e".to_vec()))).is_authenticated());
    }

    // ── the bearer principals ─────────────────────────────────────────────

    #[test]
    fn the_token_principal_carries_its_subject_and_scopes_and_no_credential() {
        let principal = Principal::token("usr_9", ["posts:read", "posts:write"]);

        assert_eq!(principal.kind, PrincipalKind::Token);
        assert!(principal.is_authenticated());
        assert_eq!(principal.subject(), Some("usr_9"));
        assert_eq!(principal.credential, None, "a JWT names no stored row");
        assert_eq!(principal.scopes, ["posts:read", "posts:write"]);
    }

    #[test]
    fn the_api_key_principal_records_the_prefix_never_the_secret() {
        let principal = Principal::api_key("usr_9", "0123abcd", ["deploy"]);

        assert_eq!(principal.kind, PrincipalKind::ApiKey);
        assert_eq!(
            principal.subject(),
            Some("usr_9"),
            "the owner is the subject"
        );
        assert_eq!(
            principal.credential.as_deref(),
            Some("0123abcd"),
            "the public prefix, so an audit can name the key"
        );
        assert_eq!(principal.scopes, ["deploy"]);
    }

    #[test]
    fn a_bearer_header_yields_its_token_and_the_scheme_is_case_insensitive() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            "Bearer  abc.def.ghi ".parse().unwrap(),
        );
        assert_eq!(bearer_of(&headers).as_deref(), Some("abc.def.ghi"));

        headers.insert(http::header::AUTHORIZATION, "bearer token".parse().unwrap());
        assert_eq!(bearer_of(&headers).as_deref(), Some("token"));
    }

    #[test]
    fn a_non_bearer_or_empty_authorization_yields_nothing() {
        let mut headers = http::HeaderMap::new();
        assert_eq!(bearer_of(&headers), None, "no header at all");

        headers.insert(
            http::header::AUTHORIZATION,
            "Basic dXNlcjpwYXNz".parse().unwrap(),
        );
        assert_eq!(bearer_of(&headers), None, "a different scheme");

        headers.insert(http::header::AUTHORIZATION, "Bearer   ".parse().unwrap());
        assert_eq!(bearer_of(&headers), None, "the scheme with no token");
    }

    #[test]
    fn scopes_are_read_from_either_the_string_or_the_array_claim() {
        let space = crate::Claims::new("usr_1").with_claim("scope", serde_json::json!("a b c"));
        assert_eq!(scopes_of(&space), ["a", "b", "c"]);

        let array = crate::Claims::new("usr_1").with_claim("scopes", serde_json::json!(["a", "b"]));
        assert_eq!(scopes_of(&array), ["a", "b"]);

        assert!(
            scopes_of(&crate::Claims::new("usr_1")).is_empty(),
            "unscoped is not malformed"
        );
    }
}
