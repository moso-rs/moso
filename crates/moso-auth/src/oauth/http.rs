//! The outbound HTTP a social login needs, and the seam that lets a test — or
//! an application behind a proxy — replace it.
//!
//! An OAuth flow is three requests to somebody else's server: the discovery
//! document, the token endpoint, the userinfo endpoint. That is enough network
//! to need a policy, and the policy is here rather than scattered across the
//! provider table:
//!
//! - **`https` only**, except to a loopback address, which is what a local
//!   Keycloak or a test server is.
//! - **A total deadline** on every request, so a provider that hangs cannot
//!   hold a request handler open.
//! - **A response size cap**, so a provider that streams cannot exhaust memory.
//! - **Redirects are followed on `GET` only**, up to three. A redirected `POST`
//!   is how a client secret ends up somewhere it was not meant to go.
//!
//! The TLS stack is `rustls` on the **`ring`** provider, chosen explicitly and
//! not by default: `sqlx` has already installed ring in any process that has a
//! database, and two crypto providers in one process is a runtime panic rather
//! than a compile error.
//!
//! ```no_run
//! use moso_auth::oauth::http::{HttpRequest, HttpTransport, RustlsTransport};
//!
//! # async fn f() -> moso_auth::Result<()> {
//! let transport = RustlsTransport::new()?;
//! let response = transport
//!     .send(&HttpRequest::get("https://accounts.google.com/.well-known/openid-configuration"))
//!     .await?;
//! assert!(response.is_success());
//! # Ok(()) }
//! ```

use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt as _, Full, Limited};
use moso_core::BoxFuture;

use crate::{Error, Result};

/// How long a whole request may take, including connect and TLS.
///
/// ```
/// assert_eq!(moso_auth::oauth::http::DEFAULT_TIMEOUT.as_secs(), 10);
/// ```
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// How much of a response body is read before giving up.
///
/// A discovery document is a few kilobytes and a userinfo response is smaller;
/// a megabyte is four orders of magnitude of headroom and still bounded.
///
/// ```
/// assert_eq!(moso_auth::oauth::http::MAX_BODY, 1024 * 1024);
/// ```
pub const MAX_BODY: usize = 1024 * 1024;

/// How many redirects a `GET` follows.
///
/// ```
/// assert_eq!(moso_auth::oauth::http::MAX_REDIRECTS, 3);
/// ```
pub const MAX_REDIRECTS: usize = 3;

/// One outbound request.
///
/// ```
/// use moso_auth::oauth::http::HttpRequest;
///
/// let request = HttpRequest::get("https://example.com/.well-known/openid-configuration");
/// assert_eq!(request.method, "GET");
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub struct HttpRequest {
    /// `GET` or `POST`.
    pub method: &'static str,
    /// The absolute URL.
    pub url: String,
    /// Headers, in order. A `Debug` of this type prints them, so an
    /// `Authorization` header must be set through
    /// [`bearer`](Self::bearer) or [`basic`](Self::basic), which redact.
    pub headers: Vec<(String, String)>,
    /// The body, for a `POST`.
    pub body: Option<Vec<u8>>,
    /// Credentials to send, kept out of `headers` so they are not printed.
    authorization: Option<String>,
}

impl core::fmt::Debug for HttpRequest {
    /// Prints everything except the credentials. A request logged at trace
    /// level would otherwise be a client secret in a log aggregator, and a
    /// derived `Debug` prints private fields too.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &self.headers)
            .field("body", &self.body.as_ref().map(Vec::len))
            .field("authorization", &self.authorization.as_ref().map(|_| "***"))
            .finish()
    }
}

