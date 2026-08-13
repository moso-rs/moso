//! The [`CaptchaVerifier`] that ships: one HTTP verifier, three providers.
//!
//! [`ThrottleDecision::Challenge`](crate::ThrottleDecision) is only worth having
//! if something can clear it, and until this module existed nothing could — the
//! trait was public and the workspace contained no implementation, so the
//! challenge tier was a refusal with no way out. That is fixed from both ends:
//! here, by shipping a verifier, and in [`crate::throttle`], by making the tier
//! off until one is configured.
//!
//! # One verifier, not three
//!
//! Cloudflare Turnstile, hCaptcha and reCAPTCHA verify identically once you
//! strip the branding: `POST` a form body carrying `secret` and `response` to a
//! fixed URL, read a JSON `success` field, and look at `error-codes` when it is
//! false. The only thing that differs is the URL, so the only thing
//! [`CaptchaProvider`] holds is the URL — three constructors over one code path,
//! rather than three verifiers that would each need their own test and would
//! drift the moment one of them learned about `remoteip`.
//!
//! ```text
//! POST https://…/siteverify        secret=…&response=…&remoteip=…
//!   200 {"success": true}                          → Ok(true)      cleared
//!   200 {"success": false, "error-codes": […]}     → Ok(false)     refused
//!   200 {"success": false, "error-codes": ["invalid-input-secret"]}
//!                                                  → Err(Config)   our fault
//!   anything else, or no answer                    → Err(Unavailable)
//! ```
//!
//! # Three answers, and why none of them may collapse
//!
//! A verifier that cannot reach its provider has **not** decided anything, so it
//! returns [`Error::Unavailable`] rather than a `bool`. Answering `Ok(false)`
//! would lock every throttled user out for as long as a third party is down;
//! answering `Ok(true)` would make "make the provider unreachable" the way to
//! skip the challenge tier, which is the attack the tier exists to slow.
//!
//! The third answer is the one most implementations get wrong. When the provider
//! says `invalid-input-secret`, the *token* was never judged: the deployment is
//! misconfigured. Reporting that as `Ok(false)` would turn a typo in an
//! environment variable into a silent, permanent lockout that looks exactly like
//! an attack in progress, so it is [`Error::Config`], whose message names the
//! provider and the codes it sent.
//!
//! # The transport is the one this crate already has
//!
//! [`crate::oauth::http`] already owns a `hyper` client with a total deadline, a
//! response-size cap, a redirect cap and `rustls` pinned to the `ring` provider.
//! A CAPTCHA check is one more `POST` to somebody else's server with a secret in
//! the body, which is the same problem with the same answer, so this module
//! borrows that transport rather than adding a second HTTP client to a workspace
//! that is already over its dependency budget. It is also the seam a test uses:
//! [`HttpCaptchaVerifier::with_transport`] takes any
//! [`HttpTransport`], and this module's own
//! tests point it at a real listener on loopback.
//!
//! # Registering one is half the wiring
//!
//! The other half is [`ThrottleConfig::challenge_after`](crate::ThrottleConfig),
//! which is off by default and says when a challenge starts being demanded. The
//! reasoning for that default is on the field itself; it is not restated here.

use std::sync::Arc;

use moso_core::{BoxFuture, SecretString};

use crate::oauth::http::{HttpRequest, HttpTransport, RustlsTransport};
use crate::throttle::CaptchaVerifier;
use crate::{Error, Result};

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

/// The longest response token this verifier will forward.
///
/// A provider's token is bounded — Turnstile's is documented at 2 048
/// characters — but the value arrives in a header from an unauthenticated
/// caller, so the cap is enforced here rather than assumed. Eight kilobytes
/// leaves every current provider four times the room it needs and still stops
/// this endpoint from being an amplifier that turns a small request to us into
/// a large one to somebody else.
///
/// ```
/// assert_eq!(moso_auth::captcha::MAX_RESPONSE_TOKEN, 8 * 1024);
/// ```
pub const MAX_RESPONSE_TOKEN: usize = 8 * 1024;

/// The component an unreachable or misbehaving provider is reported under.
const COMPONENT: &str = "captcha provider";

