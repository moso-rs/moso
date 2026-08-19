//! `Cookies` — reading and setting cookies, including signed and private ones.
//!
//! ```
//! use moso::prelude::*;
//! use moso::extract::{Cookie, Cookies};
//!
//! /// What the reader has chosen.
//! #[derive(Schema)]
//! pub struct Prefs {
//!     /// Light or dark.
//!     pub theme: Option<String>,
//! }
//!
//! /// Read the reader's preferences, and remember that they visited.
//! #[endpoint]
//! async fn show(cookies: Cookies) -> Result<Json<Prefs>> {
//!     let theme = cookies.get("theme").map(|c| c.value().to_owned());
//!     cookies.add(Cookie::new("seen", "1"));
//!     Ok(Json(Prefs { theme }))
//! }
//! # fn main() { assert_eq!(Router::new().get("/prefs", moso::ep!(show)).len(), 1); }
//! ```
//!
//! # How the jar reaches the response
//!
//! Extractors run inside the handler adapter and cannot touch the response, so
//! [`Cookies`] records its changes in a shared jar which the adapter drains into
//! `Set-Cookie` headers after the handler returns. Adding a cookie from a
//! dependency, a guard and the handler itself therefore all work, and all end
//! up on the same response.
//!
//! ```text
//! RouteHandler::call
//!   ├─ RequestCtx::new                     the jar's one home, still empty
//!   ├─ guard.check(&parts, &ctx)           ctx.cookies() ─┐
//!   ├─ handler                                            ├─ one OnceLock<Cookies>
//!   │    └─ Cookies::extract(parts, ctx)   ctx.cookies() ─┘
//!   └─ ctx.cookies_if_used()               → Cookies::apply_to(response headers)
//! ```
//!
//! The jar lives in the [`RequestCtx`], behind a `OnceLock`, and every road to
//! it — the extractor, a guard, a dependency, [`RequestCtx::cookies`] itself —
//! goes through that one cell. There is no constructor an extractor could reach
//! that would produce a *second* jar for the same request, which matters because
//! a second jar is not a visible bug: it accepts every write and then throws
//! them away.
//!
//! A request that never mentions a cookie never initialises the cell, so the
//! adapter's check after the handler returns is a single atomic load against a
//! `None`. No jar is allocated, no header is parsed and no mutex is taken.
//!
//! Cookies written by a **guard that then rejects the request are still sent**.
//! A guard that clears a stale session before answering 401 is the reason: were
//! the write dropped, the browser would keep presenting a credential that can
//! never work again. The rule has no exceptions worth remembering — every
//! `Set-Cookie` recorded during a request reaches the response, whatever the
//! status.
//!
//! # What Moso fills in
//!
//! `cookie::Cookie` leaves every attribute absent unless the caller sets it, so
//! a bare `Cookie::new("seen", "1")` would reach the browser as a
//! script-readable, plain-HTTP, directory-scoped cookie. Every write through
//! this module fills in the attributes the caller did not mention; see
//! [`CookieDefaults`], which also documents the escape hatch.
//!
//! # Signed and private cookies
//!
//! [`Cookies::signed`] gives a view whose values are authenticated with the
//! application's `secret_key`; [`Cookies::private`] gives one whose values are
//! additionally encrypted. Reading a tampered cookie yields `None` rather than
//! an error, which is the behaviour that keeps a rotated key from turning into
//! a wall of 500s.
//!
//! Both require a [`CookieKey`] provider, registered at boot from
//! `config.secret_key`. Without one, [`Cookies::signed`] and
//! [`Cookies::private`] return a view that **fails closed**: reads yield `None`
//! and writes are dropped, each with an `ERROR` log naming the missing
//! provider. [`Cookies::try_signed`] and [`Cookies::try_private`] are the same
//! views with the failure surfaced as a `Result`, for code that would rather
//! handle it than log it. Neither is meant to be reached: the intended failure
//! mode is a boot error, and the runtime behaviour exists so a missing
//! provider cannot become a silently-unsigned cookie.

use std::sync::{Arc, Mutex};

use moso_openapi::OperationBuilder;

use crate::config::Profile;
use crate::ctx::RequestCtx;
use crate::di::ProviderReq;
use crate::error::{Error, Result};
use crate::extract::Extract;

/// A single cookie. Re-exported from the `cookie` crate, which is the de-facto
/// model of the header and has no useful alternative.
pub use cookie::Cookie;

/// The `SameSite` attribute, re-exported so that overriding
/// [`CookieDefaults`]'s `Lax` does not require naming the `cookie` crate.
pub use cookie::SameSite;

// ---------------------------------------------------------------------------
// CookieDefaults
// ---------------------------------------------------------------------------

/// The attributes Moso writes into a cookie whose author left them unset.
///
/// `cookie::Cookie` defaults every attribute to *absent*: `Cookie::new("a", "b")`
/// renders as exactly `a=b`. A browser handed that makes the cookie readable
/// from JavaScript, sends it over plain HTTP, and scopes it to the directory of
/// the request that set it. None of those is the posture Moso states, and none
/// of them is visible in review — the mistake is a line that looks complete. So
/// every write through [`Cookies`] fills in what the caller did not say:
///
/// | Attribute | Filled in with | Because |
/// | --- | --- | --- |
/// | `HttpOnly` | on | a cookie a script can read is a cookie one XSS can steal |
/// | `SameSite` | `Lax` | the CSRF default current browsers assume anyway |
/// | `Path` | `/` | a directory-scoped cookie is almost never what was meant |
/// | `Secure` | on unless the profile is [`Profile::Dev`] | `http://localhost` never receives a `Secure` cookie, and a development server that cannot log in teaches people to turn security off |
///
/// **Unset** is the operative word, and it is the escape hatch. An attribute the
/// caller states is left exactly as stated, so the one-in-a-hundred cookie that
/// genuinely needs to be readable from JavaScript, or genuinely needs to travel
/// over plain HTTP in production behind a terminating proxy, says so out loud
/// and keeps saying so in the diff:
///
/// ```
/// use moso::extract::{Cookie, CookieDefaults, Cookies, CookieJar};
///
/// let cookies = Cookies::new(CookieJar::new());
///
/// // Said nothing: gets the defaults.
/// cookies.add(Cookie::new("seen", "1"));
///
/// // Said something: keeps it, in production too.
/// cookies.add(Cookie::build(("csrf", "t0ken")).http_only(false).secure(false).into());
///
/// let rendered: Vec<String> = cookies
///     .delta()
///     .iter()
///     .map(|value| value.to_str().expect("ascii").to_owned())
///     .collect();
///
/// let seen = rendered.iter().find(|line| line.starts_with("seen=")).expect("set");
/// assert!(seen.contains("HttpOnly") && seen.contains("SameSite=Lax") && seen.contains("Path=/"));
///
/// let csrf = rendered.iter().find(|line| line.starts_with("csrf=")).expect("set");
/// assert!(!csrf.contains("HttpOnly"), "an explicit `false` is honoured");
/// assert!(!csrf.contains("Secure"), "an explicit `false` is honoured");
/// ```
///
/// The profile-independent three are constants rather than fields on purpose:
/// making `HttpOnly` configurable turns "we disabled it once for a debugging
/// session" into a deployed default nobody re-reads.
///
/// One combination is decided below this layer: `cookie` renders `; Secure` for
/// any cookie carrying `SameSite=None`, whatever the profile, because a browser
/// rejects that pair without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CookieDefaults {
    /// Whether a cookie that does not mention `Secure` is marked `Secure`.
    ///
    /// The one attribute that has to vary, because a `Secure` cookie is simply
    /// discarded by a browser talking to `http://localhost`.
    pub secure: bool,
}