impl HttpRequest {
    /// A `GET`.
    ///
    /// ```
    /// use moso_auth::oauth::http::HttpRequest;
    ///
    /// assert_eq!(HttpRequest::get("https://example.com").method, "GET");
    /// ```
    #[must_use]
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET",
            url: url.into(),
            headers: vec![("accept".to_owned(), "application/json".to_owned())],
            body: None,
            authorization: None,
        }
    }

    /// A `POST` of a form body, which is what an OAuth token endpoint takes.
    ///
    /// ```
    /// use moso_auth::oauth::http::HttpRequest;
    ///
    /// let request = HttpRequest::form("https://example.com/token", "grant_type=x");
    /// assert_eq!(request.method, "POST");
    /// assert!(request.body.is_some());
    /// ```
    #[must_use]
    pub fn form(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            method: "POST",
            url: url.into(),
            headers: vec![
                (
                    "content-type".to_owned(),
                    "application/x-www-form-urlencoded".to_owned(),
                ),
                ("accept".to_owned(), "application/json".to_owned()),
            ],
            body: Some(body.into().into_bytes()),
            authorization: None,
        }
    }

    /// Add a header.
    ///
    /// ```
    /// use moso_auth::oauth::http::HttpRequest;
    ///
    /// let r = HttpRequest::get("https://example.com").header("user-agent", "moso");
    /// assert_eq!(r.headers.len(), 2);
    /// ```
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Send `Authorization: Bearer …`, without recording it where `Debug` can
    /// see it.
    ///
    /// ```
    /// use moso_auth::oauth::http::HttpRequest;
    ///
    /// let r = HttpRequest::get("https://example.com").bearer("secret-token");
    /// assert!(!format!("{r:?}").contains("secret-token"));
    /// ```
    #[must_use]
    pub fn bearer(mut self, token: &str) -> Self {
        self.authorization = Some(format!("Bearer {token}"));
        self
    }

    /// Send HTTP basic credentials, which is how a confidential OAuth client
    /// authenticates to a token endpoint that prefers it to form fields.
    ///
    /// ```
    /// use moso_auth::oauth::http::HttpRequest;
    ///
    /// let r = HttpRequest::form("https://example.com/token", "").basic("id", "shh");
    /// assert!(!format!("{r:?}").contains("shh"));
    /// ```
    #[must_use]
    pub fn basic(mut self, user: &str, password: &str) -> Self {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(
            format!(
                "{}:{}",
                form_urlencoded::byte_serialize(user.as_bytes()).collect::<String>(),
                form_urlencoded::byte_serialize(password.as_bytes()).collect::<String>()
            )
            .as_bytes(),
        );
        self.authorization = Some(format!("Basic {encoded}"));
        self
    }

    /// The `Authorization` header value, if any.
    fn authorization(&self) -> Option<&str> {
        self.authorization.as_deref()
    }
}

/// One response.
///
/// ```
/// use moso_auth::oauth::http::HttpResponse;
///
/// # fn f(r: &HttpResponse) {
/// let _ = r.status;
/// # }
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The body, capped at [`MAX_BODY`].
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Whether the status is 2xx.
    ///
    /// ```
    /// use moso_auth::oauth::http::HttpResponse;
    ///
    /// # fn f(r: &HttpResponse) { let _ = r.is_success(); }
    /// ```
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as text, lossily — it is going into a log message, not into a
    /// parser.
    ///
    /// ```
    /// use moso_auth::oauth::http::HttpResponse;
    ///
    /// # fn f(r: &HttpResponse) -> String { r.text() }
    /// ```
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Parse the body as JSON.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the provider sent something that is not the
    /// JSON it promised — with the first 200 bytes in the log, because
    /// "expected value at line 1 column 1" on its own says nothing about which
    /// HTML error page arrived.
    ///
    /// ```no_run
    /// use moso_auth::oauth::http::HttpResponse;
    ///
    /// # fn f(r: &HttpResponse) -> moso_auth::Result<serde_json::Value> {
    /// r.json::<serde_json::Value>("token endpoint")
    /// # }
    /// ```
    pub fn json<T: serde::de::DeserializeOwned>(&self, what: &'static str) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(|e| {
            let preview: String = self.text().chars().take(200).collect();
            Error::Unavailable {
                component: what,
                detail: format!("the response is not the JSON it promised ({e}): {preview}"),
                source: None,
            }
        })
    }
}

/// How this crate reaches an identity provider.
///
/// Implement it to route through a proxy, to add a header every request needs,
/// or to point a test at a server that is not really Google.
///
/// ```no_run
/// use moso_auth::oauth::http::{HttpRequest, HttpResponse, HttpTransport};
///
/// async fn fetch(t: &dyn HttpTransport) -> moso_auth::Result<HttpResponse> {
///     t.send(&HttpRequest::get("https://example.com")).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot make the outbound requests a social login needs",
    label = "not an HTTP transport",
    note = "an HTTP transport implements one method: `send(&HttpRequest) -> BoxFuture<Result<HttpResponse>>`",
    note = "help: use the bundled one — `Provider::google(config).transport(RustlsTransport::shared()?)`",
    note = "help: it is `dyn`-compatible on purpose, so a provider holds \
            `Arc<dyn HttpTransport>` and which one is configuration"
)]
pub trait HttpTransport: Send + Sync + 'static {
    /// Perform one request.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] for anything that is not a response: a refused
    /// connection, a TLS failure, a timeout, a body over the cap.
    fn send<'a>(&'a self, request: &'a HttpRequest) -> BoxFuture<'a, Result<HttpResponse>>;
}

