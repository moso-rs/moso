---
title: JWT and API keys
description: Issue and verify JWTs with rotating keys and a published JWKS, rotate refresh tokens with reuse detection, and mint scoped API keys that are stored only as a hash.
order: 24
status: shipped
---

Sessions are for browsers. JWTs are for service-to-service calls and mobile clients, and API keys are
for the machine that runs your customer's nightly export. This page covers the two credential kinds
that travel in a header rather than a cookie: how you issue them, what verification actually checks,
how keys rotate without an outage, and how a key that leaks stops working.

Everything here lives in `moso-auth`. The types are real, tested and usable today. A bearer-token
and API-key extractor now reads a credential off a request and turns it into a `Principal`, and
`moso_auth::routes()` mounts `POST /auth/token` and `POST /auth/refresh` alongside the JWKS document
and the four API-key routes. `moso-auth` also gives you `Jwt`, `RefreshStore` and
`ApiKeyAuthenticator` as libraries, so a handler that needs its own shape calls them directly. See
[authentication](./authentication.md) for the shape of the whole battery and
[passwords and sessions](./passwords-and-sessions.md) for the cookie half.

> [!IMPORTANT]
> **The token endpoint now exists.** `moso_auth::routes()` mounts `POST /auth/token`, which issues
> an access token and a refresh token, and `POST /auth/refresh`, which rotates the pair with reuse
> detection: a replayed refresh token burns the whole family, and `ReuseDetected` and `Invalid` both
> answer an identical 401. Refresh tokens are stored SHA-256-hashed; access tokens are short-lived
> signed JWTs. The JWKS document and the four `/auth/api-keys` routes are mounted too. See
> [what is mounted](#what-is-mounted-and-what-is-not).
>
> **A bearer token or an API key becomes a `Principal` through the extractor**, so
> `PrincipalKind::Token` and `PrincipalKind::ApiKey` are produced for you now, not only by code you
> write. And `?` on a `moso_auth::Result` inside a handler returning `moso_core::Result` works:
> `From<moso_auth::Error> for moso_core::Error` is implemented and total. See
> [errors at the boundary](#errors-at-the-boundary) for what each variant becomes.

## Adding the crate

`moso-auth` is reached through the `moso` facade, behind an `auth` feature that is **off by default**.

```toml title="Cargo.toml"
[dependencies]
moso = { path = "/absolute/path/to/moso/crates/moso", features = ["auth"] }
```

`auth` implies `orm`, because `moso-auth` depends on `moso-orm` (a user lives in a table) and the
database driver is compiled either way, so hiding `moso::db` behind a second flag would cost you the
ORM without saving a crate. The battery is then `moso::auth`, and every path on this page can be
spelled `moso::auth::Jwt` instead of `moso_auth::Jwt`. Depending on `moso-auth` directly also works,
and is what the examples below name because it is shorter:

```toml title="Cargo.toml"
[dependencies]
moso = { path = "/absolute/path/to/moso/crates/moso" }
moso-auth = { path = "/absolute/path/to/moso/crates/moso-auth" }
```

Nothing is published to crates.io yet, so this is a path or git dependency for now. See
[installation](../start/installation.md).

> [!NOTE]
> `moso-auth` gates passkeys behind an off-by-default `passkeys` Cargo feature; argon2, ring, rustls
> and hyper are unconditional. Turning `passkeys` on pulls `webauthn-rs` and, through
> `webauthn-rs-core`, OpenSSL, so that build needs a C toolchain and libssl headers (ADR-0015).
> Nothing on this page needs the feature, so a JWT-and-API-key build leaves it off.

## What is mounted, and what is not

`moso_auth::routes()` is a set of switches; nothing is mounted until a flag asks for it. Two flags
matter for the credentials on this page.

| Flag | Route | What it does |
| --- | --- | --- |
| `.jwks()` | `GET /.well-known/jwks.json` | serves `Jwt::jwks()` from the signer on `AuthState`, at the root rather than under `/auth` |
| `.api_keys()` | `GET /auth/api-keys` | the caller's keys, prefixes and metadata only |
| | `POST /auth/api-keys` | mints one; 201 with `Location`, and the only response that ever carries the secret |
| | `DELETE /auth/api-keys` | revokes every key the caller owns |
| | `DELETE /auth/api-keys/{prefix}` | revokes the one the listing names |

```rust
use std::sync::Arc;

use moso_auth::{AuthState, Jwt, MemoryApiKeyStore, MemorySessionStore, routes};

fn auth_router() -> moso::Router {
    routes().api_keys().jwks().build()
}

fn auth_state(jwt: Jwt) -> AuthState {
    AuthState::new(MemorySessionStore::shared())
        .api_keys(Arc::new(MemoryApiKeyStore::new()))
        .jwt(jwt)
}
```

`AuthState` is taken by every handler as `Inject<AuthState>`, so it is one `provide` in the
composition root. A route whose dependency was never configured answers with an `Error::Config`
naming the builder call that fixes it (`require_jwt` says `AuthState::jwt`, `require_api_keys` says
`AuthState::api_keys`), which is a 500 rather than a boot failure, because these handlers are
registered without a provider requirement.

Four things about the mounted API-key routes are worth knowing before you rely on them:

- **They authenticate by session**, through `Depends<AuthSession>`: managing API keys is a
  first-party action for a signed-in user, so an API key is deliberately not accepted to manage
  other API keys.
- **The mounted set as a whole is fixed to `DefaultUser`.** `AuthRoutes::build` has no type
  parameter, so the account store `AuthState` holds is `Arc<dyn AccountStore<User = DefaultUser>>`.
  These four routes never reach it (they need only the session and the `ApiKeyStore`), but an
  application with its own `User` type that wants the rest of the mounted set copies the handlers
  rather than configuring them.
- **Their bodies are undocumented.** They are registered through `Router::post`/`get`/`delete` rather
  than `#[endpoint]`, so they carry `UndocumentedEndpoint` and their request and response bodies are
  stamped `x-moso-undocumented`. `CreateApiKeyRequest`, `CreatedApiKey` and `ApiKeySummary` all
  implement `Schema`, so the schemas exist; the registration path does not carry them. What *is*
  documented, because a person wrote it down: the `auth` tag, the 401 and the 503.
- **Revoking somebody else's prefix is a 404, not a 403.** A 403 would confirm that a guessed prefix
  is a real key, which is the only thing a prefix is good for guessing.

> [!IMPORTANT]
> The four API-key routes read a `Session` out of the request extensions (the JWKS document does
> not), and only `SessionLayer` puts one there. Nothing installs it for you: without
> `stack.replace_custom(Slot::Session, layer)` in your composition root, a request to
> `/auth/api-keys` resolves `AuthSession` to the 500 whose message says
> `install SessionLayer in Slot::Session`. See
> [passwords and sessions](./passwords-and-sessions.md#installing-the-layer).

## Issue and verify a token

`Jwt::issuer` takes the configuration, a key id and the private key. `Jwt::verifier` takes public keys
and cannot sign. Both are generic over the claims type and default to `Claims`.

```rust
use moso_auth::{Claims, Jwt, JwtConfig};
use moso_core::config::SecretBytes;

fn round_trip(pkcs8: SecretBytes) -> moso_auth::Result<()> {
    let jwt: Jwt = Jwt::issuer(JwtConfig::default(), "2026-07", pkcs8)?;
    let token = jwt.issue(&Claims::new("usr_123"), std::time::Duration::from_secs(900))?;
    assert_eq!(jwt.verify(&token)?.subject(), "usr_123");
    Ok(())
}
```

The `kid` is not optional. Without one, a JWKS cannot name the key and rotating it means rejecting
every token in flight, so `Jwt::issuer` refuses an empty string. The public half is derived from the
private key and registered under the same `kid`, which is why an issuer can always verify what it
issued.

`issue` overwrites `exp` from the `ttl` you pass, stamps `iat` when the claims do not carry one, and
fills `iss` and `aud` from the configuration when the claims omit them. A verifier configured with an
audience therefore accepts what a matching issuer produces without you writing the field twice.

## Claims

`Claims` is the built-in payload. It is `#[non_exhaustive]`, so build it with `Claims::new` and the
builder methods rather than a struct literal.

| Field | Type | Notes |
| --- | --- | --- |
| `sub` | `String` | The subject. `Claims::new` sets it, `subject()` reads it. |
| `iss` | `Option<String>` | Filled from `JwtConfig::issuer` at issue time when absent. |
| `aud` | `Option<String>` | Accepts a string *or* an array on the way in, per RFC 7519. |
| `exp` | `i64` | Always overwritten from the `ttl` argument. |
| `iat` | `i64` | Stamped when absent or zero. |
| `nbf` | `Option<i64>` | Checked against the leeway on verify. |
| `jti` | `Option<String>` | Yours to populate and yours to track. |
| `extra` | `serde_json::Map` | `#[serde(flatten)]`, so extra claims are top-level members. |

```rust
use moso_auth::Claims;

fn tenant_scoped() -> moso_auth::Result<Option<String>> {
    let claims = Claims::new("usr_123")
        .with_audience("api.example.com")
        .with_claim("tenant", serde_json::json!("acme"));

    claims.claim("tenant")
}
```

For a fixed payload shape, use your own type: `Jwt<C>` works with any `C: Serialize +
DeserializeOwned`. The only constraint is that `C` must serialise to a JSON object, because a JWT
payload is an object by definition; anything else is an `Error::Config` at issue time.

## Signing keys

| Algorithm | Symmetric | Private key encodings `Jwt::issuer` accepts |
| --- | --- | --- |
| `EdDSA` (default) | no | PKCS#8 v1 or v2 Ed25519, or the bare 32-byte seed |
| `ES256` | no | PKCS#8 P-256 |
| `RS256` | no | PKCS#8 or PKCS#1 `RSAPrivateKey`. `ring` refuses keys under 2048 bits |
| `HS256` | yes | the raw shared secret |

`JwtAlgorithm` is `#[non_exhaustive]` and defaults to `EdDSA`: small keys, small signatures, no
parameter to get wrong. Use `ES256` or `RS256` when a consumer speaks nothing else.

`HS256` is refused unless you set `JwtConfig::allow_symmetric = true`, and when you do it logs a
warning at construction. Every service that can verify a symmetric token can also mint one, so a
shared secret across a service boundary is how tokens leak. A symmetric configuration also has an
empty `jwks()`, which is one more reason not to use one.

## Configuration

`JwtConfig` is `#[non_exhaustive]` with public fields, so start from `default()` and assign.

```rust
use moso_auth::JwtConfig;

let mut config = JwtConfig::default();
config.issuer = Some("https://id.example.com".to_owned());
config.audience = Some("api.example.com".to_owned());
config.access_ttl = std::time::Duration::from_secs(300);
```

| Field | Default | What it does |
| --- | --- | --- |
| `algorithm` | `EdDSA` | Fixed at construction. Never selected from a token's header. |
| `issuer` | `None` | Written into `iss` when issuing; required to match exactly when verifying. |
| `audience` | `None` | Same for `aud`. A token whose `aud` does not name it is rejected. |
| `access_ttl` | 900 s | What `MemoryRefreshStore` mints access tokens with on rotation. |
| `refresh_ttl` | 30 days | What a rotation mints the next refresh token with. |
| `leeway` | 60 s | Clock tolerance applied to `exp`, `nbf` and `iat`. |
| `allow_symmetric` | `false` | The HS256 opt-in. |

## Rotating keys and publishing a JWKS

Rotation is two phase: publish the new key while still verifying the old one, then stop. `also_verifying`
registers an extra public key under a new `kid`.

```rust
use moso_auth::{Claims, Jwt};

fn during_rotation(jwt: Jwt<Claims>, previous_public_key: Vec<u8>) -> moso_auth::Result<Jwt<Claims>> {
    jwt.also_verifying("2026-06", &previous_public_key)
}
```

`Jwt::jwks()` returns the public keys as a JWKS document ready to serve at
`moso_auth::jwt::JWKS_PATH`, which is `/.well-known/jwks.json`.

```rust
use moso_auth::{Claims, Jwt};

fn jwks_body(jwt: &Jwt<Claims>) -> serde_json::Value {
    jwt.jwks()
}
```

Two `kid`s cannot name the same key: `Jwt::verifier` and `also_verifying` both refuse a duplicate with
an `Error::Config` that says so. Keys are parsed at construction, so a malformed key is a boot failure
rather than a 401 on the first request.

`moso_auth::jwks::Jwk` and `JwkSet` are the document types if you need to build or read one by hand.
`JwkSet::find(kid)` is the lookup, and `Jwk::is_signing_key()` treats a key as usable unless it
explicitly declares `"use": "enc"`.

## Consuming another service's tokens

`RemoteJwks` fetches a JWKS over HTTPS and verifies against it.

```rust
use moso_auth::{Claims, JwtConfig, RemoteJwks};

let mut config = JwtConfig::default();
config.issuer = Some("https://idp.example.com".to_owned());
config.audience = Some("api.example.com".to_owned());

let jwks = RemoteJwks::new("https://idp.example.com/jwks", config)
    .cache_for(std::time::Duration::from_secs(600))
    .refetch_at_most_every(std::time::Duration::from_secs(60));
```

Then `jwks.verify(token).await` for `Claims`, or `jwks.verify_as::<C>(token).await` for your own type.
`refresh()` forces a fetch and `cached_key_count()` tells you how many keys are in hand.

The defaults are a one hour cache and a five minute floor between fetches. That floor matters: without
it, a stream of tokens carrying made-up `kid`s is a denial-of-service amplifier pointed at somebody
else's identity provider. The throttle keys on the last *attempt* rather than the last success, so a
failing endpoint cannot be hammered either.

When a fetch fails and something is cached, `RemoteJwks` serves the stale key set rather than failing.
The keys were valid a moment ago and the endpoint being down is not evidence that they are not. It
also silently skips keys it cannot use for the configured algorithm, and returns `Error::Unavailable`
only when the document yields no usable key at all. The fetch is capped at 256 KiB.

## What verification rejects

`Jwt::verify` reads the header's `alg` exactly once, to compare it with the algorithm the verifier was
built with. It is never used to select a key, a curve or a routine. Three classic attacks are
unrepresentable rather than defended against: `"none"` is not a `JwtAlgorithm` variant so it can never
equal the configured one, an `RS256` verifier holds an RSA modulus rather than a byte string HMAC
could key, and an unknown `kid` is a rejection rather than a fetch.

| Condition | Result |
| --- | --- |
| Not exactly three dot-separated parts | `Error::InvalidCredentials` |
| Header `alg` differs from the configured algorithm | `Error::InvalidCredentials` |
| Header `crit` present and non-empty | `Error::InvalidCredentials` |
| `kid` present and unknown | `Error::InvalidCredentials` (no fallback to the other keys) |
| Signature does not verify | `Error::InvalidCredentials` |
| `exp` **absent** | `Error::InvalidCredentials` |
| `now - leeway >= exp` | `Error::Expired { kind: "token" }` |
| `nbf` in the future beyond the leeway | `Error::InvalidCredentials` |
| `iat` in the future beyond the leeway | `Error::InvalidCredentials` |
| `iss` configured and not matching | `Error::InvalidCredentials` |
| `aud` configured and not named | `Error::InvalidCredentials` |
| Payload does not deserialise into `C` | `Error::InvalidCredentials` |

`exp` being required is a deliberate departure from RFC 7519, which makes it optional. A token that
never expires and cannot be revoked is not a credential this crate accepts.

Nothing that costs an allocation of unbounded size or a database lookup happens before the signature
is verified: the split, the `alg` comparison, the key lookup and the signature check run first, and
the payload is only deserialised after.

## Expiry and refresh

An access token is short lived by design, so something has to mint the next one. `RefreshToken` is a
256-bit random value stored as a hex SHA-256, tagged with a *family*.

```rust
use moso_auth::RefreshToken;

fn mint() -> moso_auth::Result<()> {
    let token = RefreshToken::mint("usr_1", "fam_1", std::time::Duration::from_secs(60))?;
    assert_eq!(token.subject, "usr_1");
    assert_eq!(token.hash().len(), 64, "hex SHA-256");
    Ok(())
}
```

`RefreshStore` is the trait a deployment implements. `exchange` is where the interesting behaviour
lives: a token that has already been used means a copy exists, so the whole family is revoked.

This is the crate's own test for it, with `store` a `MemoryRefreshStore`:

```rust
let first = store
    .issue("usr_1", Duration::from_secs(3600))
    .await
    .unwrap();
let RefreshOutcome::Rotated {
    refresh: second, ..
} = store.exchange(first.expose()).await.unwrap()
else {
    panic!("expected a rotation");
};

// The attacker replays the token the legitimate client already used.
let outcome = store.exchange(first.expose()).await.unwrap();
let RefreshOutcome::ReuseDetected { family } = outcome else {
    panic!("expected reuse detection, got {outcome:?}");
};
assert_eq!(family, first.family());

// And the legitimate client's current token is dead too.
assert!(matches!(
    store.exchange(second.expose()).await.unwrap(),
    RefreshOutcome::Invalid
));
assert!(store.is_empty(), "the family's rows are gone");
```

| `RefreshOutcome` | Meaning | What a handler does |
| --- | --- | --- |
| `Rotated { access, refresh }` | Success. The old token is dead. | Return both. |
| `ReuseDetected { family }` | The token was presented twice. The family is gone. | 401, and treat it as a security event. |
| `Invalid` | Unknown, expired, or in a revoked family. | 401. |

Burning the family logs out the legitimate user, which is strictly better than an attacker holding a
token that rotates forever. The audit line is emitted on `target: "moso_auth::audit"` with
`event = "refresh_token_reuse"`, the family, the subject and the number of revoked rows. Wire that
into [observability](./observability.md) alerting.

`revoke_family(family)` and `revoke_subject(subject)` are the manual levers: the second is "log this
user out of every API client".

> [!WARNING]
> `MemoryRefreshStore` serialises `exchange` behind a `std::sync::Mutex`. That is correct for one
> process and wrong for two: a mutex in process A says nothing to process B, so a token issued by
> one instance is unknown to the other and reuse detection sees half the traffic. Past one process,
> use `moso_auth::store::TableRefreshStore`.

`TableRefreshStore` keeps the families in `moso_auth_refresh_tokens` and makes the reuse detection a
compare-and-set, so the database does the serialising:

```sql
update moso_auth_refresh_tokens
   set used = $1                    -- true
 where token_hash = $2
   and used       = $3              -- false
   and expires_at > $4              -- now
```

The **affected row count is the answer**. One means this caller claimed the token and nobody else
can; zero means somebody already did, or it never existed, or it had expired, and a second cheap
read decides which. There is no window between a read and a write for a second process to also
decide it won, because there is no read. The whole exchange runs in a transaction, so the loser's
family burn always sees the successor the winner minted rather than racing it: two concurrent
exchanges of one token produce exactly one rotation, exactly one `ReuseDetected`, and an empty
family afterwards.

```rust
use std::sync::Arc;

use moso_auth::store::TableRefreshStore;

let store = TableRefreshStore::new(db.clone(), Arc::new(jwt));
```

The table comes from your migrations, never from the store: see
[getting the tables](./authentication.md#getting-the-tables).

## Where a token may be carried

`ApiKeyAuthenticator::presented_in` is the one shipped header reader, and it covers both bearer JWTs
and API keys because the shape is the same:

1. `Authorization: <scheme> <token>`, with the scheme compared case-insensitively against `bearer`.
2. A configured extra header, if you called `.header("x-api-key")`.

It returns `None` when neither is present, which means "no credentials presented" rather than "wrong
credentials". That distinction is what lets a handler choose between a 401 challenge and serving the
request anonymously.

Nothing reads a token from a query string or a cookie. A credential in a URL ends up in access logs,
`Referer` headers and browser history.

Once you have authenticated a token, the way to make it visible to the rest of the request is a
`Principal` in the request extensions. The extractors prefer an extension-supplied `Principal` and only
fall back to the session when none is there.

```rust
use moso_auth::{Principal, PrincipalKind};

fn principal_for(subject: &str, key_prefix: &str, scopes: Vec<String>) -> Principal {
    // `Principal` is `#[non_exhaustive]`, so start from a constructor and assign.
    let mut principal = Principal::session(subject);
    principal.kind = PrincipalKind::ApiKey;
    principal.credential = Some(key_prefix.to_owned());
    principal.scopes = scopes;
    principal
}
```

`PrincipalKind` has five values: `Anonymous`, `Session`, `Token`, `ApiKey` and `Service`. Their wire
names come from `as_str()`. `RequireKind::new([PrincipalKind::Token, PrincipalKind::ApiKey])` is the
guard that restricts an endpoint to particular credential kinds, and it documents its own 403 in the
generated [OpenAPI](./openapi.md) document. The three OpenAPI scheme names are the constants
`extract::SESSION_SCHEME`, `extract::BEARER_SCHEME` and `extract::API_KEY_SCHEME`: `session`, `bearer`
and `api_key`.

> [!NOTE]
> The bearer-token and API-key extractor inserts a `Principal` into the request extensions, so
> `PrincipalKind::Token` and `ApiKey` are produced for you; `Service` still comes only from your own
> code. Cookie traffic gets `PrincipalKind::Session` from the session fallback. The mounted API-key
> routes themselves read `AuthSession` and do not construct a `Principal` of another kind.

## API keys

The format is `mso_live_<prefix>_<secret>` or `mso_test_<prefix>_<secret>`, and every part earns its
place.

| Part | Purpose |
| --- | --- |
| `mso` | Makes a leaked key findable. Secret scanners match on a registered prefix. |
| `live` / `test` | Visible in a log, and a production deployment can refuse a test key. |
| `<prefix>` | 8 lowercase hex characters, stored in the clear and indexed. One query, not a scan. |
| `<secret>` | Never stored. Only its hex SHA-256 is, and the full key is shown exactly once. |

The constants are `apikey::KEY_PREFIX` (`"mso"`), `apikey::PREFIX_LENGTH` (8) and
`apikey::TOUCH_INTERVAL` (60 seconds).

### Issuing

```rust
use moso_auth::{ApiKey, KeyEnvironment};

fn issue_and_check() -> moso_auth::Result<()> {
    let new = ApiKey::generate("deploy key", "usr_1", KeyEnvironment::Live)?;
    let presented = new.secret.expose();
    assert!(presented.starts_with("mso_live_"));

    // What the server does with what the client sent.
    let (environment, prefix, secret) = ApiKey::parse(presented)?;
    assert_eq!(environment, KeyEnvironment::Live);
    assert_eq!(prefix, new.record.prefix);
    assert!(new.record.verify_secret(&secret));
    Ok(())
}
```

`NewApiKey` holds two things: `record`, which you persist through `ApiKeyStore::insert`, and `secret`,
a `SecretString` you show the user once. `secret` does not appear in a `Debug`, a log or a panic
message, and there is no way to recover it afterwards.

```rust
use moso_auth::{ApiKey, KeyEnvironment, NewApiKey};

fn ci_key() -> moso_auth::Result<NewApiKey> {
    let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)?
        .with_scopes(["posts:read", "posts:write"])
        .expiring_in(std::time::Duration::from_secs(90 * 24 * 3600));
    assert_eq!(new.record.scopes.len(), 2);
    assert!(new.record.expires_at.is_some());
    Ok(new)
}
```

`scopes` holds permission *wire names* as plain strings, not a typed permission set. That is
deliberate: `moso-auth` is not allowed to depend on `moso-authz`, so the authorization side parses
them back into bits and intersects a key's set with its owner's. A key can never grant more than the
account it belongs to holds. See [permissions and roles](./permissions.md).

### Authenticating

```rust
use std::sync::Arc;
use moso_auth::{ApiKeyAuthenticator, ApiKeyStore, KeyEnvironment};

fn authenticator(store: Arc<dyn ApiKeyStore>) -> ApiKeyAuthenticator {
    ApiKeyAuthenticator::new(store)
        .header("x-api-key")
        .accept([KeyEnvironment::Live])
        .touch_at_most_every(std::time::Duration::from_secs(60))
}
```

`authenticate(presented)` parses the key, checks the environment is accepted, looks the record up by
its indexed prefix, compares the secret in constant time, and only then checks the revocation and
expiry state. That order matters: "this key was revoked" is only ever said to somebody who actually
holds the key, otherwise the prefix alone becomes an oracle for which keys once existed.

| Failure | Error |
| --- | --- |
| Wrong shape, unknown prefix, wrong secret, refused environment | `Error::InvalidCredentials` |
| Presented key longer than 256 characters | `Error::InvalidCredentials` |
| The key is real and has `revoked_at` set | `Error::Revoked { kind: "api key" }` |
| The key is real and past `expires_at` | `Error::Expired { kind: "api key" }` |

`ApiKeyAuthenticator::new` accepts `Live` only. Calling `.accept([KeyEnvironment::Test])` in a
development profile is how a test key works there and nowhere else.

### Scoping, revoking and last-used

`ApiKey::has_scope(name)` is an exact string match against the key's scopes. `ApiKey::is_usable(now)`
is `revoked_at.is_none() && expires_at.is_none_or(|e| now < e)`.

Revocation is a tombstone: `ApiKeyStore::revoke(id)` sets `revoked_at` rather than deleting the row,
so an audit trail survives and a presented key can be told apart from a key that never existed.

Last-used tracking is deliberately lossy. `authenticate` spawns the write off the request path and at
most once per key per `touch_at_most_every` interval (60 seconds by default). A full or unreachable
store loses the timestamp rather than the request, and a call made outside a tokio runtime skips it
entirely rather than panicking. Pass `Duration::ZERO` to write on every request, which is what the
default exists to avoid.

Two stores ship. `MemoryApiKeyStore` is complete and single-process; `moso_auth::store::TableApiKeyStore`
keeps the rows in `moso_auth_api_keys` and is what a deployment with more than one instance needs.
One conformance suite runs against both, on SQLite and on PostgreSQL, so they cannot drift.

```rust
use moso_auth::store::TableApiKeyStore;

let keys = TableApiKeyStore::new(db.clone());
```

The table's lookup is `where prefix = $1` on a unique index, and nothing else. The secret's hash
carries no index and appears in no predicate anywhere in that file, deliberately: a `where hash = $1`
makes the database's own `memcmp`, which returns at the first differing byte, the timing oracle that
`ApiKey::verify_secret`'s constant-time comparison exists to remove. Revoking is a compare-and-set on
`revoked_at is null`, so two operators revoking at once produce one `true` and one `false`, and the
row is kept rather than deleted so an audit can still resolve the key id.

If you write your own store, the rule is the same one: `find_by_prefix` must be one indexed lookup on
the public prefix, and the secret is checked afterwards in constant time.

## Errors at the boundary

`From<moso_auth::Error> for moso_core::Error` is implemented and total, so `?` on a
`moso_auth::Result` inside a handler returning `moso_core::Result` is the ordinary way to write these
handlers. What it produces is not a mechanical status lookup, so it is worth knowing.

Everything except `Unavailable` goes through `Error::client_facing()` first, which collapses
`InvalidCredentials`, `Expired`, `Revoked` and `Ceremony` into one `InvalidCredentials`. That is the
point: an expired session, a revoked API key, a forged OAuth `state` and a wrong password must be
indistinguishable to a probe. The reason survives in the log through `Display`, and never in the
response.

| `moso_auth::Error` | Becomes | Carrying |
| --- | --- | --- |
| `InvalidCredentials`, `Unauthenticated`, `Expired`, `Revoked`, `Ceremony` | 401 | `WWW-Authenticate: Bearer` |
| `SecondFactorRequired { challenge }` | 401 | the same header, plus a `challenge` member in the problem document |
| `RateLimited { retry_after }` | 429 | `Retry-After` in whole seconds **rounded up**, and a `retry_after` member |
| `PasswordPolicy { code, detail }` | 422 | one field error at `/password`, with `detail` as the message |
| `Unavailable { .. }` | 503 | retryable, the source chain kept for the operator, `detail` suppressed at render |
| `Config(detail)` | 500 | `detail` for the log and the dev error page, never for the client |

`Unavailable` is the one variant the conversion takes by value rather than through the collapse,
because `client_facing` cannot clone a `BoxError` and the source is the only record of *why* a store
was unreachable. Nothing is disclosed by keeping it: a 5xx renders neither its detail nor its chain
unless an operator has deliberately set `http.expose_internal_errors`.

The `code` on the 422 is `moso-schema`'s documented one where there is a match and namespaced where
there is not. Of the five codes `PasswordPolicy` is constructed with, only `len` is in `codes::ALL`;
`banned`, `breached`, `weak` and `reused` are reported as `custom:banned`, `custom:breached`,
`custom:weak` and `custom:reused`. A bare `"breached"` would collide with a code a future
`moso-schema` minor release might add, which is exactly what `custom:` exists to prevent.

Three predicates on `Error` are what a hand-written flow branches on, and all three work:

| Call | Answers |
| --- | --- |
| `client_facing()` | the collapsed error, for anything that reaches a client |
| `counts_as_attempt()` | whether this failure should charge a rate limit: true for `InvalidCredentials`, `Unauthenticated`, `Expired`, `Revoked` and `Ceremony`; false for `SecondFactorRequired`, `RateLimited`, `PasswordPolicy`, `Unavailable` and `Config` |
| `retryable()` | true for `Unavailable` and `RateLimited`, false for everything else |

`counts_as_attempt` agreeing across the four collapsed variants is not tidiness. If an expired
session were free and a wrong password were not, an attacker could tell them apart by watching when
the backoff starts, which is the oracle the collapse just closed.

## Failure modes

**A `Jwt` built with `verifier` cannot issue.** `issue` returns `Error::Config` naming the fix. A
`verifier` with an empty key list is refused at construction, because a verifier with no keys accepts
nothing and finding that out at boot beats finding it out in production.

**A symmetric configuration publishes an empty JWKS.** There is nothing to publish, so every consumer
needs the secret, and every consumer can then forge.

**`RefreshToken` serialises its token exposed.** Unlike a configuration `SecretString`, the value's
whole purpose is to reach the client. Your store must persist `hash()` and never the token.

**A refresh family burn logs out a real user.** That is the intended behaviour on reuse. If you see
`event = "refresh_token_reuse"` for a subject repeatedly, the cause is usually a client that retries a
failed exchange with the same token rather than an attacker.

**`RemoteJwks` failing open is a choice.** It serves stale keys through an outage. If your threat model
needs the opposite, call `refresh()` on a schedule and treat its error as fatal yourself.

**`AuthConfig` loads from the environment.** `AuthConfig::from_env` reads the configuration and runs
`AuthConfig::validate` before returning, so `secret_keys`, `jwt` and the rest can come from the
environment (signing keys are `SecretBytes`, with a redacting `Debug`) rather than being assigned in
Rust. `validate()` reports every problem in one error rather than the first (signing keys, the
session rules, the redirect allowlist, a symmetric JWT without the opt-in, the password policy and
the argon2 floor), and warns when `hash_params` is `None`. `cookie_for(profile)` and
`effective_hash_params()` work too. See [configuration](./configuration.md) for how the rest of the
framework does it.

**Rate limiting your own token handlers is on you.** `LoginThrottle` works (`check`, `record`,
`should_notify` and `recent` are all implemented over `moso_kv`, with the address tier delegated to
`moso-kv`'s own GCRA limiter and a per-identity exponential backoff on top, fail-closed). A token
handler you write yourself calls it, the way the login flow does. It also cannot be a `Guard`: the
per-identity tier keys on a field of the request *body*, and a `Guard` only ever sees the parts, so
the check runs inside the handler. See [rate limiting and locks](./rate-limiting.md).

## See also

- [Authentication](./authentication.md) for the whole battery and how the pieces fit.
- [Passwords and sessions](./passwords-and-sessions.md) for the cookie half and `auth_hash`.
- [OAuth and passkeys](./oauth-and-passkeys.md), which consumes identity tokens with the same checks.
- [Permissions and roles](./permissions.md) for what an API key's scopes turn into.
- [Security](./security.md) for the defaults and the disclosure posture.