impl CookieDefaults {
    /// The `Path` given to a cookie that does not set one.
    pub const PATH: &'static str = "/";

    /// The `SameSite` given to a cookie that does not set one.
    pub const SAME_SITE: SameSite = SameSite::Lax;

    /// `Secure` on: what every profile but [`Profile::Dev`] uses, and what a
    /// [`Cookies`] built without an application assumes.
    pub const SECURE: Self = Self { secure: true };

    /// `Secure` left off, for a development server on `http://localhost`.
    pub const INSECURE: Self = Self { secure: false };

    /// The defaults a profile implies.
    ///
    /// [`Profile::Test`] is production-shaped, like every other profile-driven
    /// default in the framework, so only [`Profile::Dev`] relaxes `Secure`.
    ///
    /// ```
    /// use moso::config::Profile;
    /// use moso::extract::CookieDefaults;
    ///
    /// assert!(!CookieDefaults::for_profile(Profile::Dev).secure);
    /// assert!(CookieDefaults::for_profile(Profile::Test).secure);
    /// assert!(CookieDefaults::for_profile(Profile::Production).secure);
    /// ```
    #[must_use]
    pub const fn for_profile(profile: Profile) -> Self {
        Self {
            secure: !matches!(profile, Profile::Dev),
        }
    }

    /// `cookie` with everything its author left absent filled in.
    ///
    /// The one place the rule is written, so a plain, a signed and a private
    /// write cannot drift apart. Only ever widens: an attribute the caller set
    /// reads back as `Some` and is left alone, which is what makes an explicit
    /// `false` survive.
    fn filled_in(self, mut cookie: Cookie<'static>) -> Cookie<'static> {
        if cookie.http_only().is_none() {
            cookie.set_http_only(true);
        }
        if cookie.same_site().is_none() {
            cookie.set_same_site(Self::SAME_SITE);
        }
        if cookie.path().is_none() {
            cookie.set_path(Self::PATH);
        }
        if self.secure && cookie.secure().is_none() {
            cookie.set_secure(true);
        }
        cookie
    }
}

impl Default for CookieDefaults {
    /// [`CookieDefaults::SECURE`] — the safe answer for a jar with no
    /// application behind it to ask about the profile.
    fn default() -> Self {
        Self::SECURE
    }
}

// ---------------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------------

/// Access to the request's cookies, and to the ones the response will set.
///
/// Cheap to clone; every clone shares one jar.
///
/// Reads the request's `Cookie` header and collects the changes a handler makes,
/// which the handler adapter writes back as `Set-Cookie` on the way out. Cloning
/// a `Cookies` shares the same pending set, so a helper function can add a
/// cookie the handler will send.
///
/// ```
/// use moso::prelude::*;
/// use moso::extract::{Cookie, Cookies};
/// use moso::response::NoContent;
///
/// /// Remember that this reader visited.
/// #[endpoint]
/// async fn visit(cookies: Cookies) -> Result<NoContent> {
///     let theme = cookies.get("theme").map(|c| c.value().to_owned());
///     let _ = theme;
///     cookies.add(Cookie::new("seen", "1"));
///     Ok(NoContent)
/// }
/// # fn main() { assert_eq!(Router::new().get("/visit", moso::ep!(visit)).len(), 1); }
/// ```
///
/// Attributes the caller does not set are filled in from [`CookieDefaults`].
///
/// [`Cookies::signed`] and [`Cookies::private`] give tamper-evident and encrypted
/// jars; both need a `CookieKey` provider, and say so if one is missing.
#[derive(Debug, Clone)]
pub struct Cookies {
    jar: Arc<Mutex<CookieJar>>,
    key: Option<Arc<CookieKey>>,
    defaults: CookieDefaults,
}

impl Cookies {
    /// Build from an existing jar. Used by tests and by code driving a Moso
    /// extractor from outside a request.
    ///
    /// The resulting `Cookies` has no signing key, so [`Cookies::signed`] and
    /// [`Cookies::private`] fail closed, and it uses
    /// [`CookieDefaults::SECURE`] — there is no application to ask which
    /// profile is running, and the strict answer is the one that cannot leak.
    /// Use [`Cookies::with_defaults`] to say otherwise.
    pub fn new(jar: CookieJar) -> Self {
        Self {
            jar: Arc::new(Mutex::new(jar)),
            key: None,
            defaults: CookieDefaults::SECURE,
        }
    }

    /// Build from a jar and the application's signing key.
    pub fn with_key(jar: CookieJar, key: Arc<CookieKey>) -> Self {
        Self {
            jar: Arc::new(Mutex::new(jar)),
            key: Some(key),
            defaults: CookieDefaults::SECURE,
        }
    }

    /// The request's one jar, as [`RequestCtx::cookies`] creates it.
    ///
    /// Private because it is the *only* constructor a request may go through:
    /// `RequestCtx` holds the result in a `OnceLock`, and nothing else in the
    /// crate may make a second jar for a request that already has one.
    pub(crate) fn for_request(ctx: &RequestCtx) -> Self {
        Self {
            jar: Arc::new(Mutex::new(jar_from_headers(ctx.headers()))),
            key: ctx.try_provider::<CookieKey>(),
            defaults: CookieDefaults::for_profile(ctx.state().profile()),
        }
    }

    /// Use `defaults` for the attributes a caller leaves unset.
    #[must_use]
    pub fn with_defaults(mut self, defaults: CookieDefaults) -> Self {
        self.defaults = defaults;
        self
    }

    /// The attributes this jar fills in.
    #[must_use]
    pub fn defaults(&self) -> CookieDefaults {
        self.defaults
    }

    /// A cookie the client sent, or one already added to this response.
    pub fn get(&self, name: &str) -> Option<Cookie<'static>> {
        // The guard is taken, the value cloned out, and the guard dropped at
        // the end of this expression. It is never held across an `.await`,
        // which is what keeps a `std::sync::Mutex` correct here.
        self.with_jar(|jar| jar.get(name).cloned())
    }

    /// Set a cookie on the response.
    ///
    /// Attributes the caller did not set come from [`Cookies::defaults`].
    pub fn add(&self, cookie: Cookie<'static>) {
        let cookie = self.defaults.filled_in(cookie);
        self.with_jar_mut(|jar| jar.add(cookie));
    }