/// The bundled transport: `hyper` over `rustls`, on the `ring` provider.
///
/// ```no_run
/// use moso_auth::oauth::http::RustlsTransport;
///
/// # fn f() -> moso_auth::Result<()> {
/// let _ = RustlsTransport::new()?;
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct RustlsTransport {
    /// The client configuration, shared between requests.
    tls: Arc<rustls::ClientConfig>,
    /// The deadline for one request.
    timeout: Duration,
    /// The response body cap.
    max_body: usize,
}

impl core::fmt::Debug for RustlsTransport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RustlsTransport")
            .field("timeout", &self.timeout)
            .field("max_body", &self.max_body)
            .finish_non_exhaustive()
    }
}

impl RustlsTransport {
    /// Build one, trusting the Mozilla root set.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when `rustls` refuses the protocol versions, which can
    /// only happen if the crate is built with every TLS version turned off.
    ///
    /// ```no_run
    /// # use moso_auth::oauth::http::RustlsTransport;
    /// # fn f() -> moso_auth::Result<RustlsTransport> { RustlsTransport::new() }
    /// ```
    pub fn new() -> Result<Self> {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| {
                Error::Config(std::borrow::Cow::Owned(format!(
                    "rustls refused every TLS version: {e}"
                )))
            })?
            .with_root_certificates(roots)
            .with_no_client_auth();

        Ok(Self {
            tls: Arc::new(tls),
            timeout: DEFAULT_TIMEOUT,
            max_body: MAX_BODY,
        })
    }

    /// Build one behind an `Arc`, which is what a [`Provider`](crate::Provider)
    /// holds.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    ///
    /// ```no_run
    /// # use moso_auth::oauth::http::{HttpTransport, RustlsTransport};
    /// # use std::sync::Arc;
    /// # fn f() -> moso_auth::Result<Arc<dyn HttpTransport>> { RustlsTransport::shared() }
    /// ```
    pub fn shared() -> Result<Arc<dyn HttpTransport>> {
        Ok(Arc::new(Self::new()?))
    }

    /// Change the per-request deadline.
    ///
    /// ```no_run
    /// # use moso_auth::oauth::http::RustlsTransport;
    /// # use std::time::Duration;
    /// # fn f(t: RustlsTransport) { let _ = t.timeout(Duration::from_secs(3)); }
    /// ```
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// One request, without the redirect loop.
    async fn send_once(&self, request: &HttpRequest) -> Result<Response> {
        let target = Target::parse(&request.url)?;

        let mut builder = http::Request::builder()
            .method(request.method)
            .uri(target.path_and_query.as_str())
            .header(http::header::HOST, target.authority.as_str())
            .header(http::header::USER_AGENT, "moso-auth")
            .header(http::header::CONNECTION, "close");
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(authorization) = request.authorization() {
            builder = builder.header(http::header::AUTHORIZATION, authorization);
        }

        let body = request.body.clone().unwrap_or_default();
        let outbound = builder
            .header(http::header::CONTENT_LENGTH, body.len())
            .body(Full::new(bytes::Bytes::from(body)))
            .map_err(|e| unavailable(&target.host, format!("the request is malformed: {e}")))?;

        let stream = tokio::net::TcpStream::connect((target.host.as_str(), target.port))
            .await
            .map_err(|e| unavailable(&target.host, format!("cannot connect: {e}")))?;
        stream
            .set_nodelay(true)
            .map_err(|e| unavailable(&target.host, format!("cannot configure the socket: {e}")))?;

        if target.tls {
            let connector = tokio_rustls::TlsConnector::from(Arc::clone(&self.tls));
            let name = rustls_pki_types::ServerName::try_from(target.host.clone())
                .map_err(|_| unavailable(&target.host, "is not a valid TLS server name"))?;
            let stream = connector
                .connect(name, stream)
                .await
                .map_err(|e| unavailable(&target.host, format!("the TLS handshake failed: {e}")))?;
            self.exchange(&target, stream, outbound).await
        } else {
            self.exchange(&target, stream, outbound).await
        }
    }

    /// Drive one HTTP/1.1 exchange over an established stream.
    async fn exchange<S>(
        &self,
        target: &Target,
        stream: S,
        request: http::Request<Full<bytes::Bytes>>,
    ) -> Result<Response>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .map_err(|e| {
                    unavailable(&target.host, format!("the HTTP handshake failed: {e}"))
                })?;

        // The connection future drives the socket; it ends when the response is
        // complete, and `Connection: close` means that is once, which is what
        // makes a pool unnecessary for three requests per login.
        let pump = tokio::spawn(async move {
            let _ = connection.await;
        });

        let response = sender
            .send_request(request)
            .await
            .map_err(|e| unavailable(&target.host, format!("the request failed: {e}")))?;

        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(http::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        let body = Limited::new(response.into_body(), self.max_body)
            .collect()
            .await
            .map_err(|e| {
                unavailable(
                    &target.host,
                    format!("the response body could not be read, or exceeded the cap: {e}"),
                )
            })?
            .to_bytes()
            .to_vec();

        pump.abort();

        Ok(Response {
            status,
            location,
            body,
        })
    }
}

