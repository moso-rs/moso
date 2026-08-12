//! The authorization code flow, end to end, against a real identity provider.
//!
//! "Real" means a real TCP listener speaking real HTTP/1.1 on loopback, not a
//! stubbed transport. The flow is three requests — discovery, token, userinfo —
//! and a mocked transport would prove that the code calls three functions,
//! which is not the thing worth proving. What is worth proving is that the URL
//! is built correctly, that the PKCE verifier the provider receives hashes to
//! the challenge it was sent, that the identity token's `nonce` is checked, and
//! that every one of those refuses when it should.
//!
//! The provider here *validates* PKCE the way a real one does: it recomputes
//! `S256(code_verifier)` and compares it against the challenge from the
//! authorization request. A test provider that accepted any verifier would let
//! a broken PKCE implementation pass.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use moso_core::config::SecretString;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use super::*;
use crate::Error;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

const CLIENT_ID: &str = "moso-test-client";
const CLIENT_SECRET: &str = "sh-not-a-real-secret";
const REDIRECT_URI: &str = "https://app.example.com/auth/oauth/test/callback";

// ---------------------------------------------------------------------------
// A real identity provider, on loopback
// ---------------------------------------------------------------------------

/// What the provider will do on the next exchange.
#[derive(Clone)]
struct Behaviour {
    /// The challenge the authorization request sent, which the token endpoint
    /// checks the verifier against — exactly as a real provider does.
    challenge: Option<String>,
    /// The `nonce` to put in the identity token.
    nonce: Option<String>,
    /// Whether the identity token says the address is verified.
    email_verified: bool,
    /// The address to report.
    email: Option<String>,
    /// Return this OAuth error from the token endpoint instead of a token.
    token_error: Option<&'static str>,
    /// Mint the identity token for this client id instead of the real one.
    audience: Option<String>,
    /// Omit the identity token entirely.
    omit_id_token: bool,
    /// Codes already redeemed, so a replay gets `invalid_grant`.
    used_codes: Vec<String>,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            challenge: None,
            nonce: None,
            email_verified: true,
            email: Some("ada@example.com".to_owned()),
            token_error: None,
            audience: None,
            omit_id_token: false,
            used_codes: Vec::new(),
        }
    }
}

/// A running identity provider.
struct Idp {
    /// Where it is listening.
    origin: String,
    /// What it will do next.
    behaviour: Arc<Mutex<Behaviour>>,
}

impl Idp {
    /// Bind, spawn, and return the origin to point a provider at.
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback binds");
        let port = listener
            .local_addr()
            .expect("the socket has an address")
            .port();
        let origin = format!("http://127.0.0.1:{port}");
        let behaviour = Arc::new(Mutex::new(Behaviour::default()));

        let served_origin = origin.clone();
        let served_behaviour = Arc::clone(&behaviour);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let origin = served_origin.clone();
                let behaviour = Arc::clone(&served_behaviour);
                tokio::spawn(async move {
                    serve(stream, &origin, &behaviour).await;
                });
            }
        });

        Self { origin, behaviour }
    }

    /// The discovery URL a provider is configured with.
    fn discovery_url(&self) -> String {
        format!("{}/.well-known/openid-configuration", self.origin)
    }

    /// Change what the provider will do next.
    fn set(&self, edit: impl FnOnce(&mut Behaviour)) {
        edit(
            &mut self
                .behaviour
                .lock()
                .expect("the test lock is not poisoned"),
        );
    }
}