/// The `error-codes` values that mean the deployment is wrong, not the caller.
///
/// Every provider spells its own set, and the union is short enough to write
/// down. Anything not on this list is treated as a failed token, which is the
/// safe direction: an unknown code refuses the attempt rather than clearing it.
const OUR_FAULT: &[&str] = &[
    // Turnstile and reCAPTCHA.
    "missing-input-secret",
    "invalid-input-secret",
    "bad-request",
    // hCaptcha.
    "invalid-keys",
    "sitekey-secret-mismatch",
    "not-using-dummy-passcode",
];

/// Which CAPTCHA service, and where it verifies.
///
/// ```
/// use moso_auth::captcha::CaptchaProvider;
///
/// assert_eq!(CaptchaProvider::turnstile().name(), "turnstile");
/// assert!(CaptchaProvider::hcaptcha().verify_url().starts_with("https://"));
/// ```
#[derive(Clone, Debug)]
pub struct CaptchaProvider {
    /// What the log calls it.
    name: &'static str,
    /// Where the verification `POST` goes.
    verify_url: String,
}

impl CaptchaProvider {
    /// Cloudflare Turnstile.
    ///
    /// ```
    /// use moso_auth::captcha::CaptchaProvider;
    ///
    /// assert!(CaptchaProvider::turnstile().verify_url().contains("challenges.cloudflare.com"));
    /// ```
    #[must_use]
    pub fn turnstile() -> Self {
        Self {
            name: "turnstile",
            verify_url: "https://challenges.cloudflare.com/turnstile/v0/siteverify".to_owned(),
        }
    }

    /// hCaptcha.
    ///
    /// ```
    /// use moso_auth::captcha::CaptchaProvider;
    ///
    /// assert_eq!(CaptchaProvider::hcaptcha().name(), "hcaptcha");
    /// ```
    #[must_use]
    pub fn hcaptcha() -> Self {
        Self {
            name: "hcaptcha",
            verify_url: "https://api.hcaptcha.com/siteverify".to_owned(),
        }
    }

    /// Google reCAPTCHA (v2 and v3, which share this endpoint).
    ///
    /// ```
    /// use moso_auth::captcha::CaptchaProvider;
    ///
    /// assert_eq!(CaptchaProvider::recaptcha().name(), "recaptcha");
    /// ```
    #[must_use]
    pub fn recaptcha() -> Self {
        Self {
            name: "recaptcha",
            verify_url: "https://www.google.com/recaptcha/api/siteverify".to_owned(),
        }
    }

    /// Any other service that speaks the same three fields, or a proxy in front
    /// of one of the three above.
    ///
    /// `name` is what the log calls it and is `&'static str` because
    /// [`CaptchaVerifier::provider`] is; `url` is where the `POST` goes.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when `url` is not an `http` or `https` URL — a boot
    /// error naming the value, rather than a login that fails forever. Whether
    /// the URL is *safe* is the transport's rule, not this one: it refuses
    /// plaintext to anything but loopback, so a test server is allowed and a
    /// production misconfiguration is not.
    ///
    /// ```
    /// use moso_auth::captcha::CaptchaProvider;
    ///
    /// let provider = CaptchaProvider::custom("edge", "https://captcha.internal/siteverify")?;
    /// assert_eq!(provider.name(), "edge");
    /// assert!(CaptchaProvider::custom("edge", "not a url").is_err());
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn custom(name: &'static str, url: impl Into<String>) -> Result<Self> {
        let verify_url = url.into();
        moso_schema::Url::parse_with_schemes(&verify_url, &["https", "http"]).map_err(|error| {
            Error::Config(
                format!("`{verify_url}` is not a captcha verification URL: {error}").into(),
            )
        })?;
        Ok(Self { name, verify_url })
    }

    /// What the log calls it.
    ///
    /// ```
    /// # use moso_auth::captcha::CaptchaProvider;
    /// # fn f(p: &CaptchaProvider) { let _: &'static str = p.name(); }
    /// ```
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Where the verification `POST` goes.
    ///
    /// ```
    /// # use moso_auth::captcha::CaptchaProvider;
    /// # fn f(p: &CaptchaProvider) { let _: &str = p.verify_url(); }
    /// ```
    #[must_use]
    pub fn verify_url(&self) -> &str {
        &self.verify_url
    }
}

// ---------------------------------------------------------------------------
// The verifier
// ---------------------------------------------------------------------------