/// One response, plus the redirect target the loop needs.
struct Response {
    /// The status code.
    status: u16,
    /// `Location`, when there was one.
    location: Option<String>,
    /// The body.
    body: Vec<u8>,
}

impl HttpTransport for RustlsTransport {
    fn send<'a>(&'a self, request: &'a HttpRequest) -> BoxFuture<'a, Result<HttpResponse>> {
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + self.timeout;
            let mut current = request.clone();

            for hop in 0..=MAX_REDIRECTS {
                let response = tokio::time::timeout_at(deadline, self.send_once(&current))
                    .await
                    .map_err(|_| Error::Unavailable {
                        component: "identity provider",
                        detail: format!(
                            "no response within {}s",
                            self.timeout.as_secs_f32().round()
                        ),
                        source: None,
                    })??;

                let redirecting = matches!(response.status, 301 | 302 | 303 | 307 | 308);
                if !redirecting || hop == MAX_REDIRECTS {
                    return Ok(HttpResponse {
                        status: response.status,
                        body: response.body,
                    });
                }

                // A redirected POST would re-send the client secret to whatever
                // host the redirect names. Providers do not do this; an attacker
                // who can answer for one would love to.
                if current.method != "GET" {
                    return Err(unavailable(
                        &current.url,
                        "redirected a POST, which would forward the client credentials elsewhere",
                    ));
                }

                let location = response.location.ok_or_else(|| {
                    unavailable(&current.url, "redirected without saying where to")
                })?;
                current.url = resolve(&current.url, &location)?;
            }

            unreachable!("the loop returns on its last iteration")
        })
    }
}

/// Where a request is going.
#[derive(Debug)]
struct Target {
    /// Whether to speak TLS.
    tls: bool,
    /// The host, for SNI, the `Host` header and the connection.
    host: String,
    /// The port.
    port: u16,
    /// `host:port`, for the `Host` header.
    authority: String,
    /// The request target.
    path_and_query: String,
}

impl Target {
    /// Parse and check a URL.
    fn parse(url: &str) -> Result<Self> {
        let parsed = moso_schema::Url::parse_with_schemes(url, &["https", "http"])
            .map_err(|e| unavailable(url, format!("is not an http(s) URL: {e}")))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| unavailable(url, "has no host"))?
            .to_owned();
        let tls = parsed.scheme() == "https";
        let loopback = host == "localhost" || host == "127.0.0.1" || host == "[::1]";

        if !tls && !loopback {
            return Err(unavailable(
                url,
                "is plaintext HTTP to a host that is not loopback; an authorization code, a \
                 client secret and an access token would all travel in the clear",
            ));
        }

        let port = parsed.as_url().port().unwrap_or(if tls { 443 } else { 80 });
        let authority = if parsed.as_url().port().is_some() {
            format!("{host}:{port}")
        } else {
            host.clone()
        };
        let path_and_query = match parsed.as_url().query() {
            Some(query) => format!("{}?{query}", parsed.as_url().path()),
            None => parsed.as_url().path().to_owned(),
        };