    /// Expire a cookie on the response.
    ///
    /// Always emits a removal `Set-Cookie`, even for a cookie the client did
    /// not present on *this* request. `cookie::CookieJar::remove` suppresses it
    /// in that case, and the suppression is wrong here: path and domain scoping
    /// mean "the browser did not send it" and "the browser does not hold it"
    /// are different questions, and a logout that silently sends nothing is the
    /// worst answer available.
    ///
    /// The removal carries the same [`Cookies::defaults`] as an `add`, because
    /// a browser only drops a cookie whose `Path` and `Domain` match.
    pub fn remove(&self, cookie: Cookie<'static>) {
        let cookie = self.defaults.filled_in(cookie);
        self.with_jar_mut(|jar| {
            jar.remove(cookie.clone());
            force_removal(jar, cookie);
        });
    }

    /// Every cookie currently in the jar.
    pub fn iter(&self) -> Vec<Cookie<'static>> {
        self.with_jar(|jar| jar.iter().cloned().collect())
    }

    /// A view whose values are signed with the application's `secret_key`.
    ///
    /// Reading a cookie whose signature does not verify yields `None`. Without
    /// a [`CookieKey`] provider the view fails closed; see the module header.
    pub fn signed(&self) -> SignedCookies {
        if self.key.is_none() {
            report_missing_key("signed");
        }
        SignedCookies(self.view())
    }

    /// A view whose values are encrypted and authenticated.
    ///
    /// Available only with the off-by-default `private-cookies` feature
    /// (RFC-0001); signed cookies ([`Cookies::signed`]) are always available.
    #[cfg(feature = "private-cookies")]
    pub fn private(&self) -> PrivateCookies {
        if self.key.is_none() {
            report_missing_key("private");
        }
        PrivateCookies(self.view())
    }

    /// [`Cookies::signed`] with the missing-key case as a `Result`.
    ///
    /// # Errors
    /// 500 when no [`CookieKey`] provider is registered.
    pub fn try_signed(&self) -> Result<SignedCookies> {
        self.checked_view("signed").map(SignedCookies)
    }

    /// [`Cookies::private`] with the missing-key case as a `Result`.
    ///
    /// # Errors
    /// 500 when no [`CookieKey`] provider is registered.
    #[cfg(feature = "private-cookies")]
    pub fn try_private(&self) -> Result<PrivateCookies> {
        self.checked_view("private").map(PrivateCookies)
    }

    /// The `Set-Cookie` headers this jar has accumulated.
    ///
    /// Rendered percent-encoded, one header value per pending cookie. The
    /// handler adapter calls this once per request through
    /// [`Cookies::apply_to`]; call it directly only when writing an adapter of
    /// your own.
    pub fn delta(&self) -> Vec<http::HeaderValue> {
        self.with_jar(|jar| {
            jar.delta()
                .filter_map(|cookie| {
                    http::HeaderValue::from_str(&cookie.encoded().to_string()).ok()
                })
                .collect()
        })
    }

    /// Append this jar's pending `Set-Cookie` values to `headers`.
    ///
    /// **Appends, never sets.** One response may legitimately carry several
    /// `Set-Cookie` headers, and a middleware that writes one of its own — as
    /// `moso-auth`'s session layer does — must not lose it to a handler that
    /// happened to touch the jar.
    ///
    /// A name `headers` already sets is left alone and the jar's value for it is
    /// dropped, with a `DEBUG` line naming the cookie. Emitting both would leave
    /// the outcome to header ordering, and the header already on the response is
    /// the more specific statement: the code that wrote it had the response in
    /// hand.
    ///
    /// ```
    /// use moso::deps::http::{HeaderMap, HeaderValue, header::SET_COOKIE};
    /// use moso::extract::{Cookie, CookieJar, Cookies};
    ///
    /// let cookies = Cookies::new(CookieJar::new());
    /// cookies.add(Cookie::new("seen", "1"));
    /// cookies.add(Cookie::new("theme", "dark"));
    ///
    /// let mut headers = HeaderMap::new();
    /// headers.append(SET_COOKIE, HeaderValue::from_static("theme=light; Path=/"));
    /// cookies.apply_to(&mut headers);
    ///
    /// let set: Vec<&str> = headers
    ///     .get_all(SET_COOKIE)
    ///     .iter()
    ///     .map(|value| value.to_str().expect("ascii"))
    ///     .collect();
    ///
    /// assert_eq!(set.len(), 2, "the jar's `theme` yields to the one already set");
    /// assert!(set.contains(&"theme=light; Path=/"));
    /// assert!(set.iter().any(|value| value.starts_with("seen=1")));
    /// ```
    pub fn apply_to(&self, headers: &mut http::HeaderMap) {
        let mut pending = self.delta();
        if pending.is_empty() {
            return;
        }
        if headers.contains_key(http::header::SET_COOKIE) {
            pending.retain(|value| {
                let Some(name) = set_cookie_name(value) else {
                    return true;
                };
                let clashes = headers
                    .get_all(http::header::SET_COOKIE)
                    .iter()
                    .filter_map(set_cookie_name)
                    .any(|existing| existing == name);
                if clashes {
                    tracing::debug!(
                        cookie = name,
                        "the response already sets this cookie; the jar's value is dropped"
                    );
                }
                !clashes
            });
        }
        for value in pending {
            headers.append(http::header::SET_COOKIE, value);
        }
    }

    /// The shared state a signed or private view needs.
    fn view(&self) -> CookieView {
        CookieView {
            jar: Arc::clone(&self.jar),
            key: self.key.clone(),
            defaults: self.defaults,
        }
    }

    /// [`Cookies::view`], refusing to build one without a key.
    fn checked_view(&self, view: &'static str) -> Result<CookieView> {
        let key = self.key.clone().ok_or_else(|| missing_key_error(view))?;
        Ok(CookieView {
            jar: Arc::clone(&self.jar),
            key: Some(key),
            defaults: self.defaults,
        })
    }

    /// Run `f` with the jar locked, returning its result.
    ///
    /// A poisoned lock means another task panicked while holding it. The jar is
    /// a plain container, so its contents are still coherent; recovering beats
    /// turning one panic into a panic on every subsequent request.
    fn with_jar<R>(&self, f: impl FnOnce(&CookieJar) -> R) -> R {
        with_jar(&self.jar, f)
    }

    fn with_jar_mut<R>(&self, f: impl FnOnce(&mut CookieJar) -> R) -> R {
        with_jar_mut(&self.jar, f)
    }
}

fn missing_key_error(view: &'static str) -> Error {
    Error::internal_msg(format!(
        "`Cookies::{view}` needs a `CookieKey` provider. Register one at boot with \
         `App::provide(CookieKey::derive(&config.secret_key)?)`"
    ))
}

fn report_missing_key(view: &'static str) {
    tracing::error!(
        view,
        "no `CookieKey` provider is registered: cookie reads yield `None` and writes are \
         dropped. Register one with `App::provide(CookieKey::derive(&config.secret_key)?)`"
    );
}

/// The cookie name a `Set-Cookie` value sets.
///
/// Everything before the first `=` of the first `;`-delimited part. The first
/// `=` is the right split even though a value may contain more of them —
/// base64 padding does — and an attribute may not appear before the pair.
/// `None` for a header value that is not ASCII, or that carries no `=` at all.
fn set_cookie_name(value: &http::HeaderValue) -> Option<&str> {
    let (name, _) = value.to_str().ok()?.split(';').next()?.split_once('=')?;
    Some(name.trim())
}