/// The shipped [`CaptchaVerifier`]: one `POST`, one `success` field.
///
/// ```
/// use moso_auth::captcha::{CaptchaProvider, HttpCaptchaVerifier};
/// use moso_core::SecretString;
///
/// # fn f() -> moso_auth::Result<()> {
/// let verifier = HttpCaptchaVerifier::new(
///     CaptchaProvider::turnstile(),
///     SecretString::new(std::env::var("TURNSTILE_SECRET").unwrap_or_default()),
/// )?;
///
/// // Register it with `AuthState::captcha(verifier.shared())`, and set
/// // `ThrottleConfig::challenge_after` so the tier is actually reached.
/// assert_eq!(verifier.provider_name(), "turnstile");
/// # Ok(()) }
/// # f().unwrap();
/// ```
pub struct HttpCaptchaVerifier {
    /// Which service, and where.
    provider: CaptchaProvider,
    /// The server-side secret. Never logged, never printed.
    secret: SecretString,
    /// How the request gets out.
    transport: Arc<dyn HttpTransport>,
}

impl HttpCaptchaVerifier {
    /// A verifier over the bundled `rustls` transport.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when `rustls` refuses every TLS version, which can only
    /// happen in a build with them all turned off.
    ///
    /// ```
    /// use moso_auth::captcha::{CaptchaProvider, HttpCaptchaVerifier};
    /// use moso_core::SecretString;
    ///
    /// # fn f() -> moso_auth::Result<HttpCaptchaVerifier> {
    /// HttpCaptchaVerifier::new(CaptchaProvider::hcaptcha(), SecretString::new("shh".to_owned()))
    /// # }
    /// # assert!(f().is_ok());
    /// ```
    pub fn new(provider: CaptchaProvider, secret: SecretString) -> Result<Self> {
        Ok(Self::with_transport(
            provider,
            secret,
            Arc::new(RustlsTransport::new()?),
        ))
    }

    /// A verifier over a transport you supply: a proxy, a shorter deadline, or
    /// a test server.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_auth::captcha::{CaptchaProvider, HttpCaptchaVerifier};
    /// use moso_auth::oauth::http::{HttpTransport, RustlsTransport};
    /// use moso_core::SecretString;
    ///
    /// # fn f() -> moso_auth::Result<()> {
    /// let transport: Arc<dyn HttpTransport> =
    ///     Arc::new(RustlsTransport::new()?.timeout(std::time::Duration::from_secs(3)));
    /// let _ = HttpCaptchaVerifier::with_transport(
    ///     CaptchaProvider::turnstile(),
    ///     SecretString::new("shh".to_owned()),
    ///     transport,
    /// );
    /// # Ok(()) }
    /// # f().unwrap();
    /// ```
    #[must_use]
    pub fn with_transport(
        provider: CaptchaProvider,
        secret: SecretString,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            provider,
            secret,
            transport,
        }
    }

    /// Behind the `Arc` [`AuthState::captcha`](crate::AuthState::captcha) takes.
    ///
    /// ```
    /// use moso_auth::captcha::{CaptchaProvider, HttpCaptchaVerifier};
    /// use moso_core::SecretString;
    ///
    /// # fn f() -> moso_auth::Result<()> {
    /// let verifier =
    ///     HttpCaptchaVerifier::new(CaptchaProvider::recaptcha(), SecretString::new("shh"))?
    ///         .shared();
    /// assert_eq!(verifier.provider(), "recaptcha");
    /// # Ok(()) }
    /// # f().unwrap();
    /// ```
    #[must_use]
    pub fn shared(self) -> Arc<dyn CaptchaVerifier> {
        Arc::new(self)
    }

    /// Which provider this verifier talks to.
    ///
    /// ```
    /// # use moso_auth::captcha::HttpCaptchaVerifier;
    /// # fn f(v: &HttpCaptchaVerifier) { let _: &'static str = v.provider_name(); }
    /// ```
    #[must_use]
    pub fn provider_name(&self) -> &'static str {
        self.provider.name()
    }

    /// The form body: the secret, the token, and the caller's address when the
    /// deployment resolved one.
    ///
    /// Percent-encoded rather than concatenated, because `response` is a value
    /// an unauthenticated caller chose and a `&` in it would otherwise become a
    /// second field.
    fn body(&self, token: &str, ip: Option<&str>) -> String {
        let mut form = form_urlencoded::Serializer::new(String::new());
        form.append_pair("secret", self.secret.expose());
        form.append_pair("response", token);
        if let Some(ip) = ip {
            form.append_pair("remoteip", ip);
        }
        form.finish()
    }

    /// The whole check, as one `async fn` so the trait method stays a `Box::pin`
    /// of something readable.
    async fn check(&self, token: &str, ip: Option<&str>) -> Result<bool> {
        // Refused without a round trip. An empty token is a client that did not
        // solve anything, and an oversized one is not a token any provider
        // issued — neither is worth a request to a third party.
        if token.is_empty() || token.len() > MAX_RESPONSE_TOKEN {
            return Ok(false);
        }

        let request = HttpRequest::form(self.provider.verify_url(), self.body(token, ip));
        let response = self.transport.send(&request).await?;

        if !response.is_success() {
            return Err(Error::Unavailable {
                component: COMPONENT,
                detail: format!(
                    "{} answered {} rather than verifying the token",
                    self.provider.name(),
                    response.status
                ),
                source: None,
            });
        }

        let verdict: Verdict = response.json(COMPONENT)?;
        if verdict.success {
            return Ok(true);
        }

        if let Some(code) = verdict
            .error_codes
            .iter()
            .find(|code| OUR_FAULT.contains(&code.as_str()))
        {
            return Err(Error::Config(
                format!(
                    "the {} secret was rejected by the provider (`{code}`); help: check the \
                     secret key registered with `AuthState::captcha`",
                    self.provider.name()
                )
                .into(),
            ));
        }

        Ok(false)
    }
}

