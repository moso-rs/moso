---
title: Authentication
description: 'Wire the auth battery: a principal type, a backend, the request extractors, the account lifecycle flows, the 32 mountable routes, and the credential kinds the next pages cover.'
order: 22
status: shipped
---

Authentication answers one question: who is making this request. Moso answers it in `moso-auth`, a
battery behind an off-by-default facade feature, built around your own user type rather than a
`User` table the framework owns. Authorization, the other question, is `moso-authz` and is covered
in [permissions and roles](./permissions.md). Keeping them apart is deliberate: a framework that
ships a login form and calls it security has answered half of one question.

This page is the wiring. It covers the principal type, the backends that check credentials, the
extractors a handler takes, the account lifecycle flows (registration, verification, reset, email
change), the mountable route builder, and which of the following pages covers each kind of
credential.

> [!IMPORTANT]
> One detail shapes everything below, so read it before you wire anything. The mounted routes are
> registered without `#[endpoint]`, so their request and response **bodies** are
> `x-moso-undocumented`: the tag and the statuses are documented, the schemas are not, and
> `moso new --auth` copies the documented handlers into your project when you want the bodies in
> your own document too. Other details (how `AuthConfig::from_env` loads and validates configuration
> at boot, how the mounted `POST /auth/refresh` route rotates a refresh token with reuse detection)
> are called out where you would meet them. Nothing installs `SessionLayer` *for* you, but installing
> it is one line: see [wiring it into an application](#wiring-it-into-an-application).

## Add the crate

`moso-auth` is reached through the facade's `auth` feature, which is off by default and implies
`orm`, because a user lives in a table. Turning it on re-exports the crate as `moso::auth`.

```toml title="Cargo.toml"
[dependencies]
moso = { path = "/absolute/path/to/moso/crates/moso", features = ["auth"] }
moso-kv = { path = "/absolute/path/to/moso/crates/moso-kv" }
```

Path dependencies because nothing is published yet; see [installation](../start/installation.md).

`moso-kv` is a second entry here, though the facade also re-exports it as `moso::kv` behind an
off-by-default `kv` feature; every store this battery puts state in (sessions, lifecycle tokens, the
login throttle, magic links, TOTP enrolments) takes a `moso_kv::Kv`. See
[cache and key value store](./cache.md).

`moso-auth` gates passkeys behind an off-by-default `passkeys` Cargo feature; argon2, ring, rustls
and hyper are unconditional. Two costs are worth knowing before the first build rather than after
it. It depends on `moso-orm` and `moso-kv`, so it pulls a database driver whether or not you asked
for one. And turning `passkeys` on pulls `webauthn-rs` and, through `webauthn-rs-core`, OpenSSL, so
that build needs a C toolchain and libssl headers (ADR-0015); leave the feature off and neither is
compiled.

## Your type is the principal

Nothing in the crate defines a user. You implement `AuthUser` on the type you already have.

```rust title="src/models/user.rs"
use moso::auth::AuthUser;

/// The application's account record.
#[derive(Clone)]
pub struct User {
    /// Its key.
    pub id: u64,
    /// The hash of the current password.
    pub password_hash: String,
    /// Bumped by "log out everywhere".
    pub session_epoch: i32,
    /// Whether the account may sign in at all.
    pub is_active: bool,
}

impl AuthUser for User {
    type Id = u64;

    fn auth_id(&self) -> u64 {
        self.id
    }

    fn auth_hash(&self) -> Vec<u8> {
        let mut bytes = self.password_hash.as_bytes().to_vec();
        bytes.extend_from_slice(&self.session_epoch.to_le_bytes());
        bytes
    }

    fn is_active(&self) -> bool {
        self.is_active
    }
}
```

`auth_hash` is the load-bearing method. Whatever it returns is copied onto the session record at
login and compared, in constant time, on every session load. Mix in the password hash and a per-user
epoch counter and you get "log out everywhere" for free: bump the epoch and every live session is
invalid at its next request, including sessions in a store this process cannot reach, with no scan
and no revocation list. `is_active` is checked on the same path, so deactivating an account ends its
sessions too.

`DefaultUser` exists so the extractors have a working default type parameter, so tests do not need
an entity, and (as the next sections explain) because the mounted routes are fixed to it. It has a
`String` id, an epoch and an active flag: `DefaultUser::new("usr_1", b"epoch-0".to_vec())`.

> [!WARNING]
> `AuthUser::Id` must round-trip through text, because `SessionRecord::user_id` is a `String`. A
> deploy that changes the key's type invalidates every live session, deliberately, rather than
> authenticating the wrong account.

## Backends check credentials

A backend turns credentials into a principal. `Ok(None)` means "no match" and is never an error, so
a wrong password and a missing account are the same value on the same path.

```rust
pub trait AuthBackend: Send + Sync + 'static {
    type User: AuthUser;
    type Credentials: Send + 'static;

    fn authenticate<'a>(&'a self, credentials: Self::Credentials, ctx: &'a AuthCtx)
        -> BoxFuture<'a, Result<Option<Self::User>>>;

    fn load<'a>(&'a self, id: &'a <Self::User as AuthUser>::Id)
        -> BoxFuture<'a, Result<Option<Self::User>>>;
}
```

`AuthCtx` is what the backend knows about the request doing the authenticating: `with_ip`,
`with_user_agent`, `with_identity` (normalised on the way in) and `with_request_id`. Write your own
`AuthBackend` for LDAP, SAML or an existing identity service; every backend automatically becomes a
`UserStore<Self::User>` through a blanket impl, which is the half the extractors need.

`DatabaseBackend<U>` is the built-in one: identity plus password against any ORM entity.

```rust title="src/auth.rs"
use moso::auth::DatabaseBackend;

fn backend(db: moso::db::Db) -> DatabaseBackend<Account> {
    DatabaseBackend::<Account>::new(db)
        .identity_column(Account::EMAIL)
        .password_column(Account::PASSWORD_HASH)
        .active_column(Account::IS_ACTIVE)
}
```

| Builder method | What it does | Default |
| --- | --- | --- |
| `identity_column(Column<U, String>)` | the column a login presents (an address, usually) | required |
| `password_column(Column<U, String>)` | the PHC hash column | required |
| `active_column(Column<U, bool>)` | an optional "may sign in" column | none |
| `rehash_on_login(bool)` | rewrite the stored hash when the deployment's argon2 parameters are raised | `true` |
| `validate()` | a boot error naming the entity, the column and the builder call to add | call it yourself |

Three behaviours are not obvious and matter:

- The miss path runs a dummy argon2 verify, so "no such account" and "wrong password" take the same
  time. There is a test asserting the p95 gap stays under 10 ms.
- Identity matching is two exact comparisons (`column = normalised OR column = trimmed`), never
  `ilike`. An address containing `_` or `%` is a `LIKE` wildcard, and `_` matching any character
  would let one account's password sign in as another's. If you want full case insensitivity, add a
  unique index on `lower(email)`, which is the answer at the database level and does not turn every
  login into a sequential scan.
- `DatabaseBackend` requires `<U as AuthUser>::Id: Into<<U as Entity>::Pk>`. The blanket
  `T: Into<T>` covers the common case; a mismatch is a compile error at the backend rather than a
  surprise at the first login.

## Wiring it into an application

Nothing is automatic. `App::with_auth` does not exist. Every validation is a method you call at
boot, which is the point: a boot error is cheaper than a login error.

`AuthConfig::from_env` is the usual entry point: it loads the configuration from the environment and
runs `AuthConfig::validate` before returning, so a missing signing key is a boot error. Reach for
`validate` directly when you build the config in code. It reports **every** problem in a single error:
missing or short signing keys, the session rules folded in from `SessionConfig::validate`, a
redirect-allowlist entry that is not a bare origin, a symmetric JWT algorithm without
`allow_symmetric`, a password policy or hash floor nothing could satisfy, each naming its field and
the edit that fixes it. An unset `hash_params` is not an error; it is a `WARN` on the boot log
saying hashing is running on `HashParams::OWASP_MINIMUM`. `?` on a `moso_auth::Result` works inside
a function returning `moso::Result`: the `From<moso_auth::Error> for moso::Error` conversion is
real, and it runs `Error::client_facing` first, so no boot path or handler can answer with an
uncollapsed authentication failure.

```rust title="src/lib.rs"
use std::sync::Arc;

use moso::AppBuilder;
use moso::auth::lifecycle::KvLifecycleTokens;
use moso::auth::routes::TokenSink;
use moso::auth::{
    AccountStore, AuthConfig, AuthHealthCheck, AuthState, DefaultUser, KvSessionStore,
    LoginThrottle, SessionLayer, SessionStore,
};
use moso::middleware::Slot;
use moso::prelude::*;
use moso_kv::Kv;

pub fn app(
    config: AppConfig,
    auth: AuthConfig,
    kv: Kv,
    accounts: Arc<dyn AccountStore<User = DefaultUser>>,
) -> Result<AppBuilder> {
    // 1. Every configuration problem, in one error, before anything serves.
    auth.validate()?;

    // 2. The session store and the signed cookie over it.
    let store: Arc<dyn SessionStore> = Arc::new(KvSessionStore::new(kv.clone()));
    let layer = SessionLayer::new(Arc::clone(&store), auth.session.clone())
        .keys(auth.secret_keys.clone());
    layer.validate()?;

    // 3. Where a minted token goes. Register none and the routes log a warning saying the token
    //    was minted and dropped; `moso-auth` does not depend on the mailer.
    let sink: TokenSink = Arc::new(|delivery| {
        Box::pin(async move {
            // `delivery.expose()` is the token, `delivery.destination()` the address, and
            // `delivery.purpose()` says which template. Enqueue the send here.
            let _ = delivery.destination().to_owned();
        })
    });

    // 4. One provider for every mounted handler.
    let state = AuthState::new(Arc::clone(&store))
        .session_config(auth.session.clone())
        .password_policy(auth.password.clone())
        .throttle(LoginThrottle::new(kv.clone(), auth.throttle.clone()))
        .accounts(accounts, KvLifecycleTokens::shared(kv.clone()))
        .kv(kv)
        .issuer(&config.name)
        .token_sink(sink);

    Ok(App::new(config)
        .provide(state)
        .health_check("auth", AuthHealthCheck::new(store))
        // 5. `Slot::Session` is a position with no built-in, and `SessionLayer` is a
        //    `CustomLayer` (`Route -> Route`, which is exactly what the stack folds), so
        //    `replace_custom` fills it. The entry keeps the slot's name, so `moso middleware`
        //    prints `session`.
        .with_middleware(|stack| {
            stack.replace_custom(Slot::Session, layer);
        })
        .mount(moso::auth::routes().password().sessions().totp().build()))
}
```

Three things in that function are worth reading twice.

`.provide(state)` is what `Inject<AuthState>` resolves through, and **boot does not check it**. A
handler registered without `#[endpoint]` reports no provider requirements, so a forgotten
`.provide(state)` is a 500 on the first request rather than a boot error.

`AuthHealthCheck` probes the session store through `SessionStore::probe()` under a one-second
`PROBE_TIMEOUT`, half of `READINESS_BUDGET`, so a hung store is reported as the slow component while
the rest of `/readyz` still answers. It is critical by default; `.critical(false)` if you would
rather stay in rotation.

`AuthConfig::cookie_for(profile)` is the other half of the configuration surface, and it is not
called above because `SessionConfig` already carries the cookie. Reach for it when
`allow_insecure_cookies` is set: it drops `Secure` in `Profile::Dev` only (and with it the
`__Host-` prefix, which is only honoured on a secure cookie), and in `Test` or `Production` it keeps
`Secure` on and logs a warning naming what the setting would have cost.

> [!NOTE]
> `replace_custom` is the `CustomLayer` sibling of `MiddlewareStack::replace`, which takes a
> `tower::Layer<Route>` and would not accept `SessionLayer`. The two cannot be one method: widening
> `replace` to take either needs two overlapping blanket impls, which the compiler rejects (E0119).
> `insert_before_custom`, `insert_after_custom` and `append_custom` are the same pairing for the
> other three installers, and `stack.validate()` stops reporting `Slot::Session` as empty the moment
> one of them fills it. `Session::detached` is the other way in, for a test that wants a session
> without a request.

If you would rather resolve the user through the extractors than through the mounted routes, also
register the backend:

```rust
use moso::auth::UserStore;

Ok(App::new(config)
    .provide_dyn::<dyn UserStore<User>>(Arc::new(backend))
    .mount(crate::routes::mount()))
```

Forget that and `CurrentUser<User>` returns a 500 whose message names that exact call, not a 401.

## The extractors

Four `Dependency` impls, taken with `Depends<T>` in a handler. Each is memoised once per request and
each writes its own security scheme and status code into the OpenAPI document, so an endpoint
documents its authentication by taking a parameter.

| Extractor | Resolves to | When it cannot | Contributes to the document |
| --- | --- | --- | --- |
| `CurrentUser<U>` | the principal, `Deref` to `U` | 401 | `session`, `bearer`, `api_key`, plus a 401 |
| `MaybeUser<U>` | `Option<U>`, never fails on absence | anonymous | the same schemes plus the empty requirement |
| `AuthSession` | the `Session`, `Deref` to it | 503 if the store is down | nothing |
| `Principal` | a cheap non-generic audit record | anonymous | the schemes plus the empty requirement |

`MaybeUser` and `Principal` contribute the empty requirement as well so a generated client does not
demand a token for an endpoint that works without one.

```rust title="src/routes/me.rs"
use moso::prelude::*;
use moso::auth::CurrentUser;

/// The signed-in account.
#[endpoint]
async fn me(Depends(CurrentUser(user)): Depends<CurrentUser<User>>) -> Result<Json<UserOut>> {
    Ok(Json(UserOut::from(user)))
}
```

Resolution for `CurrentUser<U>` is: read the `Session` the layer put in the request extensions,
`load()` it (one store round trip, with concurrent first loads serialised so two extractors still
cost one), decode the subject into `U::Id`, resolve `dyn UserStore<U>`, load the row, compare
`auth_hash` in constant time, then check `is_active`. Any failure is the same 401, whichever step it
was, and the session is destroyed on the way out. A store outage is a 503, never a silent logout.

All three session-reading extractors need a `Session` in the request extensions and cannot invent
one: the cookie they would have to read has already gone past. Without the layer installed above,
`AuthSession` answers 500 with a message naming `Slot::Session`, and `Principal` reports anonymous.

An endpoint that names none of these extractors performs zero store round trips: the layer builds
the session handle from the cookie and touches nothing until something calls `load`.
`MemorySessionStore::round_trips()` exists so that is an assertion in your tests rather than a claim
on this page.

### Guards

Two guards, applied with `Router::guard` and documented in the OpenAPI operation the same way.

- `RequireKind::new([PrincipalKind::Session, PrincipalKind::ApiKey])` restricts an endpoint to
  particular credential kinds and documents its 403. The kinds are `Anonymous`, `Session`, `Token`,
  `ApiKey` and `Service`.
- `Csrf::new(CsrfConfig::default())` applies the double-submit check to unsafe methods on
  cookie-authenticated requests only, exempting bearer and API-key traffic automatically. Details in
  [passwords and sessions](./passwords-and-sessions.md).

`Principal` prefers a `Principal` inserted into the request extensions over the one it would derive
from the session. The bearer-token and API-key extractor now inserts one, so `PrincipalKind::Token`
and `ApiKey` are produced for you; `Service` still comes only from your own code.

`LoginThrottle` is deliberately **not** a guard. Its per-identity tier keys on a field of the
request *body*, and a `Guard` only ever sees the parts, so the check runs inside the handler,
first, before any hashing. That ordering is the point: hashing is the expensive operation an
attacker is trying to make the server do, and a refused attempt must not pay for one. The address
tier rides on `moso-kv`'s own GCRA limiter and the identity tier is an exponential backoff over
`ThrottleConfig::per_identity_base`, saturating at `per_identity_max`. Both fail **closed**: a
throttle whose store is unreachable answers 503, because a limiter that stops limiting when its
store blinks is a limiter an attacker can remove.

## The account lifecycle

`Accounts<S>` owns the ordering, the tokens and the epoch; your `AccountStore` owns the columns. The
trait is eight small methods over your own user type: `find_by_identity`, `find_by_id`, `create`,
`password_hash`, `set_password_hash`, `set_identity`, `mark_verified` and `bump_epoch`. Nothing
ships that implements it, because a row has columns Moso knows nothing about. `create` takes a
`NewAccount` carrying an identity, a `PasswordHash` and an opaque JSON profile.

```rust title="src/auth.rs"
use std::sync::Arc;

use moso::auth::lifecycle::KvLifecycleTokens;
use moso::auth::{Accounts, PasswordPolicy, SessionStore};

/// `PgAccounts` is your own type: the eight-method `AccountStore` over your user table.
fn accounts(
    store: Arc<PgAccounts>,
    kv: moso_kv::Kv,
    sessions: Arc<dyn SessionStore>,
) -> Accounts<PgAccounts> {
    Accounts::new(store, KvLifecycleTokens::shared(kv), sessions)
        .policy(PasswordPolicy::default())
}
```

| Flow | What it does | What it returns |
| --- | --- | --- |
| `register` | policy check, hash, create | `Registration { outcome, user, token }` |
| `resend_verification` | mints a fresh verification token | `Option<IssuedToken>` |
| `verify_email` | redeems it and marks the account verified | the user |
| `request_password_reset` | revokes outstanding resets, mints one | `Option<IssuedToken>` |
| `reset_password` | bumps the epoch, deletes every session | the user |
| `change_password` | requires the current one, refuses reuse, revokes others | rows revoked |
| `request_email_change` | double opt-in | `EmailChange { confirmation, notify_previous }` |
| `confirm_email_change` | re-checks the address is still free | the user |
| `log_out` / `log_out_everywhere` / `sessions_of` | one, all (with an optional keep), list | `()` / `u64` / `Vec<SessionRecord>` |

Two things about this API surprise people, and both are on purpose.

**No mail is sent.** Every flow returns the token it minted and you send it, through your own
template, your own provider and your own queue. That is what keeps `moso-auth` from depending on
[the mailer](./mail.md), and it is why `Registration::token`, `EmailChange::confirmation` and
`EmailChange::notify_previous` are values in your hand rather than side effects. The mounted routes
hand the same tokens to the `TokenSink` you registered on `AuthState`, and log a warning naming
`AuthState::token_sink` when you registered none.

**The miss paths cost what the hit paths cost.** `register` hashes the password before checking
whether the address is taken, and still mints a reset token on the taken path, so the response time
is not a membership oracle and the "somebody tried to register with your address" mail can carry a
working link. `request_password_reset` mints and immediately burns a token against a subject no
account can have when the identity is unknown, so the round trips match. Respond identically
whatever `Registration::outcome` says: it exists for your logs, not for a response body.

Tokens live in any `moso_kv::Kv` through `KvLifecycleTokens`, keyed by purpose plus digest, so a
verification token can never be presented as a reset.

```rust
use moso::auth::lifecycle::KvLifecycleTokens;
use moso::auth::{LifecycleTokens, TokenPurpose};
use moso_kv::Kv;
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> moso::auth::Result<()> {
    let tokens = KvLifecycleTokens::new(Kv::in_memory("shop").unwrap());

    let issued = tokens
        .issue(TokenPurpose::VerifyEmail, "usr_1", "ada@example.com", Duration::from_secs(60))
        .await?;

    let claim = tokens.consume(TokenPurpose::VerifyEmail, issued.expose()).await?;
    assert_eq!(claim.unwrap().subject, "usr_1");

    // Single use.
    assert!(tokens.consume(TokenPurpose::VerifyEmail, issued.expose()).await?.is_none());
    Ok(())
}
```

`LifecycleConfig` sets the windows: `verification_ttl` 1 hour, `reset_ttl` 15 minutes,
`email_change_ttl` 1 hour, `revoke_sessions_on_password_change` true, `refuse_password_reuse` true.

## The built-in routes

The design is two tier, and both tiers ship. `moso::auth::routes()` mounts a working set of flows
under `/auth` so a prototype has login on day one, and `moso new --auth` copies handlers into your
project so you can edit them when the first requirement nobody anticipated arrives.

The copy is not the same code twice. It is written against a `User` type declared in **your** crate
rather than the framework's `DefaultUser`, it carries an `AccountStore` you point at your own
database, and every handler has `#[endpoint]` on it, so the flows appear in your own OpenAPI
document and `moso client` generates a typed client for them. The mounted set cannot do that:
`moso-auth` sits below the facade in the dependency graph and a macro expansion may only name
`::moso::__private::…`, so `#[endpoint]` is unavailable to it and its operations are registered as
undocumented. That difference is the reason the copy-out tier exists.

```sh
moso new shop --auth
```

writes `src/auth.rs` (the user type, the account store, the outbox a minted token is handed to, and
seven handlers: register, login, logout, the session listing, revoke-other-sessions, forgot password
and reset password) plus a `tests/auth.rs` that drives all of it over HTTP. The account store it
generates is a `HashMap` in the process, and it is the one thing in the file that is not
production shaped: it is eight small methods, each doc-commented with what it becomes against a
database. The hashing, the signed cookie, the single-use tokens and the enumeration and timing
defences are all real.

Nothing is mounted until a flag asks for it. `AuthRoutes` is a set of switches and `build()` turns
the ones that are on into a `Router`.

| Flag | Routes | Count |
| --- | --- | --- |
| `password()` | `POST /auth/register`, `/auth/login`, `/auth/logout`, `/auth/logout-all`; `GET /auth/me`; `POST /auth/verify-email` and `/auth/verify-email/resend`; `POST /auth/password/{forgot,reset,change}`; `POST /auth/email/change` and `/auth/email/change/confirm` | 12 |
| `sessions()` | `GET`, `POST` and `DELETE /auth/sessions`, and `DELETE /auth/sessions/{handle}` | 4 |
| `api_keys()` | `GET`, `POST` and `DELETE /auth/api-keys`, and `DELETE /auth/api-keys/{prefix}` | 4 |
| `oauth([..])` | `GET /auth/oauth/{provider}` and `GET /auth/oauth/{provider}/callback` | 2 |
| `passkeys()` | `POST /auth/passkeys/{register,login}/{start,finish}` | 4 |
| `totp()` | `POST /auth/totp/{setup,confirm,disable}` | 3 |
| `magic_link()` | `POST /auth/magic-link` and `GET /auth/magic-link/{token}` | 2 |
| `jwks()` | `GET /.well-known/jwks.json`, at the **root** | 1 |

Thirty-two routes with every flag on, and the whole set is asserted route by route in the crate's
own tests, so this table cannot quietly drift from the router.

Two of them are one pair of routes for however many OAuth providers you pass: the provider is a
path parameter matched against the list, so a name nobody configured is a 404 rather than a route
that exists and fails later. `sessions()`'s `POST` is not a second spelling of `/auth/login`; it
re-keys the session making the request, which is what a user who suspects their cookie leaked
actually wants. The listing hands out an opaque handle, the SHA-256 of the session id, because
handing a client a list of live session ids is handing it a list of credentials. `jwks()` mounts at
the root because a verifier told to fetch a JWKS will not go looking under `/auth`.

```rust
use moso::auth::{OAuthConfig, Provider, routes};

let google = Provider::google(OAuthConfig::new(
    "client-id",
    moso::config::SecretString::new("client-secret"),
    "https://app.example.com/auth/oauth/google/callback",
));

let router = routes()
    .password()
    .sessions()
    .oauth([google])
    .redirect_allowlist(["https://app.example.com"])
    .build();
```

`redirect_allowlist` is where a `next` parameter may point after login, and there is no "anything"
setting. An entry containing `*` is refused rather than applied, and the refusal is *remembered*:
the method returns `Self` and has nowhere to put an error, so `build()` panics naming the entry.
Call `AuthRoutes::validate()` first if you would rather report it. `validate_next` is the check
itself, and it is public: a relative path is always allowed, an absolute URL must match an
allowlisted origin exactly, and it refuses backslashes, control characters, protocol-relative URLs
and any value that means one thing literally and another once percent-decoded, because those are
what a *browser* rewrites before it resolves a URL, not what a parser objects to.

> [!IMPORTANT]
> The mounted routes are fixed to `DefaultUser`. `AuthRoutes::build` has no type parameter, so the
> handlers it registers are one concrete instantiation, and the account store is taken as
> `Arc<dyn AccountStore<User = DefaultUser>>`. An application with its own `User` type copies the
> handlers out of `crates/moso-auth/src/routes/` and names its own types in them. There is no
> generic version and no plan to erase `Accounts<S>`, which holds an `Arc<S>` and therefore cannot
> be a trait object.

### What the OpenAPI document says about them

Less than a `#[endpoint]` would say, and the crate is explicit about it rather than filling the gap
with invented metadata. `moso-auth` sits below the facade, so `#[endpoint]`, `routes!` and `ep!`
(which expand to `::moso::__private::…`) are not available to it. Every route is registered through
`Router::post` / `get` / `delete` and therefore carries `UndocumentedEndpoint`, which stamps the
operation `x-moso-undocumented` and contributes no parameters, no request body and no response body.

What *is* documented is documented because somebody wrote it down and it is true of every route in
the group: the `auth` tag on all 32, the 429 on the four throttled password routes plus the TOTP and
magic-link routes, the 503 an unreachable store produces, and the 401 the authenticated routes
produce. A test asserts that a route documents its 429 exactly when it can produce one.

The bodies themselves are real `Schema` types: `moso-auth` writes the `Schema` and `Validate` impls
by hand, since `#[derive(Schema)]` is above it, so the schemas exist and validate; only the
registration path does not carry them into the document. Twenty-one request and response types are
public, and they are the reference shape for these flows:

| Type | Shape | Direction |
| --- | --- | --- |
| `RegisterRequest` | `email: Email`, `password: Password`, `name: Option<String>` | in |
| `LoginRequest` | `identity`, `password`, `totp`, `challenge`, `next` | in |
| `LoginResponse` | `requires_second_factor`, `challenge`, `access_token`, `next` | out |
| `ForgotPasswordRequest` / `ResetPasswordRequest` | `email` / `token` plus `password` | in |
| `ChangePasswordRequest` | `current_password`, `new_password`, `logout_other_sessions` | in |
| `AcknowledgedResponse` | one constant sentence, so 202 is byte-identical either way | out |
| `SessionSummary` | `handle` (not the session id), `label`, `ip`, timestamps, `current` | out |
| `CreatedApiKey` / `ApiKeySummary` | the secret, once / everything but the secret | out |
| `TotpSetupResponse` | `secret`, `provisioning_uri` | out |
| `PasskeyChallengeResponse` / `PasskeyFinishRequest` | an `OpaqueJson` the browser's API owns | both |

Every one is `#[non_exhaustive]`, so your crate cannot construct one with a struct literal: read
them and mirror the ones you need as your own `#[derive(Schema)]` types, which is what
[schemas](./schemas.md) are for. `LoginResponse` is worth copying carefully: it carries no token by
default, because the session is in an `HttpOnly` cookie and putting a copy in the body invites a
client to store it where JavaScript can read it. `access_token` is for a client that asked for token
authentication, not for a browser.

### What `AuthState` has to carry

One provider rather than a dozen, because these handlers are meant to be copied into an application
and a generated file that has to be edited whenever a dependency is added is a generated file that
rots. `AuthState::new` takes the one thing every flow needs, a session store, and everything else
is a builder method. A route whose dependency was never added answers 500 with a sentence naming the
call that fixes it, rather than pretending to work.

| Builder call | Needed by |
| --- | --- |
| `accounts(store, tokens)` | every password, verification, reset, email-change, OAuth and magic-link route |
| `throttle(LoginThrottle)` | optional; without one, `gate` lets every attempt through |
| `captcha(verifier)` | optional; without one a `ThrottleDecision::Challenge` is a refusal |
| `api_keys(store)` | the four `/auth/api-keys` routes |
| `passkeys(store)` + `webauthn(WebAuthn)` | the four passkey ceremonies |
| `jwt(Jwt)` | `/.well-known/jwks.json` |
| `kv(Kv)` | the TOTP and magic-link routes, which keep their own state |
| `token_sink(sink)` | anything that mints a token; a warning otherwise |
| `refresh(store)` | the mounted `POST /auth/refresh` route rotates a refresh token through it, with reuse detection |
| `session_config`, `password_policy`, `issuer` | defaults, overridable |

Set `password_policy` **before** `accounts`, which copies it into the lifecycle so the policy is
written down once.

## Where credentials are stored

Four things have to survive between two requests, and each of them has two shipped stores: a map,
complete and per-process, and a table, the same semantics somewhere a second instance can see.

| Trait | In memory | In a table | Its table |
| --- | --- | --- | --- |
| `SessionStore` | `MemorySessionStore` | `store::TableSessionStore` | `moso_auth_sessions` |
| `RefreshStore` | `MemoryRefreshStore` | `store::TableRefreshStore` | `moso_auth_refresh_tokens` |
| `ApiKeyStore` | `MemoryApiKeyStore` | `store::TableApiKeyStore` | `moso_auth_api_keys` |
| `PasskeyStore` | `store::MemoryPasskeyStore` | `store::TablePasskeyStore` | `moso_auth_passkeys` |

`KvSessionStore` is a fifth session store and the usual default: it puts sessions in Redis, in
PostgreSQL as a key-value store, or in a map.

A `Memory*` store is not a slower table, it is a different deployment. A session issued by one
instance is unknown to the other, a refresh token rotated on one is unknown to the other, and a
passkey quarantined on one still works on the other. One process is fine; two is broken in a way
that looks like intermittent logouts. One conformance suite runs against both halves of each trait,
on SQLite and on real PostgreSQL, so the map and the table cannot drift apart.

Every table statement is one string that runs on PostgreSQL and on SQLite. Timestamps are RFC 3339
text with a fixed sub-second width, so `expires_at > $1` sorts lexicographically and needs no cast
on either backend.

### Getting the tables

Nothing here creates a table behind your back: a migration is read before it is run. There are two
ways in.

**Let the generator write it.** `moso_auth::store::descriptors()` returns all four tables as
`EntityDescriptor`s. Add them to the entity list your project's `src/db.rs` passes to
`make_migration`:

```rust
use moso_migrate::command::{self, MakeMigrationOptions};
use moso_orm::descriptor::EntityDescriptor;

let mut entities: Vec<&EntityDescriptor> = my_entities();
entities.extend(moso_auth::store::descriptors());

let report = command::make_migration(
    "migrations",
    backend,
    &entities,
    &MakeMigrationOptions::default().name("create auth tables"),
)?;
```

Then, from a shell:

```sh
moso db make-migration create_auth_tables
moso db migrate
```

You get the migration, its reverse and a snapshot, and from then on `moso db check` reports drift on
these tables like any other.

**Or copy the DDL.** `store::SESSIONS_SCHEMA`, `store::REFRESH_TOKENS_SCHEMA`,
`store::API_KEYS_SCHEMA` and `store::PASSKEYS_SCHEMA` are the `create table` statements, with the
index constants beside them, for a project that writes its migrations by hand. The two forms are
checked against each other by the crate's own test, column by column and index by index, so they
cannot describe different tables.

Each table store also has `create_table()`, which runs those constants. That is for tests and for
`moso dev`, not for a deployment.

`user_id`, `owner` and `subject` are indexed `text` columns that reference nothing. `moso-auth`
cannot know what your user table is called or whether its key is a `uuid`, a `bigint` or a slug, and
a foreign key it guessed wrong would be a failed migration on your production database. Add the
constraint yourself if you want one.

## Which credential do you want

| You want | Use | Page |
| --- | --- | --- |
| A browser session with a password login | `SessionLayer`, `DatabaseBackend`, `PasswordHash` | [passwords and sessions](./passwords-and-sessions.md) |
| A service-to-service token, or programmatic access | `Jwt`, `RemoteJwks`, `ApiKey` | [JWT and API keys](./jwt-and-api-keys.md) |
| "Sign in with Google" or a phishing-resistant factor | `Provider`, `WebAuthn` | [OAuth and passkeys](./oauth-and-passkeys.md) |
| A second factor, recovery codes, or a magic link | `TotpEnrollment`, `RecoveryCodes`, `MagicLink` | covered on this page |
| To decide what the principal may do | `moso-authz` | [permissions](./permissions.md) |

Sessions are for browsers and JWT is for services. JWT-as-session is a common and expensive mistake
(no revocation, no logout, silent expiry); the crate supports it without recommending it.

## Reference implementations

`moso-auth` is now used by the `crud` example, which authenticates with its `ApiKeyAuthenticator`
over `#[derive(Entity)]` and the ORM on SQLite rather than a hand-rolled key store, so there is an
end-to-end reference to read; `crates/moso-auth/tests/public_surface.rs` is the other, writing an
`AuthUser`, an `AuthBackend` and the extractors the way an application would.

## See also

- [Passwords and sessions](./passwords-and-sessions.md) for hashing, cookies, fixation and logout.
- [Permissions and roles](./permissions.md) for the other half of the split.
- [Dependency injection](./dependency-injection.md) for `Depends`, `Inject` and `provide_dyn`.
- [Errors](./errors.md) for the problem documents these failures become.
- [Cache and key value store](./cache.md) for the `Kv` the session and token stores sit on.
- [Health and shutdown](./health-and-shutdown.md) for where `AuthHealthCheck` is reported.