impl Extract for Cookies {
    const PROVIDER_REQ: &'static [ProviderReq] = &[ProviderReq::optional_of::<CookieKey>()];

    fn describe(op: &mut OperationBuilder) {
        // Deliberately nothing. A cookie an operation reads is either an
        // implementation detail or a security scheme, and a security scheme is
        // documented by the `Dependency` that authenticates with it. Listing
        // `Cookie` as a plain header parameter here would produce a document
        // that generates a client sending its session by hand.
        let _ = op;
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        // The head is read from the context's snapshot rather than from
        // `parts`, so that a guard — which only ever sees `&Parts` — reaches
        // the same jar this returns. Extracting `Cookies` twice, or extracting
        // it after a guard already wrote to it, is one jar either way.
        let _ = parts;
        Ok(ctx.cookies().clone())
    }
}

// ---------------------------------------------------------------------------
// CookieKey
// ---------------------------------------------------------------------------

/// The jar itself. Re-exported so a caller can build one for a test.
pub use cookie::CookieJar;

/// The master key signed and private cookies are derived from.
///
/// A newtype rather than a re-export of `cookie::Key`, because the signing and
/// encryption code lives behind that crate's `signed`/`private` features and
/// `moso-core` should not force them on an application that never sets a
/// cookie. The key material is derived from `config.secret_key` at boot.
///
/// `Debug` prints `CookieKey(***)`: a key that can be logged is not a key.
#[derive(Clone)]
pub struct CookieKey(Vec<u8>);

impl CookieKey {
    /// The number of bytes a master key must have. Shorter is rejected at boot
    /// rather than silently padded.
    pub const LEN: usize = 64;

    /// The shortest secret [`CookieKey::derive`] accepts.
    ///
    /// 32 bytes is the security level of the HMAC-SHA256 that signs with it;
    /// stretching anything shorter would produce a key that *looks* 64 bytes
    /// long and is not.
    pub const MIN_SECRET_LEN: usize = 32;

    /// Derive a key from the application's secret.
    ///
    /// HKDF-SHA256 expansion, so one `secret_key` yields independent signing
    /// and encryption keys.
    ///
    /// # Errors
    /// Returns a boot-shaped error when the secret is shorter than
    /// [`CookieKey::MIN_SECRET_LEN`], because a weak signing key is a security
    /// bug and silently accepting one is how it ships.
    pub fn derive(secret: &crate::config::SecretString) -> Result<Self> {
        let bytes = secret.expose().as_bytes();
        Self::derive_from_bytes(bytes).ok_or_else(|| {
            Error::internal_msg(format!(
                "`secret_key` is {} bytes; cookie signing needs at least {}. Generate one with \
                 `openssl rand -base64 48`",
                bytes.len(),
                Self::MIN_SECRET_LEN
            ))
        })
    }

    /// The length check and the expansion, without the error rendering.
    fn derive_from_bytes(secret: &[u8]) -> Option<Self> {
        if secret.len() < Self::MIN_SECRET_LEN {
            return None;
        }
        Some(Self(cookie::Key::derive_from(secret).master().to_vec()))
    }

    /// Wrap key material that is already [`CookieKey::LEN`] bytes long.
    ///
    /// # Errors
    /// Returns an error when `material` is the wrong length.
    pub fn from_bytes(material: &[u8]) -> Result<Self> {
        Self::try_from_material(material).ok_or_else(|| {
            Error::internal_msg(format!(
                "a cookie master key is {} bytes, not {}",
                Self::LEN,
                material.len()
            ))
        })
    }

    /// The length check, without the error rendering.
    fn try_from_material(material: &[u8]) -> Option<Self> {
        (material.len() == Self::LEN).then(|| Self(material.to_vec()))
    }

    /// A key generated from the operating system's randomness.
    ///
    /// For tests and for a single-process development server. A multi-process
    /// deployment must configure `secret_key`, or each process will reject the
    /// others' cookies.
    #[must_use]
    pub fn generate() -> Self {
        Self(cookie::Key::generate().master().to_vec())
    }

    /// The raw key material, for the signing implementation.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The `cookie` crate's key type.
    fn as_cookie_key(&self) -> cookie::Key {
        cookie::Key::from(&self.0)
    }
}

impl core::fmt::Debug for CookieKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("CookieKey(***)")
    }
}

// ---------------------------------------------------------------------------
// Signed and private views
// ---------------------------------------------------------------------------

/// What a signed or private view of a jar needs: the jar, the key, the
/// defaults.
///
/// One struct rather than three identical field lists, so that adding to the
/// state a view carries cannot leave one of the two views behind — which is how
/// "the signed jar forgot `Secure`" would happen.
#[derive(Debug, Clone)]
struct CookieView {
    jar: Arc<Mutex<CookieJar>>,
    key: Option<Arc<CookieKey>>,
    defaults: CookieDefaults,
}

impl CookieView {
    /// The key, or `None` after logging that the view is failing closed.
    fn key_or_report(&self, view: &'static str) -> Option<cookie::Key> {
        match self.key.as_ref() {
            Some(key) => Some(key.as_cookie_key()),
            None => {
                report_missing_key(view);
                None
            }
        }
    }
}

/// A signed view of a [`Cookies`] jar.
///
/// Values are authenticated with HMAC-SHA256 over the application's
/// `secret_key`: the client can read them but cannot forge them.
#[derive(Debug, Clone)]
pub struct SignedCookies(CookieView);

impl SignedCookies {
    /// A cookie whose signature verifies, or `None`.
    pub fn get(&self, name: &str) -> Option<Cookie<'static>> {
        let key = self.0.key.as_ref()?.as_cookie_key();
        with_jar(&self.0.jar, |jar| jar.signed(&key).get(name))
    }

    /// Set a signed cookie.
    ///
    /// Attributes the caller did not set come from [`Cookies::defaults`], the
    /// same as an unsigned [`Cookies::add`].
    pub fn add(&self, cookie: Cookie<'static>) {
        let Some(key) = self.0.key_or_report("signed") else {
            return;
        };
        let cookie = self.0.defaults.filled_in(cookie);
        with_jar_mut(&self.0.jar, |jar| jar.signed_mut(&key).add(cookie));
    }

    /// Expire a signed cookie.
    ///
    /// A removal is not signed — there is no value left to authenticate — so
    /// this is [`Cookies::remove`] with the same always-emit rule.
    pub fn remove(&self, cookie: Cookie<'static>) {
        let Some(key) = self.0.key_or_report("signed") else {
            return;
        };
        let cookie = self.0.defaults.filled_in(cookie);
        with_jar_mut(&self.0.jar, |jar| {
            jar.signed_mut(&key).remove(cookie.clone());
            force_removal(jar, cookie);
        });
    }
}

/// An encrypted view of a [`Cookies`] jar.
///
/// Values are opaque to the client and authenticated (AES-256-GCM), which is
/// what a session identifier or a flash message wants.
#[cfg(feature = "private-cookies")]
#[derive(Debug, Clone)]
pub struct PrivateCookies(CookieView);