impl core::fmt::Debug for HttpCaptchaVerifier {
    /// Prints the provider and never the secret: a verifier reaches a boot log
    /// through `AuthState`, and a derived `Debug` prints private fields.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HttpCaptchaVerifier")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

impl CaptchaVerifier for HttpCaptchaVerifier {
    fn provider(&self) -> &'static str {
        self.provider.name()
    }

    fn verify<'a>(&'a self, token: &'a str, ip: Option<&'a str>) -> BoxFuture<'a, Result<bool>> {
        Box::pin(self.check(token, ip))
    }
}

/// What every one of the three providers answers with.
///
/// Only two fields are read. The rest — `challenge_ts`, `hostname`, `action`,
/// `score` — are provider-specific and none of them changes the one decision
/// this module makes, so reading them would be storing a fact nothing consults.
#[derive(Debug, serde::Deserialize)]
struct Verdict {
    /// Whether the token checked out.
    success: bool,
    /// Why not, when it did not.
    #[serde(default, rename = "error-codes")]
    error_codes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    use super::*;

    // ── a real provider, on loopback ──────────────────────────────────────

    /// What the fake provider answers, and what it last received.
    #[derive(Default)]
    struct Recorded {
        /// The status line to answer with.
        status: Option<&'static str>,
        /// The body to answer with.
        payload: Option<String>,
        /// The form body of the last request.
        last_body: Option<String>,
    }

    /// A CAPTCHA provider that is a real TCP listener speaking real HTTP.
    ///
    /// A stubbed `HttpTransport` would prove that this module calls a function;
    /// what is worth proving is that the secret and the token reach the wire in
    /// a form a provider would accept, and that every shape of answer maps onto
    /// the right one of the three outcomes.
    struct FakeProvider {
        /// Where it is listening.
        url: String,
        /// What it will do, and what it saw.
        recorded: Arc<Mutex<Recorded>>,
    }

