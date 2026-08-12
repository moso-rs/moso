---
title: Security
description: The headers, limits, redaction and disclosure defaults you get without asking, how to configure CSRF, cookies and secrets, and an honest account of what the framework does not defend.
order: 36
status: shipped
---

A framework's security posture is mostly its defaults, because the defaults are what ships. Moso's
position is that the safe setting is the one you get for free and the unsafe one costs an explicit
call that says what it is doing. `expose_internal_errors` is off in every profile including
development. CORS is off until configured and has no `permissive()` constructor. `trusted_proxies` is
empty, so an unconfigured deployment does not believe a header any client can send. A secret is a type
that will not print itself.

This page is the whole posture in one place: what you get, what you configure, and what Moso does not
defend against.

> [!IMPORTANT]
> Everything described on this page is built. Two deliberate design choices are worth knowing up
> front: `Slot::RateLimit` is a reserved position you fill with a route guard rather than a global
> layer (see [rate limiting](#rate-limiting)), and the auth battery's mounted routes document their
> tag and statuses but not their bodies (see [the mounted auth routes](#the-mounted-auth-routes)).

## What you get without asking

| Concern | Default |
| --- | --- |
| Security headers | HSTS, `nosniff`, `Referrer-Policy`, `frame-ancestors 'none'`, `X-Frame-Options: DENY` on every response |
| Internal error disclosure | Off in every profile, with a boot warning when forced on |
| Panic detail disclosure | Off outside `dev`; the message routinely contains the data that caused it |
| Request body size | Capped at 2 MiB before a byte is read |
| Request duration | Capped at 30 s, rendered as a 504 problem document |
| Header count and size, URI length, JSON depth, query depth | All capped; see the [limits table](#request-limits) |
| Credential headers in logs | Redacted structurally by name, never by pattern |
| HPACK/QPACK indexing of `Authorization` | Disabled, so no CRIME-style compression oracle |
| Proxy headers | Ignored entirely until you name the trusted peers |
| CORS | Off, and `Access-Control-Allow-Origin: *` with credentials is a boot error |
| Panics | Caught, logged, counted, and answered with a correlated 500 |
| SQL injection through the query builder | Structurally impossible: identifiers are typed, values are parameters |
| Secrets in config | `SecretString` refuses `Display`, `Serialize` and `tracing` fields, and zeroes on drop |
| Passwords in schemas | `Password` has no `Display`, and serialising one is a loud error |

With the auth battery mounted, you also get session id cycling on login and on privilege change, a
`__Host-` prefixed `HttpOnly` `Secure` `SameSite=Lax` cookie, argon2id hashing on a bounded blocking
pool, constant-time comparison, identical responses and timing for "no such account" and "wrong
password", refresh token family revocation on reuse, and `auth_hash` invalidation so a password change
logs every session out at its next request. Those live in
[authentication](./authentication.md) and [passwords and sessions](./passwords-and-sessions.md).

## Security headers

The `security_headers` slot sets these on every response, with `insert` rather than `append` so a
handler that set its own does not end up sending two conflicting values.

| Header | Default |
| --- | --- |
| `Strict-Transport-Security` | `max-age=63072000; includeSubDomains` |
| `X-Content-Type-Options` | `nosniff` |
| `Referrer-Policy` | `strict-origin-when-cross-origin` |
| `Content-Security-Policy` | `frame-ancestors 'none'` |
| `X-Frame-Options` | `DENY` |

Three deliberate omissions:

- **No `preload` on HSTS.** Submitting a domain to the preload list is close to irreversible and is
  not a framework's decision.
- **No `default-src 'self'` CSP.** A full policy breaks any page that loads anything, so shipping one
  by default trains people to turn the whole layer off. `frame-ancestors 'none'` is the clickjacking
  protection, applies to APIs and pages alike, and breaks nothing.
- **No `Permissions-Policy`.** A deny-all policy is correct for an API and silently breaks any page
  that asks for a camera or a location. The value most people want is one constant away.

HSTS is omitted in the `dev` profile, because a two-year pin on `localhost` makes every other local
project on that machine HTTPS-only.

```rust title="src/lib.rs"
use moso::middleware::security_headers::DENY_ALL_PERMISSIONS_POLICY;
use moso::middleware::ReferrerPolicy;

let app = App::new(config).with_middleware(|stack| {
    stack.security_headers(|headers| {
        headers.csp("default-src 'self'; frame-ancestors 'none'");
        headers.permissions_policy(DENY_ALL_PERMISSIONS_POLICY);
        headers.referrer_policy(ReferrerPolicy::NoReferrer);
        headers.frame_options("SAMEORIGIN");
    });
});
```

Calling `security_headers` marks the setting explicit, so the profile defaults will not overwrite it
at boot. Every value is built once at boot as a `HeaderValue`, so the layer allocates nothing per
request. A value that cannot be rendered as a valid header is dropped with a warning rather than
failing boot or panicking per request, because a bad CSP string is a typo and refusing to start over
one is a worse failure than sending one header fewer and saying so.

## Request limits

Enforced before allocation, not after. `Json<T>` reads with a hard cap rather than allocating four
gigabytes and then failing.

| Key | Default | What it bounds |
| --- | --- | --- |
| `http.body_max` | 2 MiB | Request body |
| `http.multipart_max` | 32 MiB | Total multipart payload |
| `http.multipart_file_max` | 16 MiB | One multipart file |
| `http.header_max_count` | 100 | Number of request headers |
| `http.header_max_bytes` | 16 KiB | Total header bytes |
| `http.uri_max` | 8 KiB | Request-target length |
| `http.query_depth_max` | 8 | Bracket nesting in a query string |
| `http.json_depth_max` | 64 | JSON nesting depth |
| `http.timeout` | 30 s | Per-request duration |

Every limit keeps its default in every profile, deliberately. A limit that is looser in development
is a limit whose failure mode is only ever seen in production.

The body cap has two halves that cannot disagree. The `body_limit` middleware refuses a request whose
`Content-Length` declares itself too large before a byte is read, and caps the stream for a chunked
body that has no length to lie in. The extractor, which knows which limit applies to this operation,
produces the precise 413. The layer records its cap in the request extensions and the extractor
enforces the tighter of the two, so the number in the error is always the number that stopped you.

Raising the stack's limit does not raise the extractors': they read `http.body_max` from the request
context, and the smaller of the two is what a handler observes.

Long-lived responses are exempted by route pattern rather than by turning the timeout off:

```rust title="src/lib.rs"
let app = App::new(config).with_middleware(|stack| {
    stack.timeout(std::time::Duration::from_secs(10));
    stack.timeout_exempt("/events/{id}");
});
```

The exemption is matched against the pattern, never the raw path, so a client cannot widen it with a
crafted URL. The composed stack is installed outside Axum's routing, where Axum's own `MatchedPath`
is not there yet, so `MiddlewareStack::compose_routed` resolves the pattern itself once at the very
outside and publishes it for the trace, timeout and metrics slots to read. A request spelling the
exemption into the URL (`/events/%7Bid%7D`) resolves against the route table and finds nothing, so
it is timed like anything else. Both behaviours are pinned by tests in
`crates/moso-core/src/middleware/mod.rs`.

> [!NOTE]
> The exemption is a whole-request escape from `Slot::Timeout`, which is coarse.
> `Router::timeout` on a router that holds only the long-lived routes sets a *different* budget
> rather than removing one, and is usually the better shape. See [middleware](./middleware.md).

## Error disclosure

`http.expose_internal_errors` is `false` in every profile, including `dev`. It is the one switch a
profile is not allowed to flip, because it is the difference between an error page and a disclosure.

Turning it on is legitimate for an internal service behind a trusted boundary or for a staging
environment during an incident. It is announced at boot rather than discovered in a bug report:

```text
WARN moso::config: http.expose_internal_errors is ON: 5xx responses will carry their detail,
     source chain and backtrace to every client profile=production
```

It is a warning and not a boot error because a framework that refuses outright is a framework people
patch out.

Panic details are separate and follow the profile: `dev` renders the panic message into the response
body because hunting for it in a terminal is a wasted afternoon, and every other profile does not,
because a panic message routinely contains an index, a key, or a slice of the data that caused it.
The backtrace is never captured at the catch site, since the stack has already unwound by then; the
useful one is the one the panic hook prints, and `RUST_BACKTRACE=1` works exactly as it always does.

Even with disclosure on, a `SecretString` still renders as `***`. The two mechanisms are independent,
and there is a canary test that plants a distinctive string in a config secret and a `Password`, drives
the application through its success and failure paths, and greps every byte the process produced,
including response bodies, headers, log lines, `Debug` renderings, the OpenAPI document and the boot
report.

## Secrets

A `String` holding a database password is indistinguishable from any other `String`, so it ends up in
a `Debug` line, a `tracing` field, a serialised error or a crash dump. `SecretString` and
`SecretBytes` make each of those a redaction.

```rust title="src/config.rs"
use moso::config::prelude::*;

/// Everything this application reads from its environment.
#[derive(moso::Config, Debug)]
pub struct AppConfig {
    /// Signing key; never logged.
    #[config(secret)]
    pub secret_key: SecretString,
}
```

Reading the value is deliberately verbose and greppable:

```rust
let key = config.secret_key.expose();
assert_eq!(format!("{:?}", config.secret_key), "SecretString(***)");
```

`#[config(secret)]` on a plain `String` is a compile error that suggests the right type, because the
attribute without the type is a comment. The derive proves it by asserting a sealed marker trait that
only the two secret types implement.

**What it promises:** no `Debug`, no `Display`, no `Serialize`, no `tracing` field ever renders the
value, and the buffer is zeroed on drop.

**What it does not:** it does not promise the value never reaches swap or a core dump. Memory locking
is an operating-system control and pretending a type provides it would be dishonest. Nor is a copy you
made with `expose()` zeroed; that copy is yours.

### Where secrets come from

`#[config(secret_from = "file")]` reads `${KEY}_FILE`, which is the Docker and Kubernetes mount
convention, with exactly one trailing newline trimmed. One, not all, because a secret may legitimately
end in a newline and trimming everything would silently corrupt it.

Anything further is a `SecretProvider`, so Moso needs no dependency on Vault or AWS:

```rust title="src/secrets.rs"
use moso::config::{SecretProvider, SecretRef, SecretString};
use moso::{BoxFuture, Result};

/// Answers `vault://…` references.
pub struct Vault;

impl SecretProvider for Vault {
    fn scheme(&self) -> &'static str {
        "vault"
    }

    fn resolve<'a>(&'a self, reference: &'a SecretRef) -> BoxFuture<'a, Result<SecretString>> {
        Box::pin(async move {
            // A real provider calls out here.
            Ok(SecretString::from(format!("secret-for-{}", reference.locator)))
        })
    }
}
```

Register it with `App::new(config).secret_provider(Arc::new(Vault))`. A file read that fails names the
*path* and never the contents, because a secret file with a stray byte must not print itself into a
boot log.

### Passwords in request bodies

`moso::schema::Password` is the type for a plaintext password in transit from a request to a hasher.
It has no `Display`, no `AsRef<str>` and no `Deref`. Reading it requires `expose()`. `Serialize` exists
because `Schema` requires it and **always fails**, so a `Password` that reaches a response body
produces a loud serialisation error rather than a quiet breach. `Debug` prints `Password(***)`.

Minimum length is 12 characters, not 8: NIST SP 800-63B drops composition rules in favour of length,
and 8 has been inadequate for a decade. Maximum is 256, because bcrypt-family hashers are linear in
their input and an unbounded password field is a denial-of-service vector. In the OpenAPI document it
emits `{ "type": "string", "format": "password", "writeOnly": true }`.

A validation failure on a `Password` never quotes the value it rejected. That rule generalises: the
request log line carries the error's title, detail and source chain and never its field errors,
because a validation message can name the value that failed.

## Cookies

The auth battery's session cookie defaults, from `CookieConfig`:

| Attribute | Default | Why |
| --- | --- | --- |
| Name | `id`, so the full name is `__Host-id` | The prefix is most of the length; a short base name is bytes off every request |
| `__Host-` prefix | On whenever it can apply | The browser then guarantees no subdomain set the cookie |
| `HttpOnly` | Always | The field exists so it can be asserted, not so it can be changed |
| `Secure` | On, auto-relaxed in development only | Forcing it off in production needs an explicit flag and logs a warning |
| `SameSite` | `Lax` | Stops most cross-site requests; CSRF closes the rest |
| `Path` | `/` | Required by the `__Host-` prefix |
| `Domain` | None, a host-only cookie | A domain cookie is readable by every subdomain, including the marketing site |

The `__Host-` prefix requires `Secure`, `Path=/` and no `Domain`. A browser silently ignores a
`__Host-` cookie that breaks any of the three, which presents as "login does not work" with nothing in
any log, so `SessionConfig::validate()` refuses that combination at boot, along with `SameSite=None`
without `Secure` and an idle timeout longer than the absolute one.

For your own cookies, `Cookies` gives plain access, `SignedCookies` authenticates with HMAC-SHA256 so
the client can read but not forge, and `PrivateCookies` encrypts with AES-256-GCM so the value is
opaque. All three need a `CookieKey` in the provider map:

```rust title="src/lib.rs"
use moso::extract::CookieKey;

let app = App::new(config.clone())
    .provide(CookieKey::derive(&config.secret_key)?);
```

`derive` uses HKDF-SHA256, so one secret yields independent signing and encryption keys. A secret
shorter than 32 bytes is rejected at boot rather than silently padded, because a weak signing key is a
security bug and silently accepting one is how it ships. `Debug` on a `CookieKey` prints
`CookieKey(***)`.

## Validating the auth configuration

`AuthConfig::validate()` reports **every** problem in one error rather than the first, each naming
its field and carrying a `help:` line with the edit that fixes it. A boot report that stops at the
earliest mistake turns one broken deployment into as many restart cycles as there are typos.

| Refused | Because |
| --- | --- |
| `auth.secret_keys` is empty | an unsigned session cookie is one anybody can mint |
| A key shorter than 32 bytes | HMAC-SHA256 signs the cookie; a short key does not make the signature shorter, only guessable |
| The three session rules | folded in from `SessionConfig::validate`, not restated: idle past absolute, `SameSite=None` without `Secure`, `__Host-` with a `Domain` or a sub-path |
| A redirect-allowlist entry that is not a bare origin | a wildcard, a relative value, or anything carrying userinfo, a path, a query or a fragment |
| A symmetric JWT algorithm without `allow_symmetric` | every holder of the verification key can also mint tokens, and the key can never appear in a JWKS document |
| `min_strength` above 4, or `min_length` outside `Password`'s own 12–256 | a policy nothing could ever satisfy, or one that can never take effect |
| `hash_params` below `HashParams::OWASP_MINIMUM` in any dimension | being slow hardware is not a reason to be weak |

Two deliberate omissions. `hash_params` being **unset** is not a problem, it is a `WARN` naming the
floor it will run on, because uncalibrated hashing is a defensible choice and a silent one is not.
And `allow_insecure_cookies` is not checked here at all: whether it is acceptable depends entirely on
the profile, and this method has no profile to read. The enforcement point is
`AuthConfig::cookie_for(profile)`, which drops `Secure` in `dev` only and in every other profile
keeps it on and logs a warning saying the setting reads as protection that is switched off.

> [!WARNING]
> `AuthConfig::from_env` reads the environment into an `AuthConfig` and validates it at boot, with the
> signing keys held as `SecretBytes` and a redacting `Debug`. What is not automatic is the call site:
> nothing invokes it on your behalf, so call `from_env` (or construct the struct and call
> `validate()`) in the composition root, or none of the above runs.

## CSRF

`SameSite=Lax` stops most cross-site requests, and "most" is not a security property. The `Csrf` guard
closes the rest with a double-submit token: it lives in the session and is echoed in a header or a
form field, and a cross-site attacker can do neither.

It applies only to requests that are **both** non-idempotent and cookie-authenticated. A `GET`, `HEAD`,
`OPTIONS` or `TRACE` changes nothing. A request carrying an `Authorization` header is not a CSRF
target, because a browser does not attach bearer tokens or API keys to a cross-site request, so
machine-to-machine calls pay nothing.

```rust title="src/routes/mod.rs"
use moso::auth::{Csrf, CsrfConfig};

let router = Router::new()
    .post("/orders", moso::ep!(place_order))
    .delete("/orders/{id}", moso::ep!(cancel_order))
    .guard(Csrf::new(CsrfConfig::default()));
```

A guard is middleware that documents itself, so applying it adds a `403` response and an optional
`x-csrf-token` header parameter to every operation on that router in the OpenAPI document. That is the
gap most frameworks leave open: a middleware that can return 403 makes the document wrong because
nothing tells the document about it.

Hand the token to the client from a bootstrap endpoint or a template:

```rust title="src/routes/session.rs"
use moso::auth::{AuthSession, Csrf, CsrfConfig};
use moso::prelude::*;

/// The token a browser client must echo on state-changing requests.
#[endpoint]
async fn csrf_token(Depends(AuthSession(session)): Depends<AuthSession>) -> Result<Json<String>> {
    let token = Csrf::new(CsrfConfig::default()).token(&session)?;
    Ok(Json(token))
}
```

`token` mints 32 bytes of entropy on first call and is stable afterwards for the life of the session.

| `CsrfConfig` field | Default | Meaning |
| --- | --- | --- |
| `header` | `x-csrf-token` | Where the token may arrive |
| `field` | `csrf_token` | Query-string field, for a form post without JavaScript |
| `session_key` | `_csrf` | Where the token is stored in the session |
| `check_origin` | `true` | Also require an `Origin` or `Referer` matching the `Host` |

`check_origin` is belt and braces. A request with neither header is refused, because every browser
sends one on a cross-origin state-changing request, so their joint absence is itself a signal rather
than a compatibility problem.

Two sharp edges. The form field is read from the **query string only**: reading it from the body would
mean buffering the body inside a guard, which runs before the handler has said how large a body it
will accept. And a session with no token yet is a 403 with a message telling you to call
`Csrf::token`, not a silent pass.

Comparison is constant time.

## Trusting a proxy

`http.trusted_proxies` is empty by default, so `X-Forwarded-For` is not consulted at all. An IP taken
from an untrusted header is a client-controlled string, and rate limiters and audit logs built on one
are worse than useless: they are confidently wrong.

```toml title=".env or config"
HTTP__TRUSTED_PROXIES=10.0.0.0/8,2001:db8::/32
```

With peers configured, `ClientIp` walks the header **right to left**, stopping at the first address
that is not itself a trusted proxy. Right to left is the only defensible direction: the rightmost entry
was appended by the proxy nearest you and is the most trustworthy, the leftmost is whatever the client
claimed and is worth nothing. Several popular middlewares walk left to right, which hands an attacker
their choice of IP.

An unparseable CIDR entry matches nothing. Refusing to match is the safe failure: a typo must not
accidentally trust the internet.

```rust title="src/routes/orders.rs"
use moso::extract::ClientIp;
use moso::prelude::*;

/// Record where a request came from.
#[endpoint]
async fn audit(ClientIp(ip): ClientIp) -> Result<NoContent> {
    tracing::info!(%ip, "request accepted");
    Ok(NoContent)
}
```

`ClientIp` needs connection info, which `App::serve` sets up. A hand-rolled `axum::serve` must use
`into_make_service_with_connect_info::<SocketAddr>()`, and the error message says so.

## CORS

Off until configured, and there is no `permissive()` constructor. `Access-Control-Allow-Origin: *`
combined with credentials is the single most common security misconfiguration in web APIs, and a
framework that ships a one-word way to get there is complicit.

```rust title="src/lib.rs"
use moso::middleware::CorsConfig;

let app = App::new(config).with_middleware(|stack| {
    stack.cors(
        CorsConfig::allow_origins(["https://app.example"])
            .allow_credentials(true),
    );
});
```

`CorsConfig::any_origin()` exists and is legitimate for a genuinely public, unauthenticated API.
Combining it with credentials is a **boot error**, because the browser would discard every such
response anyway and the failure would otherwise surface in a client developer's console rather than in
yours. An origin with a path, a trailing slash or anything beyond scheme, host and port is also a boot
error with the corrected spelling in the fix line.

The slot needs the `cors` cargo feature. Enabling it without the feature is a boot error rather than a
silent no-op. `x-request-id` is exposed by default, because a client that cannot read the correlation
id cannot report a useful bug.

## Rate limiting

`Slot::RateLimit` is a reserved position in the middleware stack with **no built-in implementation**.
Enabling it with nothing in it is a boot error naming the fix, rather than a layer that silently does
nothing.

The implementation lives in `moso-kv` as a GCRA limiter you apply as a route guard, which is usually
what you want anyway: a global limit is rarely the right shape. See
[rate limiting and locks](./rate-limiting.md).

Credential endpoints get a sharper one, `moso_auth::LoginThrottle`, which is per-address **and**
per-identity. The address tier is that same GCRA limiter. The identity tier is exponential backoff
(`per_identity_base` of 2 s doubling to a `per_identity_max` of ten minutes, after
`per_identity_free` consecutive failures) rather than a lockout, because a hard lockout is itself an
attack: anybody who knows your address can lock you out with five bad logins. Past `challenge_after`
failures the decision is `ThrottleDecision::Challenge`, and a challenge with no `CaptchaVerifier`
registered is a **refusal**, since treating "we cannot check" as "let them through" would make the
challenge tier a way to skip the throttle. That is why `challenge_after` defaults to
`ThrottleConfig::CHALLENGE_OFF`: a refusal nobody can clear is the same hard lockout the paragraph
above refuses to ship, so the tier stays off until you register a verifier
(`moso_auth::captcha::HttpCaptchaVerifier` is the one that ships) and turn it on in the same edit.
Every per-identity key is a SHA-256 digest rather than the identity, and every namespace declares
`on_failure = fail`, so an unreachable store is a 503 and never an allow.

It is not a `Guard` and cannot be: `Guard::check` sees only the request parts, and the identity tier
keys on a field in the body, so you call it from the handler. Nothing sends the "somebody is trying
to sign into your account" email either: `moso-auth` deliberately does not depend on `moso-mail`.
What it does is build the alert. `LoginThrottle::notice` claims the once-per-window marker and
returns a `SecurityNotice` carrying the failure count, the window and the recent attempts, and a
`NoticeSink` registered with `AuthState::notice_sink` is where you take delivery of it. A
`SecurityNotice` holds no token and has no `expose()`, so a sink wired up for alerts can never be
handed a live credential. See [passwords and sessions](./passwords-and-sessions.md).

## The threat model

**What Moso defends against.**

- Reflected data in error responses: problem documents disclose only what the error kind marks safe.
- Credentials in logs and in HPACK/QPACK compression tables.
- Secrets in `Debug`, `Display`, serialisation and `tracing` output.
- Resource exhaustion through oversized bodies, headers, URIs, deeply nested JSON and long-running
  requests.
- Clickjacking, MIME sniffing, referrer leakage and protocol downgrade, through the default headers.
- Cross-site request forgery on cookie-authenticated state changes, when you apply the guard.
- SQL injection through the query builder and through `moso::sql!`, structurally: identifiers are a
  typed `Ident` and values are bound parameters, so there is no path from a runtime string to SQL
  syntax. The escape hatch is `Db::postgres_pool`, and at that point it is sqlx's rules, not Moso's.
- Spoofed client IPs, by ignoring proxy headers until you say which peers to believe.
- A panic taking down a connection and every request multiplexed onto it.

**What it does not.**

- **TLS.** Moso does not terminate TLS in this release. `ServerConfig::tls` exists as a reserved shape
  and a configuration that sets it is a **boot error**, not a silent plaintext listener. Terminate in
  front of the process: an ingress controller, a load balancer, or a service mesh.
- **A web application firewall.** No signature matching, no bot detection, no IP reputation.
- **Denial of service at the network layer.** Connection floods, slowloris and amplification are the
  edge's job. The limits here bound one request, not one attacker.
- **Cross-site scripting in your own templates.** Moso serves JSON and problem documents; if you render
  HTML, escaping is yours. The framework's own HTML error page escapes what it interpolates.
- **Authorization correctness.** The [policy engine](./policies.md) enforces the rules you write. It
  cannot tell you that you wrote the wrong rule.
- **Supply chain integrity, fully.** Dependencies are gated by `cargo deny` and `cargo audit` on
  every pull request and again nightly against a freshly fetched advisory database; a release runs
  only from a `v*.*.*` tag push, and every published binary carries a SHA-256 checksum and a SLSA
  build provenance attestation signed with the workflow's OIDC identity. Signing the tag itself is
  a documented rule the pipeline does not verify: `gh release create --verify-tag` checks that the
  tag exists, not that it carries a signature.
- **Anything an audit would find.** Moso has not had an external security review.

## The mounted auth routes

`moso_auth::routes()` is a set of switches (`password()`, `sessions()`, `api_keys()`, `oauth()`,
`passkeys()`, `totp()`, `magic_link()`, `jwks()`), and `build()` turns the ones you switched on into
a `Router` of 32 routes. Nothing is on by default, which is what keeps both the OpenAPI document and
the attack surface honest. Three limits are worth knowing before you rely on them:

- They are registered through `Router::post`/`get` rather than `#[endpoint]`, because `moso-auth`
  sits below the facade and cannot use the macro. So each carries `UndocumentedEndpoint`: its
  request and response **bodies** are stamped `x-moso-undocumented`. What is documented is what a
  person wrote down and is true of every route in the group: the `auth` tag, the 429 the throttle
  produces, the 503 an unreachable store produces and the 401 an authenticated route produces. The
  DTOs do implement `Schema`; the registration path does not carry the schemas.
- They speak `DefaultUser`, because `build()` has no type parameter and `Accounts<S>` cannot be
  erased. An application with its own `User` type copies the handlers.
- Both halves of every store ship, and the in-memory ones are the default a prototype reaches for.
  `MemoryRefreshStore`, `MemoryApiKeyStore` and `MemoryPasskeyStore` are single process: a
  deployment on them survives a restart having forgotten every refresh family, every API key and
  every passkey, and a second instance behind a load balancer shares none of them, so a passkey
  quarantined as cloned on one instance still authenticates on the other. Anything past one process
  wants `store::TableRefreshStore`, `TableApiKeyStore` or `TablePasskeyStore`.

The battery is behind the facade's `auth` feature, which is **off by default** because it pulls
`orm`. `examples/crud` turns it on and authenticates with `moso-auth`'s `ApiKeyAuthenticator`, but it
does not mount `routes()`, so the 32 mounted routes are covered by `moso-auth`'s own tests rather than
by a worked application you can read.

## Failure modes

**Login works locally and not in production, with nothing in the log.** The `__Host-` prefix is being
silently dropped by the browser because one of `Secure`, `Path=/` or no `Domain` does not hold. Run
`SessionConfig::validate()` at boot to turn it into an error you can read.

**Every state-changing request returns 403 after adding the CSRF guard.** The client is not echoing the
token, or is sending it in the body rather than the header or the query string. The 403 detail names
the header it expected.

**`ClientIp` returns the load balancer's address.** `http.trusted_proxies` is empty, which is the
default and is correct until you name the peers. Add the CIDR ranges of the hops in front of you, not
`0.0.0.0/0`.

**A 5xx body containing a table name.** `expose_internal_errors` is on. Check the boot log: it says so
in a `WARN` line every time the process starts.

**A secret appearing in a log line.** It went through `expose()` somewhere and was then formatted as a
plain `String`. The type cannot follow a copy you made. Grep for `.expose()` and check that each
result is handed straight to the thing that needs it.

**Boot fails with "server.tls is set, but Moso does not terminate TLS".** Working as intended.
Terminate in front of the process and remove the key.

## See also

- [Middleware](./middleware.md) for the slot model, ordering rules and how to replace a layer.
- [Errors](./errors.md) for what a problem document does and does not disclose.
- [Configuration](./configuration.md) for every key named here.
- [Authentication](./authentication.md), [passwords and sessions](./passwords-and-sessions.md) and
  [JWT and API keys](./jwt-and-api-keys.md) for the credential side.
- [Permissions and roles](./permissions.md) and [policies](./policies.md) for authorization.
- [Observability](./observability.md) for what reaches your logs, and what is redacted before it does.