#[cfg(feature = "private-cookies")]
impl PrivateCookies {
    /// A cookie that decrypts and authenticates, or `None`.
    pub fn get(&self, name: &str) -> Option<Cookie<'static>> {
        let key = self.0.key.as_ref()?.as_cookie_key();
        with_jar(&self.0.jar, |jar| jar.private(&key).get(name))
    }

    /// Set an encrypted cookie.
    ///
    /// Attributes the caller did not set come from [`Cookies::defaults`], the
    /// same as an unencrypted [`Cookies::add`].
    pub fn add(&self, cookie: Cookie<'static>) {
        let Some(key) = self.0.key_or_report("private") else {
            return;
        };
        let cookie = self.0.defaults.filled_in(cookie);
        with_jar_mut(&self.0.jar, |jar| jar.private_mut(&key).add(cookie));
    }

    /// Expire an encrypted cookie.
    ///
    /// As [`SignedCookies::remove`]: a removal carries no value to encrypt.
    pub fn remove(&self, cookie: Cookie<'static>) {
        let Some(key) = self.0.key_or_report("private") else {
            return;
        };
        let cookie = self.0.defaults.filled_in(cookie);
        with_jar_mut(&self.0.jar, |jar| {
            jar.private_mut(&key).remove(cookie.clone());
            force_removal(jar, cookie);
        });
    }
}

// ---------------------------------------------------------------------------
// Jar helpers
// ---------------------------------------------------------------------------

fn with_jar<R>(jar: &Mutex<CookieJar>, f: impl FnOnce(&CookieJar) -> R) -> R {
    match jar.lock() {
        Ok(jar) => f(&jar),
        Err(poisoned) => f(&poisoned.into_inner()),
    }
}

fn with_jar_mut<R>(jar: &Mutex<CookieJar>, f: impl FnOnce(&mut CookieJar) -> R) -> R {
    match jar.lock() {
        Ok(mut jar) => f(&mut jar),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

/// Put an expiring `cookie` in the delta unless one is already there.
///
/// `cookie::CookieJar::remove` emits nothing for a cookie the client did not
/// present, which would make a logout a silent no-op whenever path or domain
/// scoping kept the cookie out of *this* request. See [`Cookies::remove`].
fn force_removal(jar: &mut CookieJar, mut cookie: Cookie<'static>) {
    if jar.delta().any(|pending| pending.name() == cookie.name()) {
        return;
    }
    cookie.make_removal();
    jar.add(cookie);
}

/// Parse a `Cookie` request header into a jar.
///
/// A malformed pair is skipped rather than failing the request: one unparseable
/// cookie left over from an old deployment must not 400 every request from a
/// browser that still has it.
pub fn jar_from_headers(headers: &http::HeaderMap) -> CookieJar {
    let mut jar = CookieJar::new();
    for value in headers.get_all(http::header::COOKIE) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for pair in value.split(';') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            if let Ok(cookie) = Cookie::parse_encoded(pair.to_owned()) {
                jar.add_original(cookie);
            }
        }
    }
    jar
}

#[cfg(test)]
mod tests {
    use tower::ServiceExt as _;

    use super::*;
    use crate::app::AppBuilder;
    use crate::middleware::{Guard, MiddlewareStack};
    use crate::response::{IntoResponse, NoContent};
    use crate::router::Router;
    use crate::{BoxFuture, Response};