    impl FakeProvider {
        /// Bind, spawn, and answer `payload` with `status` until told otherwise.
        async fn start(status: &'static str, payload: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("loopback binds");
            let port = listener
                .local_addr()
                .expect("the socket has an address")
                .port();
            let recorded = Arc::new(Mutex::new(Recorded {
                status: Some(status),
                payload: Some(payload.to_owned()),
                last_body: None,
            }));

            let served = Arc::clone(&recorded);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let recorded = Arc::clone(&served);
                    tokio::spawn(async move {
                        serve(stream, &recorded).await;
                    });
                }
            });

            Self {
                url: format!("http://127.0.0.1:{port}/siteverify"),
                recorded,
            }
        }

        /// The form body of the last request it saw.
        fn last_body(&self) -> String {
            self.recorded
                .lock()
                .expect("the test lock is not poisoned")
                .last_body
                .clone()
                .unwrap_or_default()
        }

        /// A verifier pointed at this listener.
        fn verifier(&self) -> HttpCaptchaVerifier {
            HttpCaptchaVerifier::with_transport(
                CaptchaProvider::custom("fake", &self.url).expect("a URL"),
                SecretString::new("the-server-secret".to_owned()),
                Arc::new(RustlsTransport::new().expect("a transport")),
            )
        }
    }

    /// One connection: read the request, record its body, answer, close.
    async fn serve(mut stream: tokio::net::TcpStream, recorded: &Arc<Mutex<Recorded>>) {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 2048];

        let head_end = loop {
            let read = match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(count) => count,
            };
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(at) = find(&buffer, b"\r\n\r\n") {
                break at + 4;
            }
            if buffer.len() > 64 * 1024 {
                return;
            }
        };

        let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
        let length: usize = head
            .split("\r\n")
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0);

        while buffer.len() < head_end + length {
            let read = match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(count) => count,
            };
            buffer.extend_from_slice(&chunk[..read]);
        }

        let (status, payload) = {
            let mut recorded = recorded.lock().expect("the test lock is not poisoned");
            recorded.last_body = Some(String::from_utf8_lossy(&buffer[head_end..]).into_owned());
            (
                recorded.status.unwrap_or("200 OK"),
                recorded.payload.clone().unwrap_or_default(),
            )
        };

        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{payload}",
            payload.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    }

    /// The index of `needle` in `haystack`.
    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    // ── the three outcomes ────────────────────────────────────────────────

    #[tokio::test]
    async fn a_token_the_provider_accepts_clears_the_challenge() {
        let provider = FakeProvider::start("200 OK", r#"{"success":true}"#).await;

        assert!(
            provider
                .verifier()
                .verify("the-response-token", Some("203.0.113.7"))
                .await
                .expect("the provider answered")
        );
    }

    #[tokio::test]
    async fn a_token_the_provider_rejects_is_a_refusal_and_not_an_error() {
        let provider = FakeProvider::start(
            "200 OK",
            r#"{"success":false,"error-codes":["invalid-input-response"]}"#,
        )
        .await;

        assert!(
            !provider
                .verifier()
                .verify("stale", None)
                .await
                .expect("a failed check is not an error")
        );
    }

    /// A `?` on this method must never be able to turn a failed CAPTCHA into a
    /// 500 that some middleware retries, so an unknown code refuses rather than
    /// erroring.
    #[tokio::test]
    async fn an_unrecognised_error_code_refuses_rather_than_erroring() {
        let provider = FakeProvider::start(
            "200 OK",
            r#"{"success":false,"error-codes":["something-new"]}"#,
        )
        .await;

        assert!(!provider.verifier().verify("token", None).await.expect("ok"));
    }

    #[tokio::test]
    async fn a_provider_that_cannot_be_reached_is_unavailable_and_never_a_verdict() {
        // Port 1 on loopback: nothing listens, and the connection is refused
        // rather than left hanging, so the test does not wait for a timeout.
        let verifier = HttpCaptchaVerifier::with_transport(
            CaptchaProvider::custom("fake", "http://127.0.0.1:1/siteverify").expect("a URL"),
            SecretString::new("shh".to_owned()),
            Arc::new(RustlsTransport::new().expect("a transport")),
        );

        match verifier.verify("token", None).await {
            Err(Error::Unavailable { .. }) => {}
            other => panic!("an unreachable provider answered {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_five_hundred_from_the_provider_is_unavailable_rather_than_a_refusal() {
        let provider = FakeProvider::start("502 Bad Gateway", "<html>nope</html>").await;

        match provider.verifier().verify("token", None).await {
            Err(Error::Unavailable {
                component, detail, ..
            }) => {
                assert_eq!(component, COMPONENT);
                assert!(detail.contains("502"), "{detail}");
            }
            other => panic!("a 502 answered {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_is_unavailable_rather_than_a_refusal() {
        let provider = FakeProvider::start("200 OK", "<html>captive portal</html>").await;

        assert!(matches!(
            provider.verifier().verify("token", None).await,
            Err(Error::Unavailable { .. })
        ));
    }

    /// A wrong secret is our mistake, and a lockout that looks exactly like an
    /// attack in progress is the worst possible way to report it.
    #[tokio::test]
    async fn a_rejected_secret_is_a_configuration_error_and_not_a_failed_token() {
        let provider = FakeProvider::start(
            "200 OK",
            r#"{"success":false,"error-codes":["invalid-input-secret"]}"#,
        )
        .await;

        match provider.verifier().verify("token", None).await {
            Err(Error::Config(detail)) => {
                assert!(detail.contains("invalid-input-secret"), "{detail}");
                assert!(detail.contains("help:"), "{detail}");
            }
            other => panic!("a rejected secret answered {other:?}"),
        }
    }

    // ── what reaches the wire ─────────────────────────────────────────────

    #[tokio::test]
    async fn the_secret_the_token_and_the_address_are_form_encoded() {
        let provider = FakeProvider::start("200 OK", r#"{"success":true}"#).await;

        provider
            .verifier()
            .verify("a token with a & in it", Some("203.0.113.7"))
            .await
            .expect("the provider answered");

        let body = provider.last_body();
        assert!(body.contains("secret=the-server-secret"), "{body}");
        assert!(
            body.contains("response=a+token+with+a+%26+in+it"),
            "a `&` in a caller-chosen value must not become a second field: {body}"
        );
        assert!(body.contains("remoteip=203.0.113.7"), "{body}");
    }

    #[tokio::test]
    async fn an_absent_address_is_simply_omitted() {
        let provider = FakeProvider::start("200 OK", r#"{"success":true}"#).await;

        provider
            .verifier()
            .verify("token", None)
            .await
            .expect("the provider answered");

        assert!(!provider.last_body().contains("remoteip"));
    }

    /// An empty or oversized token is not a token any provider issued, so it is
    /// refused without making a request on a stranger's behalf.
    #[tokio::test]
    async fn a_missing_or_oversized_token_costs_no_round_trip() {
        let provider = FakeProvider::start("200 OK", r#"{"success":true}"#).await;
        let verifier = provider.verifier();

        assert!(!verifier.verify("", None).await.expect("ok"));
        assert!(
            !verifier
                .verify(&"x".repeat(MAX_RESPONSE_TOKEN + 1), None)
                .await
                .expect("ok")
        );
        assert!(
            provider.last_body().is_empty(),
            "neither attempt reached the provider"
        );
    }

    // ── the secret does not leak ──────────────────────────────────────────

    #[test]
    fn the_secret_never_reaches_debug() {
        let verifier = HttpCaptchaVerifier::with_transport(
            CaptchaProvider::turnstile(),
            SecretString::new("the-server-secret".to_owned()),
            Arc::new(RustlsTransport::new().expect("a transport")),
        );

        let printed = format!("{verifier:?}");
        assert!(!printed.contains("the-server-secret"), "{printed}");
        assert!(printed.contains("turnstile"), "{printed}");

        // …nor through the request, which is what a trace-level log would print.
        let request = HttpRequest::form(
            CaptchaProvider::turnstile().verify_url(),
            verifier.body("token", None),
        );
        assert!(!format!("{request:?}").contains("the-server-secret"));
    }

    #[test]
    fn a_verify_url_that_is_not_a_url_is_refused_at_construction() {
        let error = CaptchaProvider::custom("edge", "siteverify").expect_err("not a URL");
        assert!(matches!(error, Error::Config(_)), "{error}");
        assert!(error.to_string().contains("siteverify"), "{error}");
    }

    #[test]
    fn the_three_bundled_providers_point_at_their_documented_endpoints() {
        for (provider, host) in [
            (CaptchaProvider::turnstile(), "challenges.cloudflare.com"),
            (CaptchaProvider::hcaptcha(), "api.hcaptcha.com"),
            (CaptchaProvider::recaptcha(), "www.google.com"),
        ] {
            assert!(
                provider.verify_url().starts_with("https://"),
                "{provider:?}"
            );
            assert!(provider.verify_url().contains(host), "{provider:?}");
        }
    }
}