        Ok(Self {
            tls,
            host,
            port,
            authority,
            path_and_query,
        })
    }
}

/// Resolve a `Location` against the URL it came from.
fn resolve(base: &str, location: &str) -> Result<String> {
    let base = moso_schema::Url::parse(base)
        .map_err(|e| unavailable(base, format!("is not a URL: {e}")))?;
    base.as_url()
        .join(location)
        .map(|u| u.to_string())
        .map_err(|e| unavailable(location, format!("is not a resolvable redirect: {e}")))
}

/// An [`Error::Unavailable`] naming the provider, not this crate.
fn unavailable(what: &str, detail: impl Into<String>) -> Error {
    Error::Unavailable {
        component: "identity provider",
        detail: format!("{what} {}", detail.into()),
        source: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Credentials must not reach `Debug`, because a request logged at trace
    /// level is a client secret in a log aggregator.
    #[test]
    fn credentials_are_not_printed() {
        let request = HttpRequest::form("https://example.com/token", "grant_type=code")
            .basic("client-id", "the-client-secret");
        let printed = format!("{request:?}");
        assert!(!printed.contains("the-client-secret"), "{printed}");
        assert!(!printed.contains("Basic"), "{printed}");

        let request = HttpRequest::get("https://example.com/me").bearer("ya29.the-access-token");
        assert!(!format!("{request:?}").contains("ya29"));
    }

    /// Plaintext HTTP to a real host is refused: every value in an OAuth
    /// exchange is a bearer credential.
    #[test]
    fn plaintext_to_a_real_host_is_refused() {
        let error = Target::parse("http://accounts.google.com/token")
            .expect_err("plaintext to a public host must be refused");
        assert!(format!("{error}").contains("clear"), "{error}");
    }

    /// …and allowed to loopback, which is a local Keycloak or a test server.
    #[test]
    fn plaintext_to_loopback_is_allowed() {
        let target = Target::parse("http://127.0.0.1:8080/token").expect("loopback is allowed");
        assert!(!target.tls);
        assert_eq!(target.port, 8080);
        assert_eq!(target.authority, "127.0.0.1:8080");
        assert_eq!(target.path_and_query, "/token");
    }

    /// The default port is derived from the scheme, and the `Host` header omits
    /// it — a `Host` of `example.com:443` is legal and some providers dislike
    /// it.
    #[test]
    fn the_default_port_is_implied_and_not_sent() {
        let target = Target::parse("https://example.com/oauth/token?x=1").expect("parses");
        assert!(target.tls);
        assert_eq!(target.port, 443);
        assert_eq!(target.authority, "example.com");
        assert_eq!(target.path_and_query, "/oauth/token?x=1");
    }

    /// A URL with no path still has to produce a valid request target.
    #[test]
    fn an_empty_path_becomes_a_slash() {
        let target = Target::parse("https://example.com").expect("parses");
        assert_eq!(target.path_and_query, "/");
    }

    /// A relative `Location` resolves against the request it answered.
    #[test]
    fn a_relative_redirect_resolves() {
        assert_eq!(
            resolve("https://example.com/a/b", "../c").expect("resolves"),
            "https://example.com/c"
        );
        assert_eq!(
            resolve("https://example.com/a", "https://other.example/x").expect("resolves"),
            "https://other.example/x"
        );
    }

    /// A non-JSON response names what arrived, because "expected value at line
    /// 1 column 1" does not tell an operator that Cloudflare answered.
    #[test]
    fn a_non_json_response_says_what_arrived() {
        let response = HttpResponse {
            status: 502,
            body: b"<html>Bad gateway</html>".to_vec(),
        };
        let error = response
            .json::<serde_json::Value>("token endpoint")
            .expect_err("HTML is not JSON");
        let message = format!("{error}");
        assert!(message.contains("Bad gateway"), "{message}");
        assert!(message.contains("token endpoint"), "{message}");
    }

    /// The status classifier is the one every caller branches on.
    #[test]
    fn success_is_2xx_and_nothing_else() {
        for status in [200_u16, 201, 204, 299] {
            assert!(
                HttpResponse {
                    status,
                    body: Vec::new()
                }
                .is_success()
            );
        }
        for status in [199_u16, 300, 400, 401, 500] {
            assert!(
                !HttpResponse {
                    status,
                    body: Vec::new()
                }
                .is_success()
            );
        }
    }
}