    fn cookie_headers(value: &str) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            http::HeaderValue::from_str(value).unwrap(),
        );
        headers
    }

    fn keyed() -> Cookies {
        Cookies::with_key(CookieJar::new(), Arc::new(CookieKey::generate()))
    }

    // ── reading the request header ────────────────────────────────────────

    #[test]
    fn a_cookie_header_parses_into_a_jar() {
        let jar = jar_from_headers(&cookie_headers("theme=dark; sid=abc123"));
        assert_eq!(
            jar.get("theme").map(|c| c.value().to_owned()),
            Some("dark".into())
        );
        assert_eq!(
            jar.get("sid").map(|c| c.value().to_owned()),
            Some("abc123".into())
        );
    }

    #[test]
    fn percent_encoded_values_are_decoded() {
        let jar = jar_from_headers(&cookie_headers("greeting=hello%20world"));
        assert_eq!(
            jar.get("greeting").map(|c| c.value().to_owned()),
            Some("hello world".into())
        );
    }

    #[test]
    fn a_malformed_pair_is_skipped_not_fatal() {
        let jar = jar_from_headers(&cookie_headers("broken; theme=dark; =nothing"));
        assert_eq!(
            jar.get("theme").map(|c| c.value().to_owned()),
            Some("dark".into())
        );
    }

    #[test]
    fn several_cookie_headers_are_all_read() {
        let mut headers = http::HeaderMap::new();
        headers.append(http::header::COOKIE, http::HeaderValue::from_static("a=1"));
        headers.append(http::header::COOKIE, http::HeaderValue::from_static("b=2"));
        let jar = jar_from_headers(&headers);
        assert!(jar.get("a").is_some());
        assert!(jar.get("b").is_some());
    }

    // ── the delta ─────────────────────────────────────────────────────────

    #[test]
    fn added_cookies_appear_in_the_delta() {
        let cookies = Cookies::new(CookieJar::new());
        cookies.add(Cookie::new("seen", "1"));
        let delta = cookies.delta();
        assert_eq!(delta.len(), 1);
        assert!(delta[0].to_str().unwrap().starts_with("seen=1"));
    }

    #[test]
    fn original_cookies_are_readable_and_not_in_the_delta() {
        let cookies = Cookies::new(jar_from_headers(&cookie_headers("theme=dark")));
        assert_eq!(
            cookies.get("theme").map(|c| c.value().to_owned()),
            Some("dark".into())
        );
        assert!(cookies.delta().is_empty());
    }

    #[test]
    fn clones_share_one_jar() {
        let cookies = Cookies::new(CookieJar::new());
        let clone = cookies.clone();
        clone.add(Cookie::new("from-the-clone", "1"));
        assert!(cookies.get("from-the-clone").is_some());
    }

    #[test]
    fn removing_a_cookie_the_client_did_not_send_still_expires_it() {
        let cookies = Cookies::new(CookieJar::new());
        cookies.remove(Cookie::new("sid", ""));
        let delta = cookies.delta();
        assert_eq!(delta.len(), 1, "a logout must not be a silent no-op");
        assert!(delta[0].to_str().unwrap().contains("Max-Age=0"));
    }

    #[test]
    fn removing_a_cookie_the_client_sent_expires_it_once() {
        let cookies = Cookies::new(jar_from_headers(&cookie_headers("sid=abc123")));
        cookies.remove(Cookie::new("sid", ""));
        let delta = cookies.delta();
        assert_eq!(delta.len(), 1);
        let rendered = delta[0].to_str().unwrap();
        assert!(rendered.starts_with("sid="));
        assert!(rendered.contains("Max-Age=0"));
        assert!(rendered.contains("Path=/"), "a removal must match on path");
    }

    // ── the defaults ──────────────────────────────────────────────────────

    fn rendered(cookies: &Cookies, name: &str) -> String {
        cookies
            .delta()
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with(&format!("{name}=")))
            .expect("the cookie is pending")
            .to_owned()
    }

    #[test]
    fn an_unqualified_cookie_gets_the_documented_defaults() {
        let cookies = Cookies::new(CookieJar::new());
        cookies.add(Cookie::new("seen", "1"));
        let line = rendered(&cookies, "seen");
        assert!(line.contains("HttpOnly"), "{line}");
        assert!(line.contains("SameSite=Lax"), "{line}");
        assert!(line.contains("Path=/"), "{line}");
        assert!(line.contains("Secure"), "{line}");
    }

    #[test]
    fn an_explicit_attribute_is_never_overwritten() {
        let cookies = Cookies::new(CookieJar::new());
        cookies.add(
            Cookie::build(("csrf", "t0ken"))
                .http_only(false)
                .secure(false)
                .same_site(SameSite::Strict)
                .path("/checkout")
                .into(),
        );
        let line = rendered(&cookies, "csrf");
        assert!(!line.contains("HttpOnly"), "{line}");
        assert!(!line.contains("Secure"), "{line}");
        assert!(line.contains("SameSite=Strict"), "{line}");
        assert!(line.contains("Path=/checkout"), "{line}");
    }

    #[test]
    fn a_development_jar_does_not_demand_https() {
        let cookies = Cookies::new(CookieJar::new()).with_defaults(CookieDefaults::INSECURE);
        cookies.add(Cookie::new("seen", "1"));
        let line = rendered(&cookies, "seen");
        assert!(!line.contains("Secure"), "{line}");
        assert!(line.contains("HttpOnly"), "everything else still applies");
    }

    #[test]
    fn only_the_dev_profile_relaxes_secure() {
        assert!(!CookieDefaults::for_profile(Profile::Dev).secure);
        assert!(CookieDefaults::for_profile(Profile::Test).secure);
        assert!(CookieDefaults::for_profile(Profile::Production).secure);
        assert_eq!(CookieDefaults::default(), CookieDefaults::SECURE);
    }

    #[cfg(feature = "private-cookies")]
    #[test]
    fn a_signed_write_is_qualified_like_a_plain_one() {
        let cookies = keyed();
        cookies.signed().add(Cookie::new("uid", "42"));
        assert!(rendered(&cookies, "uid").contains("HttpOnly"));

        let private = keyed();
        private.private().add(Cookie::new("session", "s3cret"));
        assert!(rendered(&private, "session").contains("SameSite=Lax"));
    }

    // ── applying the delta to a response ──────────────────────────────────

    #[test]
    fn an_untouched_jar_writes_no_header() {
        let cookies = Cookies::new(jar_from_headers(&cookie_headers("theme=dark")));
        let mut headers = http::HeaderMap::new();
        cookies.apply_to(&mut headers);
        assert!(headers.is_empty());
    }

    #[test]
    fn each_pending_cookie_becomes_its_own_header() {
        let cookies = Cookies::new(CookieJar::new());
        cookies.add(Cookie::new("a", "1"));
        cookies.add(Cookie::new("b", "2"));
        cookies.add(Cookie::new("c", "3"));

        let mut headers = http::HeaderMap::new();
        cookies.apply_to(&mut headers);
        assert_eq!(headers.get_all(http::header::SET_COOKIE).iter().count(), 3);
    }

    #[test]
    fn a_header_the_response_already_carries_is_not_clobbered() {
        let cookies = Cookies::new(CookieJar::new());
        cookies.add(Cookie::new("seen", "1"));

        let mut headers = http::HeaderMap::new();
        headers.append(
            http::header::SET_COOKIE,
            http::HeaderValue::from_static("sid=abc; Path=/; HttpOnly"),
        );
        cookies.apply_to(&mut headers);

        let all: Vec<&str> = headers
            .get_all(http::header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&"sid=abc; Path=/; HttpOnly"));
    }

    #[test]
    fn the_same_cookie_is_never_set_twice() {
        let cookies = Cookies::new(CookieJar::new());
        cookies.add(Cookie::new("sid", "from-the-jar"));

        let mut headers = http::HeaderMap::new();
        headers.append(
            http::header::SET_COOKIE,
            http::HeaderValue::from_static("sid=from-the-response; Path=/"),
        );
        cookies.apply_to(&mut headers);

        let all: Vec<&str> = headers
            .get_all(http::header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect();
        assert_eq!(all, ["sid=from-the-response; Path=/"]);
    }

    #[test]
    fn a_name_is_read_from_before_the_first_equals() {
        let value = http::HeaderValue::from_static("sid=YWJjZA==; Path=/; HttpOnly");
        assert_eq!(set_cookie_name(&value), Some("sid"));
        assert_eq!(
            set_cookie_name(&http::HeaderValue::from_static("nonsense")),
            None
        );
    }

    // ── signing and encryption ────────────────────────────────────────────

    #[test]
    fn a_signed_cookie_round_trips() {
        let cookies = keyed();
        cookies.signed().add(Cookie::new("uid", "42"));
        assert_eq!(
            cookies.signed().get("uid").map(|c| c.value().to_owned()),
            Some("42".into())
        );
    }

    #[test]
    fn a_signed_value_is_not_the_plain_value_on_the_wire() {
        let cookies = keyed();
        cookies.signed().add(Cookie::new("uid", "42"));
        let raw = cookies.get("uid").expect("the cookie is in the jar");
        assert_ne!(raw.value(), "42");
        assert!(raw.value().ends_with("42"), "the value stays readable");
    }

    #[test]
    fn a_tampered_signed_cookie_is_rejected() {
        let cookies = keyed();
        cookies.signed().add(Cookie::new("uid", "42"));
        let signed_value = cookies.get("uid").expect("the cookie is in the jar");
        let mut tampered = signed_value.value().to_owned();
        tampered.pop();
        tampered.push('9');

        let victim = keyed();
        victim.add(Cookie::new("uid", tampered));
        assert_eq!(victim.signed().get("uid"), None);
    }

    #[test]
    fn another_key_cannot_read_a_signed_cookie() {
        let cookies = keyed();
        cookies.signed().add(Cookie::new("uid", "42"));
        let value = cookies.get("uid").expect("the cookie is in the jar");

        let other = keyed();
        other.add(Cookie::new("uid", value.value().to_owned()));
        assert_eq!(other.signed().get("uid"), None);
    }

    #[cfg(feature = "private-cookies")]
    #[test]
    fn a_private_cookie_round_trips_and_hides_its_value() {
        let cookies = keyed();
        cookies.private().add(Cookie::new("session", "s3cret"));
        let raw = cookies.get("session").expect("the cookie is in the jar");
        assert!(!raw.value().contains("s3cret"));
        assert_eq!(
            cookies
                .private()
                .get("session")
                .map(|c| c.value().to_owned()),
            Some("s3cret".into())
        );
    }

    #[cfg(feature = "private-cookies")]
    #[test]
    fn a_tampered_private_cookie_is_rejected() {
        let cookies = keyed();
        cookies.private().add(Cookie::new("session", "s3cret"));
        let encrypted = cookies.get("session").expect("the cookie is in the jar");
        let mut tampered = encrypted.value().to_owned();
        tampered.pop();
        tampered.push('A');

        let victim = keyed();
        victim.add(Cookie::new("session", tampered));
        assert_eq!(victim.private().get("session"), None);
    }

    #[test]
    fn without_a_key_a_signed_view_fails_closed() {
        let cookies = Cookies::new(CookieJar::new());
        cookies.signed().add(Cookie::new("uid", "42"));
        assert_eq!(cookies.signed().get("uid"), None);
        assert!(cookies.delta().is_empty(), "an unsigned write is dropped");
    }

    #[test]
    fn a_signed_cookie_can_be_expired() {
        let cookies = keyed();
        cookies.signed().remove(Cookie::new("uid", ""));
        let delta = cookies.delta();
        assert_eq!(delta.len(), 1);
        assert!(delta[0].to_str().unwrap().contains("Max-Age=0"));
    }

    // ── the key ───────────────────────────────────────────────────────────

    #[test]
    fn a_derived_key_is_the_documented_length_and_deterministic() {
        let secret = b"0123456789abcdef0123456789abcdef";
        let first = CookieKey::derive_from_bytes(secret).expect("32 bytes is enough");
        let second = CookieKey::derive_from_bytes(secret).expect("32 bytes is enough");
        assert_eq!(first.expose().len(), CookieKey::LEN);
        assert_eq!(first.expose(), second.expose());
    }

    #[test]
    fn different_secrets_derive_different_keys() {
        let first = CookieKey::derive_from_bytes(b"0123456789abcdef0123456789abcdef").unwrap();
        let second = CookieKey::derive_from_bytes(b"0123456789abcdef0123456789abcdeg").unwrap();
        assert_ne!(first.expose(), second.expose());
    }

    #[test]
    fn a_short_secret_is_refused() {
        assert!(CookieKey::derive_from_bytes(b"too-short").is_none());
        assert!(CookieKey::derive_from_bytes(&[b'x'; 31]).is_none());
        assert!(CookieKey::derive_from_bytes(&[b'x'; 32]).is_some());
    }

    #[test]
    fn key_material_of_the_wrong_length_is_refused() {
        assert!(CookieKey::try_from_material(&[0u8; 64]).is_some());
        assert!(CookieKey::try_from_material(&[0u8; 32]).is_none());
    }

    #[test]
    fn a_key_never_renders_its_material() {
        let key = CookieKey::generate();
        assert_eq!(format!("{key:?}"), "CookieKey(***)");
    }

    // ── through HTTP ──────────────────────────────────────────────────────
    //
    // Everything below drives a real application through `oneshot`, because the
    // bug these cover is that the jar and the response never met: a test that
    // called `Cookies::delta` itself would have passed throughout.

    /// One cookie.
    async fn adds_one(cookies: Cookies) -> NoContent {
        cookies.add(Cookie::new("seen", "1"));
        NoContent
    }

    /// Three, to prove they do not collapse into one header.
    async fn adds_three(cookies: Cookies) -> NoContent {
        cookies.add(Cookie::new("a", "1"));
        cookies.add(Cookie::new("b", "2"));
        cookies.add(Cookie::new("c", "3"));
        NoContent
    }

    /// A logout.
    async fn removes_one(cookies: Cookies) -> NoContent {
        cookies.remove(Cookie::new("theme", ""));
        NoContent
    }

    /// Never mentions a cookie.
    async fn adds_nothing() -> NoContent {
        NoContent
    }

    /// Two `Cookies` parameters, written through independently.
    async fn adds_through_two_handles(first: Cookies, second: Cookies) -> NoContent {
        first.add(Cookie::new("first", "1"));
        second.add(Cookie::new("second", "2"));
        NoContent
    }

    /// Writes its own `Set-Cookie` *and* uses the jar, as `moso-auth` does.
    async fn writes_its_own_header(cookies: Cookies) -> Response {
        cookies.add(Cookie::new("seen", "1"));
        let mut response = NoContent.into_response();
        response.headers_mut().append(
            http::header::SET_COOKIE,
            http::HeaderValue::from_static("sid=from-the-response; Path=/"),
        );
        response
    }

    /// Writes its own `Set-Cookie` for a name the jar also sets.
    async fn collides_with_its_own_header(cookies: Cookies) -> Response {
        cookies.add(Cookie::new("sid", "from-the-jar"));
        let mut response = NoContent.into_response();
        response.headers_mut().append(
            http::header::SET_COOKIE,
            http::HeaderValue::from_static("sid=from-the-response; Path=/"),
        );
        response
    }

    /// Signs a value the next request reads back.
    #[cfg(feature = "private-cookies")]
    async fn signs(cookies: Cookies) -> NoContent {
        cookies.signed().add(Cookie::new("uid", "42"));
        NoContent
    }

    /// Encrypts a value the next request reads back.
    #[cfg(feature = "private-cookies")]
    async fn encrypts(cookies: Cookies) -> NoContent {
        cookies.private().add(Cookie::new("session", "s3cret"));
        NoContent
    }

    /// Reports what each view can see, as `signed|private|plain`.
    #[cfg(feature = "private-cookies")]
    async fn reads_back(cookies: Cookies) -> String {
        let value = |cookie: Option<Cookie<'static>>| {
            cookie.map_or_else(|| "-".to_owned(), |cookie| cookie.value().to_owned())
        };
        format!(
            "{}|{}|{}",
            value(cookies.signed().get("uid")),
            value(cookies.private().get("session")),
            value(cookies.get("uid"))
        )
    }

    /// A guard that leaves a crumb in the jar and lets the request through.
    #[derive(Clone, Copy)]
    struct Crumb;

    impl Guard for Crumb {
        fn describe(&self, _op: &mut OperationBuilder) {}

        fn check<'a>(
            &'a self,
            _parts: &'a http::request::Parts,
            ctx: &'a RequestCtx,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                ctx.cookies().add(Cookie::new("guard", "was-here"));
                Ok(())
            })
        }
    }

    /// A guard that clears a stale session and then rejects, which is the
    /// reason a rejected request still sends its cookies.
    #[derive(Clone, Copy)]
    struct ClearsAndRejects;

    impl Guard for ClearsAndRejects {
        fn describe(&self, _op: &mut OperationBuilder) {}

        fn check<'a>(
            &'a self,
            _parts: &'a http::request::Parts,
            ctx: &'a RequestCtx,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                ctx.cookies().remove(Cookie::new("sid", ""));
                Err(Error::unauthenticated().with_detail("that session is over"))
            })
        }
    }

    /// Build an application with no configuration, no middleware and `router`.
    fn serve(router: Router) -> axum::Router<()> {
        AppBuilder::new()
            .profile(Profile::Test)
            .middleware(MiddlewareStack::bare())
            .mount(router)
            .build()
            .expect("these routes need nothing to boot")
            .into_service()
    }

    /// The same, with a signing key registered.
    #[cfg(feature = "private-cookies")]
    fn serve_keyed(router: Router) -> axum::Router<()> {
        AppBuilder::new()
            .profile(Profile::Test)
            .middleware(MiddlewareStack::bare())
            .provide(CookieKey::generate())
            .mount(router)
            .build()
            .expect("these routes need nothing to boot")
            .into_service()
    }

    /// `GET path`, with an optional `Cookie` header.
    async fn get(service: &axum::Router<()>, path: &str, cookie: Option<&str>) -> Response {
        let mut request = http::Request::builder().uri(path);
        if let Some(cookie) = cookie {
            request = request.header(http::header::COOKIE, cookie);
        }
        service
            .clone()
            .into_service::<axum::body::Body>()
            .oneshot(request.body(axum::body::Body::empty()).expect("valid"))
            .await
            .expect("the router is infallible")
    }

    /// Every `Set-Cookie` on a response, in order.
    fn set_cookies(response: &Response) -> Vec<String> {
        response
            .headers()
            .get_all(http::header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().expect("ascii").to_owned())
            .collect()
    }

    /// The `Cookie` request header that replays every `Set-Cookie` a response
    /// carried, the way a browser would.
    #[cfg(feature = "private-cookies")]
    fn replay(response: &Response) -> String {
        set_cookies(response)
            .iter()
            .filter_map(|line| line.split(';').next())
            .collect::<Vec<_>>()
            .join("; ")
    }

    #[tokio::test]
    async fn a_handler_that_adds_a_cookie_sends_it() {
        let service = serve(Router::new().get("/visit", adds_one));
        let response = get(&service, "/visit", None).await;

        let set = set_cookies(&response);
        assert_eq!(set.len(), 1, "the jar never reached the response");
        assert!(set[0].starts_with("seen=1"), "{}", set[0]);
        assert!(set[0].contains("HttpOnly"), "{}", set[0]);
        assert!(set[0].contains("SameSite=Lax"), "{}", set[0]);
        assert!(set[0].contains("Path=/"), "{}", set[0]);
    }

    #[tokio::test]
    async fn three_added_cookies_become_three_headers() {
        let service = serve(Router::new().get("/three", adds_three));
        let set = set_cookies(&get(&service, "/three", None).await);

        assert_eq!(set.len(), 3);
        for name in ["a=1", "b=2", "c=3"] {
            assert!(set.iter().any(|line| line.starts_with(name)), "{set:?}");
        }
    }

    #[tokio::test]
    async fn a_removed_cookie_is_expired_on_the_way_out() {
        let service = serve(Router::new().get("/logout", removes_one));
        let set = set_cookies(&get(&service, "/logout", Some("theme=dark")).await);

        assert_eq!(set.len(), 1);
        assert!(set[0].starts_with("theme="), "{}", set[0]);
        assert!(set[0].contains("Max-Age=0"), "{}", set[0]);
    }

    #[tokio::test]
    async fn a_handler_that_sets_no_cookie_sends_no_set_cookie_header() {
        let service = serve(Router::new().get("/quiet", adds_nothing));
        let response = get(&service, "/quiet", Some("theme=dark")).await;
        assert!(
            response.headers().get(http::header::SET_COOKIE).is_none(),
            "a handler that never mentions a cookie must not grow a header"
        );
    }

    #[tokio::test]
    async fn a_response_that_already_sets_a_cookie_keeps_it() {
        let service = serve(Router::new().get("/both", writes_its_own_header));
        let set = set_cookies(&get(&service, "/both", None).await);

        assert_eq!(set.len(), 2, "{set:?}");
        assert!(set.contains(&"sid=from-the-response; Path=/".to_owned()));
        assert!(set.iter().any(|line| line.starts_with("seen=1")));
    }

    #[tokio::test]
    async fn a_jar_write_never_clobbers_a_cookie_the_response_already_set() {
        let service = serve(Router::new().get("/collide", collides_with_its_own_header));
        let set = set_cookies(&get(&service, "/collide", None).await);

        assert_eq!(set, ["sid=from-the-response; Path=/"]);
    }

    #[tokio::test]
    async fn every_handle_on_a_request_writes_into_one_jar() {
        // The regression test for the bug this whole module guards against: if
        // extraction ever makes its own jar again, one of these two writes goes
        // nowhere and the assertion fails.
        let service = serve(Router::new().get("/two", adds_through_two_handles));
        let set = set_cookies(&get(&service, "/two", None).await);

        assert_eq!(set.len(), 2, "a second jar swallowed a write: {set:?}");
    }

    #[tokio::test]
    async fn a_guard_writes_into_the_same_jar_as_the_handler() {
        let service = serve(Router::new().get("/guarded", adds_one).guard(Crumb));
        let set = set_cookies(&get(&service, "/guarded", None).await);

        assert_eq!(set.len(), 2, "{set:?}");
        assert!(set.iter().any(|line| line.starts_with("guard=was-here")));
        assert!(set.iter().any(|line| line.starts_with("seen=1")));
    }

    #[tokio::test]
    async fn a_cookie_set_by_a_rejecting_guard_still_reaches_the_client() {
        let service = serve(
            Router::new()
                .get("/private", adds_one)
                .guard(ClearsAndRejects),
        );
        let response = get(&service, "/private", Some("sid=stale")).await;

        assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
        let set = set_cookies(&response);
        assert_eq!(set.len(), 1, "the handler never ran, the guard's write did");
        assert!(set[0].starts_with("sid="), "{}", set[0]);
        assert!(set[0].contains("Max-Age=0"), "{}", set[0]);
    }

    // Gated with `private-cookies` because the shared test router mounts the
    // encrypting handlers; signed round-trips are also covered by the unit tests.
    #[cfg(feature = "private-cookies")]
    #[tokio::test]
    async fn a_signed_cookie_survives_a_round_trip_through_http() {
        let service = serve_keyed(Router::new().get("/sign", signs).get("/read", reads_back));

        let first = get(&service, "/sign", None).await;
        let jar = replay(&first);
        assert!(
            !jar.contains("uid=42"),
            "the wire value is not the plain one"
        );

        let second = get(&service, "/read", Some(&jar)).await;
        let body = body_text(second).await;
        let (signed, rest) = body.split_once('|').expect("three fields");
        let (_private, plain) = rest.split_once('|').expect("three fields");

        assert_eq!(signed, "42", "the signed view could not read it back");
        assert_ne!(plain, "42", "the plain view must not yield the value");
    }

    #[cfg(feature = "private-cookies")]
    #[tokio::test]
    async fn a_private_cookie_survives_a_round_trip_through_http() {
        let service = serve_keyed(
            Router::new()
                .get("/encrypt", encrypts)
                .get("/read", reads_back),
        );

        let first = get(&service, "/encrypt", None).await;
        let jar = replay(&first);
        assert!(!jar.contains("s3cret"), "the wire value is ciphertext");

        let second = get(&service, "/read", Some(&jar)).await;
        let body = body_text(second).await;
        let private = body.split('|').nth(1).expect("three fields");

        assert_eq!(private, "s3cret", "the private view could not decrypt it");
    }

    /// A response body as a string.
    #[cfg(feature = "private-cookies")]
    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .expect("a complete body");
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }
}