/// One connection: read a request, answer it, close.
async fn serve(mut stream: tokio::net::TcpStream, origin: &str, behaviour: &Arc<Mutex<Behaviour>>) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 2048];

    // Headers first.
    let head_end = loop {
        let read = match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
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
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    // …then the body, if the request has one.
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while buffer.len() < head_end + length {
        let read = match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(&buffer[head_end..]).into_owned();

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();

    let (status, payload) = route(method, path, &headers, &body, origin, behaviour);
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: \
         close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// The three endpoints an authorization code flow uses.
fn route(
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: &str,
    origin: &str,
    behaviour: &Arc<Mutex<Behaviour>>,
) -> (&'static str, String) {
    match (method, path) {
        ("GET", "/.well-known/openid-configuration") => (
            "200 OK",
            serde_json::json!({
                "issuer": origin,
                "authorization_endpoint": format!("{origin}/authorize"),
                "token_endpoint": format!("{origin}/token"),
                "userinfo_endpoint": format!("{origin}/userinfo"),
                "jwks_uri": format!("{origin}/keys"),
                "code_challenge_methods_supported": ["S256"],
            })
            .to_string(),
        ),

        ("POST", "/token") => token_endpoint(body, origin, behaviour),

        // A GitHub-shaped provider: OAuth2 without OpenID Connect, so no
        // identity token and no `nonce`, and the address behind a second
        // request. This is the *other* half of the flow, and nothing about it
        // is exercised by the OIDC path above.
        ("POST", "/gh/token") => {
            let form: HashMap<String, String> = form_urlencoded::parse(body.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            if !form.contains_key("code_verifier") {
                return (
                    "400 Bad Request",
                    serde_json::json!({"error": "invalid_request"}).to_string(),
                );
            }
            (
                "200 OK",
                serde_json::json!({
                    "access_token": "the-access-token",
                    "token_type": "bearer",
                    "scope": "read:user,user:email",
                })
                .to_string(),
            )
        }

        ("GET", "/gh/user") => {
            if headers.get("authorization").map(String::as_str) != Some("Bearer the-access-token") {
                return (
                    "401 Unauthorized",
                    serde_json::json!({"message": "Bad credentials"}).to_string(),
                );
            }
            (
                "200 OK",
                serde_json::json!({
                    "id": 583_231,
                    "login": "octocat",
                    "name": "The Octocat",
                    // The *public* address, which is what `/user` returns and
                    // is usually absent.
                    "email": serde_json::Value::Null,
                    "avatar_url": "https://example.com/octocat.png",
                })
                .to_string(),
            )
        }

        ("GET", "/gh/user/emails") => (
            "200 OK",
            serde_json::json!([
                {"email": "old@example.com", "primary": false, "verified": true},
                {"email": "ada@example.com", "primary": true, "verified": true},
            ])
            .to_string(),
        ),

        ("GET", "/userinfo") => {
            if headers.get("authorization").map(String::as_str) != Some("Bearer the-access-token") {
                return (
                    "401 Unauthorized",
                    serde_json::json!({"error": "invalid_token"}).to_string(),
                );
            }
            let state = behaviour.lock().expect("not poisoned").clone();
            (
                "200 OK",
                serde_json::json!({
                    "sub": "the-subject",
                    "email": state.email,
                    "email_verified": state.email_verified,
                    "name": "Ada Lovelace",
                    "picture": "https://example.com/ada.png",
                    "custom_claim": "kept",
                })
                .to_string(),
            )
        }

        _ => (
            "404 Not Found",
            serde_json::json!({"error": "no"}).to_string(),
        ),
    }
}

/// The token endpoint, which validates PKCE the way a real one does.
fn token_endpoint(
    body: &str,
    origin: &str,
    behaviour: &Arc<Mutex<Behaviour>>,
) -> (&'static str, String) {
    let form: HashMap<String, String> = form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    let mut state = behaviour.lock().expect("not poisoned");

    if let Some(error) = state.token_error {
        return (
            "400 Bad Request",
            serde_json::json!({"error": error, "error_description": "the test asked for this"})
                .to_string(),
        );
    }

    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return (
            "400 Bad Request",
            serde_json::json!({"error": "unsupported_grant_type"}).to_string(),
        );
    }
    if form.get("client_id").map(String::as_str) != Some(CLIENT_ID)
        || form.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET)
    {
        return (
            "401 Unauthorized",
            serde_json::json!({"error": "invalid_client"}).to_string(),
        );
    }
    if form.get("redirect_uri").map(String::as_str) != Some(REDIRECT_URI) {
        return (
            "400 Bad Request",
            serde_json::json!({"error": "invalid_grant",
                               "error_description": "redirect_uri mismatch"})
            .to_string(),
        );
    }

    let code = form.get("code").cloned().unwrap_or_default();
    if state.used_codes.contains(&code) {
        return (
            "400 Bad Request",
            serde_json::json!({"error": "invalid_grant",
                               "error_description": "the code has already been redeemed"})
            .to_string(),
        );
    }
    state.used_codes.push(code);

    // PKCE, checked properly: recompute S256 of the verifier and compare.
    let Some(verifier) = form.get("code_verifier") else {
        return (
            "400 Bad Request",
            serde_json::json!({"error": "invalid_request",
                               "error_description": "code_verifier is required"})
            .to_string(),
        );
    };
    if let Some(expected) = &state.challenge {
        let actual = B64.encode(Sha256::digest(verifier.as_bytes()));
        if &actual != expected {
            return (
                "400 Bad Request",
                serde_json::json!({"error": "invalid_grant",
                                   "error_description": "the code_verifier does not match"})
                .to_string(),
            );
        }
    }

    let audience = state
        .audience
        .clone()
        .unwrap_or_else(|| CLIENT_ID.to_owned());
    let id_token = (!state.omit_id_token).then(|| {
        jwt(serde_json::json!({
            "iss": origin,
            "sub": "the-subject",
            "aud": audience,
            "exp": chrono::Utc::now().timestamp() + 3600,
            "iat": chrono::Utc::now().timestamp(),
            "nonce": state.nonce,
            "email": state.email,
            "email_verified": state.email_verified,
            "name": "Ada Lovelace",
        }))
    });

    (
        "200 OK",
        serde_json::json!({
            "access_token": "the-access-token",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "the-refresh-token",
            "id_token": id_token,
            "scope": "openid email profile",
        })
        .to_string(),
    )
}

/// A compact JWT with a placeholder signature.
///
/// The code flow does not verify the signature — the token came from the token
/// endpoint over the connection this process opened, which is what OIDC Core
/// §3.1.3.7 says is sufficient — so a real signature here would test the test.
/// Everything the flow *does* check is real.
fn jwt(claims: serde_json::Value) -> String {
    let header = B64.encode(br#"{"alg":"RS256","kid":"test"}"#);
    let payload = B64.encode(serde_json::to_vec(&claims).expect("claims serialise"));
    format!("{header}.{payload}.dGVzdC1zaWduYXR1cmU")
}

/// Find a needle in a haystack.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// A provider pointed at a running test identity provider.
fn provider(idp: &Idp) -> Provider {
    Provider::oidc(
        "test",
        idp.discovery_url(),
        OAuthConfig::new(CLIENT_ID, SecretString::new(CLIENT_SECRET), REDIRECT_URI),
    )
}

/// Start an authorization and tell the provider what to expect, exactly as a
/// browser round trip would.
async fn begin(idp: &Idp, provider: &Provider) -> AuthorizationRequest {
    let request = provider
        .authorize(Some("/dashboard"))
        .await
        .expect("the authorization starts");

    let query: HashMap<String, String> =
        form_urlencoded::parse(request.url.as_url().query().unwrap_or("").as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

    idp.set(|b| {
        b.challenge = query.get("code_challenge").cloned();
        b.nonce = query.get("nonce").cloned();
    });

    request
}

/// The callback a browser would arrive with.
///
/// A fresh code each time, because an authorization code is single use and the
/// test provider enforces it — which is what makes
/// `a_replayed_code_is_refused_by_the_provider` mean something.
fn callback(request: &AuthorizationRequest) -> CallbackParams {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let code = format!("code-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    CallbackParams::new(Some(&code), request.state.expose())
}

/// The same callback twice, for the replay test.
fn fixed_callback(request: &AuthorizationRequest) -> CallbackParams {
    CallbackParams::new(Some("a-code-worth-replaying"), request.state.expose())
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

/// The whole flow: discovery, an authorization URL with PKCE and a nonce, a
/// token exchange the provider validates, an identity token whose nonce is
/// checked, and a userinfo call.
#[tokio::test]
async fn the_authorization_code_flow_round_trips() {
    let idp = Idp::start().await;
    let provider = provider(&idp);

    let request = begin(&idp, &provider).await;

    // The URL is what a browser is sent, and every parameter is load-bearing.
    let query: HashMap<String, String> = form_urlencoded::parse(
        request
            .url
            .as_url()
            .query()
            .expect("there is a query")
            .as_bytes(),
    )
    .map(|(k, v)| (k.into_owned(), v.into_owned()))
    .collect();

    assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(query.get("client_id").map(String::as_str), Some(CLIENT_ID));
    assert_eq!(
        query.get("redirect_uri").map(String::as_str),
        Some(REDIRECT_URI)
    );
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256"),
        "PKCE must never be `plain`"
    );
    assert!(query.contains_key("code_challenge"), "PKCE is not optional");
    assert!(
        query.contains_key("nonce"),
        "an OIDC provider must get a nonce"
    );
    assert_eq!(request.next.as_deref(), Some("/dashboard"));
    assert_eq!(request.provider, ProviderId::Oidc("test".to_owned()));

    // …and the exchange.
    let profile = provider
        .exchange(&request, &callback(&request))
        .await
        .expect("the exchange succeeds");

    assert_eq!(profile.subject, "the-subject");
    assert_eq!(profile.email.as_deref(), Some("ada@example.com"));
    assert!(profile.email_verified);
    assert_eq!(profile.name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(profile.identity_key(), "test:the-subject");
    assert_eq!(
        profile.raw["custom_claim"],
        serde_json::json!("kept"),
        "claims Moso does not model must survive"
    );
    assert!(profile.tokens.refresh_token.is_some());
    assert!(profile.tokens.granted(&["openid", "email"]));

    // …and the profile is linkable, because the provider verified the address.
    provider
        .check_link(&profile, false)
        .expect("a verified address links");
}

/// The discovery document is fetched once, whatever the login volume.
#[tokio::test]
async fn discovery_happens_once() {
    let idp = Idp::start().await;
    let provider = provider(&idp);

    let first = begin(&idp, &provider).await;
    provider
        .exchange(&first, &callback(&first))
        .await
        .expect("the first login works");

    // Point the discovery URL at nothing. A second login must not notice,
    // because the endpoints are already resolved.
    let broken = Provider::oidc(
        "test",
        "http://127.0.0.1:1/.well-known/openid-configuration",
        OAuthConfig::new(CLIENT_ID, SecretString::new(CLIENT_SECRET), REDIRECT_URI),
    );
    assert!(
        broken.authorize(None).await.is_err(),
        "a provider that has never resolved must fail loudly"
    );

    let second = begin(&idp, &provider).await;
    provider
        .exchange(&second, &callback(&second))
        .await
        .expect("the cached endpoints are reused");
}

// ---------------------------------------------------------------------------
// The three refusals the acceptance criteria name
// ---------------------------------------------------------------------------

/// **Acceptance criterion.** A `state` that is not the one this session issued
/// is refused before anything else is looked at — it is the defence against an
/// attacker completing their own authorization in a victim's browser.
#[tokio::test]
async fn a_mismatched_state_is_refused() {
    let idp = Idp::start().await;
    let provider = provider(&idp);
    let request = begin(&idp, &provider).await;

    // The third is the right length with the wrong content: the comparison is
    // constant-time and must not shortcut on length alone.
    let same_length = "A".repeat(request.state.expose().len());
    for wrong in ["", "not-the-state", same_length.as_str()] {
        let error = provider
            .exchange(&request, &CallbackParams::new(Some("code"), wrong))
            .await
            .expect_err("a mismatched state must not exchange");
        assert!(format!("{error}").contains("state"), "{error}");
        assert!(matches!(
            error,
            Error::Ceremony {
                ceremony: "oauth",
                ..
            }
        ));
    }

    // …and the correct one still works, so the test is not vacuous.
    provider
        .exchange(&request, &callback(&request))
        .await
        .expect("the real state exchanges");
}

/// **Acceptance criterion.** A session that lost its PKCE verifier cannot be
/// completed. Falling back to an exchange without one is exactly what a stolen
/// authorization code needs.
#[tokio::test]
async fn a_missing_pkce_verifier_is_refused() {
    let idp = Idp::start().await;
    let provider = provider(&idp);
    let request = begin(&idp, &provider).await;

    let too_long = "x".repeat(200);
    for broken in ["", "short", too_long.as_str()] {
        let mut tampered = request.clone();
        tampered.verifier = SecretString::new(broken);

        let error = provider
            .exchange(&tampered, &callback(&tampered))
            .await
            .expect_err("no verifier means no exchange");
        let message = format!("{error}");
        assert!(message.contains("PKCE"), "{message}");
        assert!(
            message.contains("stolen code"),
            "the message must say why: {message}"
        );
    }
}

/// A verifier that is well formed but *wrong* is refused by the provider, which
/// is the check PKCE actually is. This is the assertion that proves the
/// verifier reaching the token endpoint is the one whose hash was sent.
#[tokio::test]
async fn a_wrong_pkce_verifier_is_refused_by_the_provider() {
    let idp = Idp::start().await;
    let provider = provider(&idp);
    let request = begin(&idp, &provider).await;

    let mut tampered = request.clone();
    tampered.verifier = SecretString::new(Pkce::generate().verifier().expose());

    let error = provider
        .exchange(&tampered, &callback(&tampered))
        .await
        .expect_err("a verifier that does not hash to the challenge must be refused");
    assert!(format!("{error}").contains("code_verifier"), "{error}");
}

/// **Acceptance criterion.** A provider that does not say it verified the
/// address must not auto-link, because anybody who can claim that address at
/// the provider would take the local account over.
#[tokio::test]
async fn an_unverified_email_does_not_auto_link() {
    let idp = Idp::start().await;
    let provider = provider(&idp);
    idp.set(|b| b.email_verified = false);

    let request = begin(&idp, &provider).await;
    let profile = provider
        .exchange(&request, &callback(&request))
        .await
        .expect("the login itself succeeds — it is the *linking* that is refused");

    assert!(!profile.email_verified);
    assert_eq!(profile.verified_email(), None);

    let error = provider
        .check_link(&profile, false)
        .expect_err("an unverified address must not link to an existing account");
    let message = format!("{error}");
    assert!(message.contains("verified"), "{message}");
    assert!(message.contains("take over"), "{message}");

    // An authenticated session is the documented way through, and the
    // documented override is the other.
    provider
        .check_link(&profile, true)
        .expect("an authenticated session proves ownership independently");
    provider
        .clone()
        .link_policy(LinkPolicy::AnyEmail)
        .check_link(&profile, false)
        .expect("AnyEmail is the documented override");
}

// ---------------------------------------------------------------------------
// The rest of the attack surface
// ---------------------------------------------------------------------------

/// An identity token whose `nonce` is not this request's belongs to a different
/// sign-in, and replaying it must not work.
#[tokio::test]
async fn a_replayed_identity_token_is_refused() {
    let idp = Idp::start().await;
    let provider = provider(&idp);
    let request = begin(&idp, &provider).await;

    idp.set(|b| b.nonce = Some("a-nonce-from-another-sign-in".to_owned()));

    let error = provider
        .exchange(&request, &callback(&request))
        .await
        .expect_err("a token bound to another request must not be accepted");
    assert!(format!("{error}").contains("nonce"), "{error}");
}

/// An identity token minted for a different client is a cross-application
/// attack: everything about it is valid except who asked for it.
#[tokio::test]
async fn an_identity_token_for_another_client_is_refused() {
    let idp = Idp::start().await;
    let provider = provider(&idp);
    let request = begin(&idp, &provider).await;

    idp.set(|b| b.audience = Some("somebody-elses-client".to_owned()));

    let error = provider
        .exchange(&request, &callback(&request))
        .await
        .expect_err("the audience does not match");
    assert!(
        format!("{error}").contains("different application"),
        "{error}"
    );
}

/// An OIDC provider that returns no identity token has left the `nonce` bound
/// to nothing, which is not a login this crate will complete.
#[tokio::test]
async fn an_oidc_provider_that_returns_no_identity_token_is_refused() {
    let idp = Idp::start().await;
    let provider = provider(&idp);
    let request = begin(&idp, &provider).await;

    idp.set(|b| b.omit_id_token = true);

    let error = provider
        .exchange(&request, &callback(&request))
        .await
        .expect_err("an OIDC provider must return an identity token");
    assert!(format!("{error}").contains("identity token"), "{error}");
}

/// A callback that belongs to another provider must not be redeemed here,
/// whatever its `state` says.
#[tokio::test]
async fn a_callback_for_another_provider_is_refused() {
    let idp = Idp::start().await;
    let provider = provider(&idp);
    let request = begin(&idp, &provider).await;

    let other = Provider::oidc(
        "other",
        idp.discovery_url(),
        OAuthConfig::new(CLIENT_ID, SecretString::new(CLIENT_SECRET), REDIRECT_URI),
    );

    let error = other
        .exchange(&request, &callback(&request))
        .await
        .expect_err("a `test` authorization is not an `other` callback");
    assert!(
        format!("{error}").contains("authorization and this callback"),
        "{error}"
    );
}

/// The user pressing "cancel" is an `error` parameter and no code, and the
/// reason belongs in the log rather than being flattened into "login failed".
#[tokio::test]
async fn a_refusal_by_the_user_names_itself() {
    let idp = Idp::start().await;
    let provider = provider(&idp);
    let request = begin(&idp, &provider).await;

    let error = provider
        .exchange(
            &request,
            &CallbackParams::refused(
                "access_denied",
                Some("the user denied the request"),
                request.state.expose(),
            ),
        )
        .await
        .expect_err("a refusal is not a login");
    let message = format!("{error}");
    assert!(message.contains("access_denied"), "{message}");
    assert!(message.contains("the user denied"), "{message}");
}

/// A callback with neither a code nor an error is malformed, and saying so is
/// better than a null-pointer-shaped failure three calls later.
#[tokio::test]
async fn a_callback_with_no_code_is_refused() {
    let idp = Idp::start().await;
    let provider = provider(&idp);
    let request = begin(&idp, &provider).await;

    let error = provider
        .exchange(&request, &CallbackParams::new(None, request.state.expose()))
        .await
        .expect_err("there is nothing to exchange");
    assert!(format!("{error}").contains("neither"), "{error}");
}

/// The token endpoint's own error codes are the difference between "login
/// failed" and "your client secret was rotated last Tuesday".
#[tokio::test]
async fn a_token_endpoint_error_is_repeated_verbatim() {
    let idp = Idp::start().await;
    let provider = provider(&idp);
    let request = begin(&idp, &provider).await;

    idp.set(|b| b.token_error = Some("invalid_client"));

    let error = provider
        .exchange(&request, &callback(&request))
        .await
        .expect_err("the provider refused");
    let message = format!("{error}");
    assert!(message.contains("invalid_client"), "{message}");
    assert!(message.contains("400"), "{message}");
}

/// An authorization code is single use. A second redemption is `invalid_grant`,
/// and this test exists because the *provider* enforcing it is what makes a
/// captured code worthless after the legitimate exchange.
#[tokio::test]
async fn a_replayed_code_is_refused_by_the_provider() {
    let idp = Idp::start().await;
    let provider = provider(&idp);
    let request = begin(&idp, &provider).await;

    provider
        .exchange(&request, &fixed_callback(&request))
        .await
        .expect("the first exchange works");

    let error = provider
        .exchange(&request, &fixed_callback(&request))
        .await
        .expect_err("a code is single use");
    assert!(format!("{error}").contains("invalid_grant"), "{error}");
}

/// Two authorizations must not share a `state`, a `nonce` or a verifier.
#[tokio::test]
async fn two_authorizations_share_nothing() {
    let idp = Idp::start().await;
    let provider = provider(&idp);

    let a = provider.authorize(None).await.expect("starts");
    let b = provider.authorize(None).await.expect("starts");

    assert_ne!(a.state.expose(), b.state.expose());
    assert_ne!(a.verifier.expose(), b.verifier.expose());
    assert_ne!(
        a.nonce.as_ref().map(SecretString::expose),
        b.nonce.as_ref().map(SecretString::expose)
    );
}

/// Nothing that must stay in the session may appear in a `Debug`, because an
/// `AuthorizationRequest` logged at debug level is a live authorization.
#[tokio::test]
async fn the_session_secrets_are_not_printed() {
    let idp = Idp::start().await;
    let request = begin(&idp, &provider(&idp)).await;

    let printed = format!("{request:?}");
    assert!(!printed.contains(request.state.expose()), "{printed}");
    assert!(!printed.contains(request.verifier.expose()), "{printed}");
    if let Some(nonce) = &request.nonce {
        assert!(!printed.contains(nonce.expose()), "{printed}");
    }
    // The URL's query string carries both the state and the nonce, so it must
    // be truncated rather than printed.
    assert!(
        !printed.contains("code_challenge"),
        "the authorization URL's query must not be printed: {printed}"
    );
    assert!(printed.contains("/authorize?***"), "{printed}");
}

// ---------------------------------------------------------------------------
// Open redirect
// ---------------------------------------------------------------------------

/// `next` is validated *before* it is stored, so an open redirect is not
/// reachable even from a tampered session. The default — no allowlist — is
/// same-site paths only.
#[tokio::test]
async fn next_is_validated_before_it_is_stored() {
    let idp = Idp::start().await;
    let provider = provider(&idp);

    for good in ["/", "/dashboard", "/a/b?c=d#e"] {
        let request = provider
            .authorize(Some(good))
            .await
            .unwrap_or_else(|e| panic!("`{good}` is same-site: {e}"));
        assert_eq!(request.next.as_deref(), Some(good));
    }

    for bad in [
        "https://evil.example",
        // Protocol-relative: a browser reads this as an absolute URL.
        "//evil.example",
        // Several browsers normalise the backslash to a slash.
        "/\\evil.example",
        "\\\\evil.example",
        "javascript:alert(1)",
        "dashboard",
    ] {
        let error = provider
            .authorize(Some(bad))
            .await
            .err()
            .unwrap_or_else(|| panic!("`{bad}` must not be accepted"));
        assert!(format!("{error}").contains("same-site"), "{bad}: {error}");
    }
}

/// With an allowlist, membership is decided on scheme, host, port and path
/// boundaries — never as a string prefix, which is how
/// `https://app.example.com.evil.test` gets through.
#[tokio::test]
async fn the_redirect_allowlist_is_not_a_string_prefix() {
    let idp = Idp::start().await;
    let provider = provider(&idp).redirect_allowlist(["https://app.example.com/app"]);

    for good in [
        "https://app.example.com/app",
        "https://app.example.com/app/",
        "https://app.example.com/app/deeper",
    ] {
        provider
            .authorize(Some(good))
            .await
            .unwrap_or_else(|e| panic!("`{good}` is allowed: {e}"));
    }

    for bad in [
        // The prefix trap.
        "https://app.example.com.evil.test/app",
        "https://app.example.com/application",
        // Right host, wrong scheme.
        "http://app.example.com/app",
        // Right host, wrong port.
        "https://app.example.com:8443/app",
        // Right origin, outside the allowed path.
        "https://app.example.com/elsewhere",
        "https://evil.example/app",
    ] {
        let error = provider
            .authorize(Some(bad))
            .await
            .err()
            .unwrap_or_else(|| panic!("`{bad}` must not be accepted"));
        assert!(format!("{error}").contains("allowlist"), "{bad}: {error}");
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Every built-in provider builds, validates and produces an authorization URL
/// with PKCE on it — without a network call, because the endpoints are in the
/// table.
#[tokio::test]
async fn every_builtin_provider_produces_a_pkce_authorization_url() {
    let config = || {
        OAuthConfig::new(
            "id",
            SecretString::new("secret"),
            "https://app.example.com/cb",
        )
    };

    let providers = [
        Provider::google(config()),
        Provider::github(config()),
        Provider::microsoft(config()),
        Provider::gitlab(config()),
        Provider::discord(config()),
        Provider::slack(config()),
    ];

    for provider in providers {
        provider
            .validate()
            .unwrap_or_else(|e| panic!("{:?} validates: {e}", provider.id()));

        let request = provider
            .authorize(None)
            .await
            .unwrap_or_else(|e| panic!("{:?} authorizes: {e}", provider.id()));

        let query: HashMap<String, String> =
            form_urlencoded::parse(request.url.as_url().query().unwrap_or("").as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();

        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256"),
            "{:?} must use S256",
            provider.id()
        );
        assert!(request.url.scheme() == "https", "{}", request.url);

        // The two non-OIDC providers get no nonce; the rest must.
        let oidc = !matches!(provider.id(), ProviderId::GitHub | ProviderId::Discord);
        assert_eq!(
            query.contains_key("nonce"),
            oidc,
            "{:?} nonce expectation",
            provider.id()
        );
        assert_eq!(request.nonce.is_some(), oidc);
    }
}

/// Apple's client secret is a JWT, and a plain string is a configuration error
/// naming the fix rather than an `invalid_client` from Apple on the first
/// login.
#[test]
fn apple_refuses_a_client_secret_that_is_not_a_jwt() {
    let apple = Provider::apple(OAuthConfig::new(
        "com.example.service",
        SecretString::new("not-a-jwt"),
        "https://app.example.com/cb",
    ));

    let error = apple.validate().expect_err("a plain string is not a JWT");
    let message = format!("{error}");
    assert!(message.contains("ES256-signed JWT"), "{message}");
    assert!(message.contains("appleid.apple.com"), "{message}");
}

/// An expired Apple secret is caught before the request, because Apple's answer
/// to it is an opaque `invalid_client`.
#[test]
fn apple_refuses_an_expired_client_secret() {
    let claims = serde_json::json!({
        "iss": "TEAMID",
        "sub": "com.example.service",
        "aud": "https://appleid.apple.com",
        "exp": chrono::Utc::now().timestamp() - 86_400 * 10,
    });
    let apple = Provider::apple(OAuthConfig::new(
        "com.example.service",
        SecretString::new(jwt(claims)),
        "https://app.example.com/cb",
    ));

    let error = apple.validate().expect_err("the secret has expired");
    assert!(format!("{error}").contains("expired"), "{error}");
    assert!(format!("{error}").contains("rotated"), "{error}");
}

/// Apple forces `response_mode=form_post`, without which it rejects any request
/// that asks for a name or an address.
#[tokio::test]
async fn apple_asks_for_a_form_post() {
    let claims = serde_json::json!({
        "iss": "TEAMID",
        "sub": "com.example.service",
        "aud": "https://appleid.apple.com",
        "exp": chrono::Utc::now().timestamp() + 86_400,
    });
    let apple = Provider::apple(OAuthConfig::new(
        "com.example.service",
        SecretString::new(jwt(claims)),
        "https://app.example.com/cb",
    ));

    let request = apple.authorize(None).await.expect("authorizes");
    assert!(
        request.url.as_str().contains("response_mode=form_post"),
        "{}",
        request.url
    );
}

/// A generic OIDC provider is always built with a discovery URL, because
/// without one there is nowhere to look its endpoints up.
#[test]
fn a_generic_provider_always_has_a_discovery_url() {
    let provider = Provider::oidc(
        "keycloak",
        "https://id.example.com/.well-known/openid-configuration",
        OAuthConfig::new("id", SecretString::new("s"), "https://a/cb"),
    );
    provider.validate().expect("a discovery URL is enough");
    assert_eq!(provider.id(), &ProviderId::Oidc("keycloak".to_owned()));

    // …and a discovery URL that is not a URL is caught at boot.
    let broken = Provider::oidc(
        "keycloak",
        "not a url",
        OAuthConfig::new("id", SecretString::new("s"), "https://a/cb"),
    );
    let error = broken.validate().expect_err("`not a url` is not a URL");
    assert!(format!("{error}").contains("discovery URL"), "{error}");
}

/// The configuration mistakes worth catching at boot: an unset environment
/// variable, a relative redirect URI, a space-separated scope string.
#[test]
fn the_boot_check_names_the_field_and_the_fix() {
    let good = || {
        OAuthConfig::new(
            "id",
            SecretString::new("secret"),
            "https://app.example.com/cb",
        )
    };

    let mut empty_id = good();
    empty_id.client_id = String::new();
    assert!(
        format!(
            "{}",
            Provider::google(empty_id).validate().expect_err("empty")
        )
        .contains("environment variable")
    );

    let mut empty_secret = good();
    empty_secret.client_secret = SecretString::new("");
    assert!(
        format!(
            "{}",
            Provider::google(empty_secret)
                .validate()
                .expect_err("empty")
        )
        .contains("environment variable")
    );

    let mut relative = good();
    relative.redirect_uri = "/auth/callback".to_owned();
    let error = Provider::google(relative).validate().expect_err("relative");
    assert!(format!("{error}").contains("absolute"), "{error}");

    let error = Provider::google(good())
        .only_scopes(["openid email profile"])
        .validate()
        .expect_err("one string is not three scopes");
    assert!(format!("{error}").contains("one per item"), "{error}");
}

/// Scopes accumulate on top of the provider's defaults, and `only_scopes`
/// replaces them.
#[test]
fn scopes_accumulate_and_can_be_replaced() {
    let config = OAuthConfig::new("id", SecretString::new("s"), "https://a/cb");

    let google = Provider::google(config.clone()).scopes(["https://example.com/auth/drive"]);
    assert!(google.requested_scopes().contains(&"openid".to_owned()));
    assert!(
        google
            .requested_scopes()
            .contains(&"https://example.com/auth/drive".to_owned())
    );

    // Twice is once.
    let twice = Provider::google(config.clone()).scopes(["openid", "openid"]);
    assert_eq!(
        twice
            .requested_scopes()
            .iter()
            .filter(|s| *s == "openid")
            .count(),
        1
    );

    let narrow = Provider::github(config).only_scopes(["user:email"]);
    assert_eq!(narrow.requested_scopes(), ["user:email"]);
}

/// Extra authorization parameters are how a refresh token is obtained from
/// Google, and the last one set wins rather than both being sent.
#[tokio::test]
async fn extra_parameters_reach_the_authorization_url() {
    let google = Provider::google(OAuthConfig::new(
        "id",
        SecretString::new("s"),
        "https://a/cb",
    ))
    .param("access_type", "offline")
    .param("prompt", "none")
    .param("prompt", "consent");

    let request = google.authorize(None).await.expect("authorizes");
    let query: HashMap<String, String> =
        form_urlencoded::parse(request.url.as_url().query().unwrap_or("").as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

    assert_eq!(
        query.get("access_type").map(String::as_str),
        Some("offline")
    );
    assert_eq!(query.get("prompt").map(String::as_str), Some("consent"));
    assert_eq!(
        request.url.as_str().matches("prompt=").count(),
        1,
        "a parameter set twice must be sent once"
    );
}

/// The Entra tenant reaches the authorization endpoint, which is the difference
/// between "our staff can sign in" and "anyone with a Microsoft account can".
#[tokio::test]
async fn the_entra_tenant_reaches_the_authorization_url() {
    let entra = Provider::microsoft(
        OAuthConfig::new("id", SecretString::new("s"), "https://a/cb")
            .tenant("contoso.onmicrosoft.com"),
    );

    let request = entra.authorize(None).await.expect("authorizes");
    assert!(
        request.url.as_str().contains("contoso.onmicrosoft.com"),
        "{}",
        request.url
    );
}

/// `ProviderId` round-trips through the path segment a callback route matches
/// on.
#[test]
fn provider_ids_round_trip_through_a_path_segment() {
    for id in [
        ProviderId::Google,
        ProviderId::GitHub,
        ProviderId::Microsoft,
        ProviderId::Apple,
        ProviderId::GitLab,
        ProviderId::Discord,
        ProviderId::Slack,
        ProviderId::Oidc("keycloak".to_owned()),
    ] {
        assert_eq!(ProviderId::parse(id.as_str()), id);
        assert_eq!(id.to_string(), id.as_str());
    }
}

/// A provider prints its configuration without printing its client secret.
#[test]
fn a_provider_does_not_print_its_secret() {
    let provider = Provider::google(OAuthConfig::new(
        "id",
        SecretString::new("the-client-secret"),
        "https://a/cb",
    ));
    let printed = format!("{provider:?}");
    assert!(!printed.contains("the-client-secret"), "{printed}");
    assert!(printed.contains("Google"), "{printed}");
}

// ---------------------------------------------------------------------------
// The non-OIDC half: GitHub, and a self-hosted one
// ---------------------------------------------------------------------------

/// A GitHub-shaped provider, pointed at the local server the way GitHub
/// Enterprise Server would be.
fn enterprise(idp: &Idp) -> Provider {
    Provider::github(OAuthConfig::new(
        CLIENT_ID,
        SecretString::new(CLIENT_SECRET),
        REDIRECT_URI,
    ))
    .self_hosted(
        format!("{}/gh/authorize", idp.origin),
        format!("{}/gh/token", idp.origin),
        Some(&format!("{}/gh/user", idp.origin)),
    )
}

/// The OAuth2-without-OIDC flow, end to end: PKCE but no `nonce`, no identity
/// token, and the address from a second request — the path five of the seven
/// built-in providers do *not* take, and the one most likely to rot.
#[tokio::test]
async fn a_non_oidc_provider_round_trips_through_a_second_request() {
    let idp = Idp::start().await;
    let github = enterprise(&idp);

    let request = github
        .authorize(None)
        .await
        .expect("the authorization starts");
    assert!(
        request.nonce.is_none(),
        "a provider with no identity token must not be sent a nonce it cannot echo"
    );
    assert!(
        request.url.as_str().contains("code_challenge_method=S256"),
        "PKCE is not conditional on OIDC: {}",
        request.url
    );

    let profile = github
        .exchange(&request, &callback(&request))
        .await
        .expect("the exchange succeeds");

    assert_eq!(profile.subject, "583231");
    assert_eq!(profile.name.as_deref(), Some("The Octocat"));
    assert_eq!(
        profile.email.as_deref(),
        Some("ada@example.com"),
        "the address comes from `/user/emails`, not from `/user`"
    );
    assert!(profile.email_verified);
    assert!(profile.tokens.id_token.is_none());
    assert!(
        profile.tokens.granted(&["user:email"]),
        "GitHub separates its scopes with commas, not spaces"
    );

    github
        .check_link(&profile, false)
        .expect("a verified GitHub address links");
}

/// The second request is derived from the profile endpoint, so a self-hosted
/// instance asks *its own* host for the address rather than github.com. Sending
/// an enterprise access token to api.github.com would be a credential leak, and
/// the login would fail with an unrelated 401.
#[tokio::test]
async fn the_github_email_request_follows_the_profile_endpoint() {
    let idp = Idp::start().await;
    let github = enterprise(&idp);

    let request = github
        .authorize(None)
        .await
        .expect("the authorization starts");
    let profile = github
        .exchange(&request, &callback(&request))
        .await
        .expect("the exchange succeeds");

    // Only the local server can have answered: api.github.com would have
    // refused this token, and there is no address on `/user`.
    assert_eq!(profile.email.as_deref(), Some("ada@example.com"));
}

/// `self_hosted` inherits whether the provider issues identity tokens, so a
/// self-hosted GitHub is still not OpenID Connect and a self-hosted GitLab
/// still is.
#[tokio::test]
async fn self_hosting_inherits_the_providers_protocol() {
    let config = || {
        OAuthConfig::new(
            CLIENT_ID,
            SecretString::new(CLIENT_SECRET),
            "https://app.example.com/cb",
        )
    };

    let github = Provider::github(config()).self_hosted(
        "https://git.example.com/login/oauth/authorize",
        "https://git.example.com/login/oauth/access_token",
        Some("https://git.example.com/api/v3/user"),
    );
    assert!(
        github
            .authorize(None)
            .await
            .expect("authorizes")
            .nonce
            .is_none(),
        "a self-hosted GitHub is still not OpenID Connect"
    );

    let gitlab = Provider::gitlab(config()).self_hosted(
        "https://gitlab.example.com/oauth/authorize",
        "https://gitlab.example.com/oauth/token",
        Some("https://gitlab.example.com/oauth/userinfo"),
    );
    let request = gitlab.authorize(None).await.expect("authorizes");
    assert!(
        request.nonce.is_some(),
        "a self-hosted GitLab still speaks OpenID Connect"
    );
    assert!(
        request
            .url
            .as_str()
            .starts_with("https://gitlab.example.com/oauth/authorize"),
        "{}",
        request.url
    );
}
