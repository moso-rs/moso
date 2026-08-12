# 30 — Authentication

> 🟡 **LARGELY BUILT.** `moso-auth` is the workspace's largest battery (~37k lines across `session`,
> `password`, `jwt`, `jwks`, `oauth`, `webauthn`, `totp`, `mfa`, `apikey`, `throttle`, `extract`,
> `routes`, `store`, `lifecycle`, `backend`, `config`, `error`), reached through the facade's
> off-by-default `auth` feature as `moso::auth`. What is **not** built is listed under
> [what is still owed](#what-is-still-owed) and marked inline; where the code deliberately diverged
> from this document, the divergence is stated where it happened rather than quietly reconciled. See
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).

## Scope

Authentication answers "who is making this request." Authorization ("may they do this") is
`31-authorization.md`. Keeping them separate is deliberate: conflating them is why most frameworks
ship a login form and call it security.

Moso ships: sessions, password auth, API keys, JWT (for service-to-service and SPAs that need it),
OAuth2/OIDC social login, passkeys/WebAuthn, TOTP two-factor, magic links, and the account
lifecycle flows (verification, password reset, email change) that everyone rewrites badly.

## The core traits

```rust
// moso-auth/src/user.rs, moso-auth/src/backend.rs — as built
/// Your user type opts in to being an authenticatable principal.
pub trait AuthUser: Clone + Send + Sync + 'static {
    type Id: Serialize + DeserializeOwned + Clone + Eq + Hash + Send + Sync + 'static;
    fn auth_id(&self) -> Self::Id;
    /// Changes when credentials change; invalidates all sessions on password reset.
    fn auth_hash(&self) -> Vec<u8>;
    fn is_active(&self) -> bool { true }
}

/// How to load a principal and verify credentials.
pub trait AuthBackend: Send + Sync + 'static {
    type User: AuthUser;
    type Credentials: Send + 'static;

    fn authenticate<'a>(&'a self, c: Self::Credentials, ctx: &'a AuthCtx)
        -> BoxFuture<'a, Result<Option<Self::User>>>;
    fn load<'a>(&'a self, id: &'a <Self::User as AuthUser>::Id)
        -> BoxFuture<'a, Result<Option<Self::User>>>;
}
```

`AuthBackend` is dyn-compatible and therefore hand-writes its `BoxFuture`s. `#[async_trait]` is
banned workspace-wide (`00-foundations/02-architecture.md`), so an earlier draft of this document
that spelled the trait with it was describing something the tree would have rejected.

The `auth_hash` mechanism (a hash of the password hash + a per-user session epoch) is what makes
"log out everywhere" work without scanning the session store. It is checked on every session load;
a mismatch drops the session. `is_active` is checked on the same path, and *after* the password
verification, so a suspended account is not a faster answer than a wrong password.

Every `AuthBackend` becomes a `UserStore<Self::User>` through a blanket impl. `UserStore` is the
half the extractors resolve, registered with `provide_dyn::<dyn UserStore<User>>`.

## The default backend

Most apps do not want to implement a trait to log a user in. `moso-auth` ships a
`DatabaseBackend<U>` that works with any entity having the expected columns:

```rust
// example — src/lib.rs
let backend = DatabaseBackend::<User>::new(db)
    .identity_column(User::EMAIL)
    .password_column(User::PASSWORD_HASH)
    .active_column(User::IS_ACTIVE);
backend.validate()?;                     // a boot error naming the entity and the missing column

App::new(cfg)
    .provide_dyn::<dyn UserStore<User>>(Arc::new(backend))
    .provide(AuthState::new(session_store).accounts(accounts, tokens))
    .mount(moso::auth::routes()
        .password()
        .oauth([Provider::google(cfg.oauth.google), Provider::github(cfg.oauth.github)])
        .passkeys()
        .totp()
        .build())
```

> **Divergence — `App::with_auth` was never built, and should not be.** This document's original
> example wired the backend through a dedicated `AppBuilder` method. What ships instead is two
> ordinary providers: the backend as `dyn UserStore<U>`, and an `AuthState` the mounted handlers
> take as `Inject<AuthState>`. A dedicated builder method would have had to know about the session
> store, the throttle, the account store, the token sink, the relying party and the signer — every
> dependency `AuthState` carries — which is a second place to maintain the same list. Nothing about
> the framework requires an ADR to record this; it is the DI rule (`00-foundations`) applied.

Two behaviours of `DatabaseBackend` are load-bearing and easy to lose:

- The miss path runs a dummy argon2 verify, so "no such account" and "wrong password" cost the same.
- Identity matching is two exact comparisons (`column = normalised OR column = trimmed`), never
  `ilike`: an address containing `_` is a `LIKE` wildcard, and `_` matching any character would let
  one account's password sign in as another's. Case insensitivity belongs in a unique index on
  `lower(email)`.

### The second factor is part of `authenticate`, not on top of it

`authenticate` has three answers: `Ok(Some(user))`, `Ok(None)`, and
`Err(Error::SecondFactorRequired { challenge })`. The third is an `Err` and not a third `Ok` variant
because it is the one outcome a caller must not be able to ignore by matching on `Some` —
`let Some(user) = authenticate(..)? else` is the shape everybody writes, and with a third `Ok`
variant it would have signed the user in.

```rust
// example — as built
DatabaseBackend::<User>::new(db)
    .identity_column(User::EMAIL)
    .password_column(User::PASSWORD_HASH)
    .second_factor(User::TOTP_SECRET, User::TOTP_LAST_PERIOD, SecondFactorChallenges::new(kv))
```

One builder call taking all three, because a backend holding two of them either cannot check a code
or cannot refuse a replayed one, and neither half-state should be constructible. An account whose
secret column is `NULL` is unaffected, so enrolment rolls out one user at a time.

The challenge is `mfa::SecondFactorChallenge`, minted and claimed by `mfa::SecondFactorChallenges`
over `moso-kv`, and it is **the** partial-authentication mechanism in the crate: one home, so two
paths cannot disagree about how long a half-finished login lives. Three properties, all enforced by
the store rather than by a handler:

| Property | How | Why |
| --- | --- | --- |
| Bound | the stored value is the account's subject, handed back on redemption for the caller to compare | otherwise a challenge earned against one account signs in another |
| Expiring | a stored `expires_at` *and* the store's ttl, both checked (`SECOND_FACTOR_TTL`, 5 min) | a partial authentication that never dies is a password with extra steps |
| Single-use | the claim *is* the store's `delete`, whose answer says whether this caller removed it | two requests racing one challenge must not both win, and one challenge must buy one code attempt |

The claim happens **before** the code is checked, so a challenge cannot be used to brute-force
codes; and the accepted period is written back to `last_period` **before** the login succeeds, so a
store that refuses that write refuses the login rather than leaving the code replayable for the rest
of its window. The token itself is never stored: the key is its SHA-256 and the value holds no
secret, so the namespace can be dumped without yielding a live credential.

### The mountable routes

`moso::auth::routes()` mounts a working set of flows under `/auth`, one flag at a time; nothing is
mounted until a flag asks for it. The intent was always two-tier: **`moso new --auth` copies the
route handlers into the user's project** rather than hiding them in the framework, because auth
flows always need customisation (extra profile fields, a different email, an audit hook) and a
framework that hides them behind a config object gets abandoned at the first requirement it did not
anticipate. The mountable version exists for prototyping and is documented as such.

> **Not built: the second tier.** `moso new --auth` does not exist — `moso new` has a `--with-db`
> flag and no `--auth` one (`04-devex/40-cli.md`). Copying a handler today means copying it out of
> `crates/moso-auth/src/routes/` by hand. Until it lands, the mounted set is the only tier, and the
> handlers are written to be readable as the thing that will be copied.

| Flag | Routes | Count |
| --- | --- | --- |
| `password()` | `POST /auth/register`, `/auth/login`, `/auth/logout`, `/auth/logout-all`; `GET /auth/me`; `POST /auth/verify-email` and `/auth/verify-email/resend`; `POST /auth/password/{forgot,reset,change}`; `POST /auth/email/change` and `/auth/email/change/confirm` | 12 |
| `sessions()` | `GET`, `POST` and `DELETE /auth/sessions`, and `DELETE /auth/sessions/{handle}` | 4 |
| `api_keys()` | `GET`, `POST` and `DELETE /auth/api-keys`, and `DELETE /auth/api-keys/{prefix}` | 4 |
| `oauth([..])` | `GET /auth/oauth/{provider}` and `GET /auth/oauth/{provider}/callback` | 2 |
| `passkeys()` | `POST /auth/passkeys/{register,login}/{start,finish}` | 4 |
| `totp()` | `POST /auth/totp/{setup,confirm,disable}` | 3 |
| `magic_link()` | `POST /auth/magic-link` and `GET /auth/magic-link/{token}` | 2 |
| `bearer()` | `POST /auth/token` and `POST /auth/refresh` | 2 |
| `jwks()` | `GET /.well-known/jwks.json`, at the **root** | 1 |

Thirty-four routes with every flag on, asserted route by route in `routes.rs`'s own tests so the
table cannot drift. `POST /auth/sessions` re-keys the session making the request — the operation a
user who suspects their cookie leaked wants — and is deliberately not a second spelling of
`/auth/login`. The session listing hands out an opaque handle (the SHA-256 of the identifier), never
a session id, because a list of live session ids is a list of credentials.

> **Divergence — the mounted operations are not fully documented, and this was chosen.** This
> document said "`moso::auth::routes()` mounts documented endpoints". They are registered through
> `Router::post`/`get`/`delete` and therefore carry `UndocumentedEndpoint`, so their request and
> response **bodies** are stamped `x-moso-undocumented`. `moso-auth` sits below the facade and
> `#[endpoint]`, `routes!` and `ep!` all expand to `::moso::__private::…`, so the macros are
> unavailable to it. What *is* documented is written by hand and true of every route in its group:
> the `auth` tag, the 429 on the throttled routes, the 503 an unreachable store produces, and the
> 401 on the authenticated ones. The bodies themselves are real `Schema` types (the impls are
> hand-written, since `#[derive(Schema)]` is also above this crate), so the schemas exist and
> validate; only the registration path does not carry them into the document. Inventing metadata to
> close the gap would break the "never synthesise plausible-looking metadata" rule.
> [ADR-0016](../adr/0016-battery-routes-documentation-and-boot-check-boundary.md) records the
> decision: the mounted set is the prototyping tier and stays honestly `x-moso-undocumented`; the
> copy-out tier (`moso new --auth`) is where the routes become documented `#[endpoint]` handlers. The
> same ADR covers why the empty boot-check `required_providers()` is not cheaply closed for the
> mounted set, and why the copy-out closes it structurally.

> **Divergence — the mounted routes are fixed to `DefaultUser`.** `AuthRoutes::build` has no type
> parameter, so the handlers it registers are one concrete instantiation, and the account store is
> taken as `Arc<dyn AccountStore<User = DefaultUser>>`. `Accounts<S>` holds an `Arc<S>` and `S`
> carries the implicit `Sized` bound, so `Accounts<dyn AccountStore<..>>` does not exist; the crate
> bridges it with a private `ErasedAccountStore` newtype for `DefaultUser` only. An application with
> its own `User` copies the handlers. This is the same decision as the tier split, recorded as a known
> limitation in [ADR-0016](../adr/0016-battery-routes-documentation-and-boot-check-boundary.md) rather
> than as its own ADR, and it is the reason the copy-out tier is not optional in the long run.

`AuthState` is the one provider every mounted handler takes. One struct rather than a dozen
providers, because these handlers are meant to be copied into an application and a generated file
that must be edited whenever a dependency is added is a generated file that rots. A route whose
dependency was never configured answers 500 naming the builder call that fixes it —
`AuthState::accounts`, `::webauthn`, `::jwt`, `::api_keys`, `::passkeys`, `::kv` — rather than
pretending to work.

`redirect_allowlist` on the router is what `routes::validate_next` reads. `AuthConfig` carries a
list of the same name that nothing feeds into the router: `AuthConfig::validate` checks its entries
are bare origins and stops there, so a composition root that keeps the allowlist in configuration
has to pass it to `AuthRoutes::redirect_allowlist` itself. A relative path on this origin is always
allowed; anything else
must match an allowlisted origin exactly. It refuses backslashes, control characters,
protocol-relative URLs and any value that means one thing literally and another once
percent-decoded, because those are what a *browser* rewrites before it resolves a URL. A wildcard
allowlist entry is refused rather than applied, and the refusal is remembered until `build()`, which
panics naming it — an open redirect configured by accident must not become a live route.

## Sessions

```rust
// moso-auth/src/session.rs — as built
pub struct Session { /* an Arc over the shared record */ }
impl Session {
    pub fn get<T: DeserializeOwned>(&self, k: &str) -> Result<Option<T>>;
    pub fn insert<T: Serialize>(&self, k: &str, v: T) -> Result<()>;
    pub fn remove(&self, k: &str) -> Result<()>;
    pub fn take<T: DeserializeOwned>(&self, k: &str) -> Result<Option<T>>;
    pub async fn load(&self) -> Result<()>;
    pub async fn save(&self) -> Result<bool>;
    pub async fn log_in<U: AuthUser>(&self, user: &U) -> Result<()>;  // cycles the id
    pub async fn cycle_id(&self) -> Result<()>;     // MUST be called on privilege change
    pub async fn destroy(&self) -> Result<()>;
    pub fn id(&self) -> SessionId;
}
```

- **Store:** any `KvStore` (`25-kv-cache.md`) through `KvSessionStore`, a `moso_auth_sessions` table
  through `TableSessionStore` (PostgreSQL and SQLite, on the same statements), or
  `MemorySessionStore`, which is a real store with real semantics plus round-trip counters so a test
  can assert *how many* round trips a request cost. `FailureMode::Fail` — a session store outage
  must not silently log everyone out.
- **Cookie:** `HttpOnly`, `Secure`, `SameSite=Lax` by default, `__Host-` prefix when the path
  allows, signed with a rotating key set (`AuthConfig::secret_keys`: the first signs, the rest only
  verify, so rotation does not log everybody out). Relaxing `Secure` is `allow_insecure_cookies`
  plus `AuthConfig::cookie_for(Profile::Dev)`, which drops `Secure` — and with it the `__Host-`
  prefix, which is only honoured on a secure cookie — in development *only*, and logs a warning in
  any other profile rather than obeying.
- **Lazy loading:** the store is not touched unless the handler reads or writes the session. A
  static endpoint costs zero Redis round trips.
- **ID cycling on login** (fixation defence) is done by `Session::log_in`, not left to the user.
- **Rolling expiry** with an absolute cap: `idle_timeout` (default 14 d), `absolute_timeout`
  (default 90 d).
- **Session listing** with device/UA/IP/last-seen so users can review and revoke. Each store keeps
  its own per-user index, which is what makes it one lookup: `KvSessionStore` a `SessionIndex`
  namespace holding the user's live identifiers, `TableSessionStore` the `SESSIONS_USER_INDEX`
  statement it ships beside the DDL.

Installing it is `stack.replace_custom(Slot::Session, layer)` — the `CustomLayer` sibling of
`MiddlewareStack::replace`, which takes a `tower::Layer<Route>` and would not accept it. The pair
cannot be one method: widening `replace` to take either needs two overlapping blanket impls, which
the compiler rejects (E0119). `insert_before_custom`, `insert_after_custom` and `append_custom` are
the same pairing for the other three installers. The entry keeps the slot's name, so
`moso middleware` prints `session` and the layer's summary rather than a type, and
`MiddlewareStack::validate` stops reporting the slot as empty. Nothing installs it *for* an
application, which is the DI rule rather than a gap: the layer needs a store and a key set that only
the composition root has.

## Passwords

```rust
// moso-auth/src/password.rs — as built
pub struct PasswordHash(String);        // PHC string format
impl PasswordHash {
    pub async fn new(plain: &Password) -> Result<Self>;            // argon2id, on the blocking pool
    pub async fn verify(&self, plain: &Password) -> Result<VerifyOutcome>;
    pub fn needs_rehash(&self) -> bool;                  // params changed → upgrade on next login
}
pub enum VerifyOutcome { Ok, OkNeedsRehash, Invalid }
```

Both are `async` because both go through `moso::task::blocking`; the original spec wrote them
synchronous, which would have put an argon2 hash on the runtime.

- **argon2id** with parameters from a **calibration routine**, `moso_auth::calibrate(target)`,
  targeting `TARGET_HASH_TIME` (250 ms) on the deployment hardware, floored at OWASP's minimum
  (19 MiB, t=2, p=1) — which is also `HashParams::default()`. The calibrated values go in
  `AuthConfig::hash_params`; leaving it unset is a `WARN` at `AuthConfig::validate`, naming the
  floor it will run on, and parameters *below* the floor are a boot error.
- Hashing runs on `moso::task::blocking` with a **bounded** pool, so a login flood cannot starve
  the async runtime. This is a real DoS vector that most frameworks leave open.
- Constant-time comparison; `password::dummy_verify` runs when the user does not exist, so response
  timing does not reveal account existence.
- **Breach check** against a local Bloom filter, plus an optional k-anonymity HIBP lookup. Rejected
  with `Error::PasswordPolicy { code: "breached", .. }`.
- **No composition rules** (no "one uppercase and a symbol"). Length ≥ 12, breach check, and a
  strength estimate scored 0–4 with a default floor of 2. Current NIST guidance, and the docs
  explain why. `PasswordPolicy::banned_words` was added on top: the product's name and the user's
  own address make a password guessable for *this* application in a way no general scorer can know.
  The stable codes are `len`, `banned`, `breached`, `weak` and — from the lifecycle — `reused`.

> **Divergence — what is embedded is a generator, not a corpus.** This document specified "a local
> bloom filter of the top 100k passwords (embedded, 200 KB)". Shipping somebody else's breach corpus
> in the source tree is a licensing question and a multi-megabyte blob in every build, so what is
> embedded is a seed list plus the suffix, capitalisation and character-substitution rules that
> dominate every published corpus, expanded into the filter on first use. `EMBEDDED_CORPUS_NOTE`
> states this in the public API rather than glossing it, `BreachCheck::with_extra_list` takes the
> real list for an application that wants it, and the k-anonymity API covers the long tail. **This
> deserves an ADR**, because it changes what "breach check" promises.

The k-anonymity lookup needs a `RangeFetcher` the application installs: `moso-auth` deliberately has
no general-purpose HTTP client for it, and a `BreachCheck::api` with no fetcher is a configuration
error rather than a silent skip. It fails **open** on a timeout — the embedded filter has already
run, and a slow third party must not stop people signing up.

## Rate limiting and lockout

`LoginThrottle`, over a `moso_kv::Kv`. Applied by the mounted routes to `login`, `register`,
`password/forgot`, `verify-email/resend`, `magic-link` and the TOTP routes:

- Per-address GCRA quota (`moso-kv`'s own limiter) **and** per-identity backoff, so one attacker
  cannot lock out a victim by hammering their account — past `per_identity_free` failures the delay
  is `per_identity_base · 2^(failures − free − 1)`, saturating at `per_identity_max`, rather than a
  hard lock.
- Fail **closed**: an unreachable throttle store is a 503, not an allow. A limiter that stops
  limiting when its store blinks is a limiter an attacker can remove.
- Failed attempts recorded with IP and UA (`AttemptRecord`, read back with `recent`);
  `should_notify` returns true **once** per window past `notify_after` failures, so a sustained
  attack is one email rather than a mail bomb. `LoginThrottle::notice` is the call the login path
  makes: it claims that marker *and* assembles the `SecurityNotice` to send — the failure count, the
  window, the recent attempts — so the marker and the evidence come from the same moment and cannot
  drift. Sending it is still the application's job, because `moso-auth` does not depend on
  `moso-mail`; the hand-off is a `NoticeSink`, the same
  `Arc<dyn Fn(..) -> BoxFuture<'static, ()>>` shape as the `TokenSink`, and with none registered the
  notice is dropped with a WARN. A `SecurityNotice` carries no token and has no `expose()`, which is
  the same structural separation `DeliveryPurpose` keeps from `TokenPurpose`: an alert sink can
  never be handed a live credential.
- CAPTCHA hook (`CaptchaVerifier` trait) invoked after `challenge_after` failures, which is
  `ThrottleConfig::CHALLENGE_OFF` by default. With no verifier configured, or one that says the
  token did not check out, a challenge is a **refusal**: treating "we cannot check" as "let them
  through" would make the challenge tier a way to skip the throttle. That refusal is only defensible
  because the tier is now off unless somebody turns it on — with `challenge_after` at 3 and no
  verifier in the tree, three mistyped passwords produced a 429 with nothing that could clear it,
  and anyone who knew a victim's address could put them there deliberately, which is the hard lock
  the per-identity tier exists to avoid. Neither the address quota nor the backoff changed.
  `captcha::HttpCaptchaVerifier` is the shipped implementation: one form `POST` over the crate's
  existing hyper/rustls transport, covering Turnstile, hCaptcha and reCAPTCHA (they verify
  identically), with an unreachable provider an `Unavailable` and a rejected *secret* a `Config`
  error rather than a failed token — reporting a bad secret as a failed token is a permanent lockout
  that looks exactly like an attack in progress.

> **Divergence — the throttle is not a `Guard`.** A `Guard` sees only the request parts, and the
> per-identity tier keys on a field of the request *body*, so the check runs as the first `await`
> inside the handler instead. That ordering is what keeps a refused attempt from paying for a
> password hash. The cost is that the 429 is declared per route group by hand rather than derived
> from a guard's `describe`. A body-reading guard would break the extraction invariants in
> `00-foundations`; the decision to keep the throttle a service-layer check is recorded in
> [ADR-0017](../adr/0017-moso-auth-seams-to-the-application.md).

## JWT

Supported, with an opinion in the docs: **sessions for browsers, JWT for service-to-service and
mobile.** JWT-as-session is a common mistake (no revocation, no logout, silent expiry).

```rust
// moso-auth/src/jwt.rs — as built
pub struct Jwt<C = Claims> { /* … */ }
impl<C> Jwt<C> {
    pub fn issuer(config: JwtConfig, kid: impl Into<String>, key: SecretBytes) -> Result<Self>;
    pub fn issue(&self, claims: &C, ttl: Duration) -> Result<String>;
    pub fn verify(&self, token: &str) -> Result<C>;
    pub fn jwks(&self) -> serde_json::Value;
}
```

- Algorithms: EdDSA (Ed25519) default, ES256, RS256. **HS256 requires explicit opt-in**
  (`JwtConfig::allow_symmetric`) — refused at `Jwt::issuer` *and* at `AuthConfig::validate`, so the
  failure lands at boot rather than at the first token — because symmetric secrets in multi-service
  setups are how tokens leak. `Jwt::jwks` drops HMAC keys from the document rather than publishing
  the signing key.
- `alg: none` and algorithm confusion are rejected structurally (the verifier is constructed with a
  fixed algorithm; the header's `alg` is never trusted).
- Key rotation via a JWKS endpoint (`GET /.well-known/jwks.json`, mounted by `routes().jwks()` at
  the root) with `kid`, and a `RemoteJwks` verifier with caching for consuming other services'
  tokens.
- Short access tokens (`access_ttl`, 15 min) + rotating refresh tokens with **reuse detection**: a
  replayed refresh token revokes the whole family and alerts. This is the OAuth BCP and almost
  nobody implements it.

Two stores implement `RefreshStore`. `MemoryRefreshStore` serialises `exchange` behind a
`std::sync::Mutex`, which is correct for one process and says nothing to a second;
`store::TableRefreshStore` puts the families in `moso_auth_refresh_tokens` and makes the reuse
detection a **compare-and-set** — `update … set used = $1 where token_hash = $2 and used = $3 and
expires_at > $4`, with the affected row count as the answer, so there is no window between a read
and a write for a second process to also decide it won. The exchange runs inside a transaction, so
the loser's family burn always sees the successor the winner minted rather than racing it. Two
concurrent exchanges of one token produce exactly one rotation and one `ReuseDetected`, and
`store::conformance` asserts that against both stores from one suite, on SQLite and on PostgreSQL.

`routes().bearer()` mounts the two routes that reach this. `POST /auth/token` exchanges a password
— and a TOTP code, when the account has one enrolled — for a fresh pair: the access token from
`AuthState::jwt`, the refresh token from `AuthState::refresh`. It is stateless and sets no cookie,
which is what keeps it apart from the cookie login at `POST /auth/login`; both return a
`LoginResponse`, but only this flow populates its `access_token` and `refresh_token`.
`POST /auth/refresh` calls `RefreshStore::exchange`, so the compare-and-set and the reuse detection
above are now reachable over HTTP: a rotation returns the next pair, and a replayed token is the same
401 as an unknown one — indistinguishable to the client — after `exchange` has already burned the
family. `/auth/token` is throttled exactly as `/auth/login` is; `/auth/refresh` is not, because the
refresh token is 256 bits of opaque entropy with nothing for a per-identity backoff to key on.

## OAuth2 / OIDC

```rust
// example — as built
Provider::google(OAuthConfig::new(client_id, client_secret, redirect_uri))
    .scopes(["openid", "email", "profile"])
    .link_policy(LinkPolicy::VerifiedEmailOrSession)
```

- PKCE always, `state` bound to the session, `nonce` verified for OIDC. All three secrets live in
  the session and nowhere else, and `AuthorizationRequest` prints its URL without them.
- Built-in providers: Google, GitHub, Microsoft/Entra, Apple, GitLab, Discord, Slack, plus
  `Provider::oidc(discovery_url)` for anything with a discovery document and
  `Provider::self_hosted(..)` for a fixed endpoint set. A Microsoft configuration with no `tenant`
  is the multi-tenant `common` endpoint, which accepts any Microsoft account in the world; the field
  exists so that is a decision rather than an accident.
- Account linking: linking a provider to an existing account requires a verified matching email or
  an authenticated session. Unverified-email auto-linking is a known account-takeover path and Moso
  refuses it by default (`LinkPolicy::default() == VerifiedEmailOrSession`) with a documented
  override.
- The token exchange goes over hyper behind an `HttpTransport` trait, not `reqwest`: reqwest 0.13's
  `rustls` feature pulls a second rustls crypto provider into a process that already installed
  ring's through sqlx, which is a runtime panic rather than a compile error.

> **Divergence — there is no `on_link` callback.** This document's example registered a closure that
> mapped a provider profile onto a local user. What ships is `LinkPolicy` plus `check_link`, and the
> mapping itself is the mounted callback handler (or your copy of it), which has the `AccountStore`
> in hand. A closure taking a `db` would have had to be generic over the connection type or box it,
> and it would have hidden the one decision — link or refuse — that a reviewer most needs to see.

## Passkeys / WebAuthn

**Behind an off-by-default `passkeys` cargo feature** ([ADR-0015](../adr/0015-webauthn-openssl-exception.md)).
`webauthn-rs-core` links OpenSSL and every crate in its subtree is MPL-2.0, so a default build of the
battery — and of a `moso` app on `--features auth` — pulls none of it and needs no C toolchain; only a
project that turns on `moso-auth/passkeys` (or the facade's `moso/passkeys`, which implies `auth`)
compiles the ceremonies, the `WebAuthn`/`PasskeyStore`/`PasskeyCredential` surface, the `TablePasskeyStore`
and the `moso_auth_passkeys` descriptor. The `auth` feature must never imply `passkeys`. `cargo deny`
judges `all-features`, so the ADR-0015 licence/ban exceptions are still required for that gate — the
feature gate is about the *build*, not the *audit*.

Via `webauthn-rs`. Registration and authentication ceremonies, counter/clone detection, and the
discoverable-credential (usernameless) flow, all exercised against the project's own virtual
authenticator (`webauthn-authenticator-rs`, dev/test-only — an optional dependency the `passkeys`
feature enables, because Cargo forbids optional dev-dependencies). Ceremony state is serialisable and
lives in the session, which is what makes passkeys work on more than one process without sticky sessions.
This matters disproportionately for the "modern framework" positioning — very few backends ship it
turnkey.

`PasskeyStore` is six operations — `insert` (the credential id is a primary key, so a second insert
of one already on record is refused rather than written over, because overwriting moves a credential
somebody else holds onto a new account), `find` (by credential id **alone**, which is what the
usernameless flow rests on), `list_for_user`, `update_counter` (which writes what it is told in both
directions, because `WebAuthn::assert` has already refused a regressed counter and a store that
clamped would be hiding a caller that skipped the verifier), `disable` (a compare-and-set, so two
requests quarantining one credential produce one alert and not two) and `delete`. Both halves ship:
`MemoryPasskeyStore` and `TablePasskeyStore`, both in `store`, the latter over `moso_auth_passkeys`, with one
`store::conformance` suite run against both so the shape a third implementation has to reproduce is
written down as a test rather than as prose. The memory store's `Debug` prints
`single_process: true`, because "why did my passkey stop working on the other instance" is otherwise
a long afternoon.

**A counter regression quarantines the credential, it does not merely refuse it.** `WebAuthn::assert`
reports the regression distinctly (`is_clone_detected`) and the mounted `login/finish` calls
`PasskeyStore::disable`, so *neither* copy of a duplicated private key works until a person has
looked; leaving the row live would make clone detection a log line an attacker retries past with a
plausible counter. The row is kept rather than deleted, for the same reason a revoked API key is.
The client is told nothing extra — the response is the same 401 a wrong credential gets — so the
channel an application alerts on is the log: `webauthn::CLONE_EVENT` (`passkey.clone_detected`) at
`ERROR`, carrying the account and the credential id.

**All four passkey routes refuse a deployment that mounted them without wiring them, and refuse it
the same way.** `login/start` needs only the relying party to produce ceremony options, so it used to
be the one route that succeeded on a deployment where nothing could finish the ceremony — worse than
an error, because it looks like it works. It now requires the store as well, before it writes
anything into the session, and all four answer **501** rather than 500: the operation is routed and
not implemented here, which a front end can branch on to hide the button. The detail naming
`AuthState::webauthn` or `AuthState::passkeys` stays server-side, because 501 is a server error.
The account store is deliberately not part of that check: it is the crate-wide dependency eight other
routes share, and a deployment missing it is not a deployment without passkeys.

## API keys

```rust
// moso-auth/src/apikey.rs — as built
pub struct ApiKey {
    pub id: Id<ApiKey>,
    pub prefix: String,
    pub hash: String,            // SHA-256 of the secret, hex. Never the secret.
    pub environment: KeyEnvironment,
    pub scopes: Vec<String>,     // permission wire names, not `Perm`
    …
}
```

- Format `mso_live_<prefix>_<secret>`; only a SHA-256 of the secret is stored. The full key is
  shown once. The prefix makes lookup a single indexed query and makes keys greppable in incident
  response.
- Scoped to permissions, expiring, revocable, with last-used tracking written asynchronously and
  rate-limited to at most once a minute per key, so an authenticated request is not a write
  transaction.
- Secret-scanning-friendly prefix so GitHub can notify on leaks.
- A revoked key is kept, not deleted, so an audit can still resolve it. Revoking a prefix that is
  not the caller's is a 404, not a 403: a 403 confirms the key exists, which is the only thing a
  prefix is worth guessing.

> **Divergence — scopes are `Vec<String>`, not `Vec<Perm>`.** `xtask/allow/dep-edges.toml` declares
> `auth -> [orm, kv]` and no edge to `authz`, so this crate cannot name `Perm`.
> `PermSet::parse_all` turns the strings back into bits on the authorization side, and the key's set
> **intersects** the owner's, so a key can never grant more than its owner has. Adding an
> `auth -> authz` edge to recover the typed form would decide what a user who wants sessions but not
> a permission system has to compile, which is exactly the trade the edge file exists to make
> explicit. **This deserves an ADR** if it is ever revisited.

`MemoryApiKeyStore` and `store::TableApiKeyStore` both implement `ApiKeyStore`, over
`moso_auth_api_keys`. The table's lookup is `where prefix = $1` on a unique index and nothing else:
the secret's hash carries no index and appears in no predicate anywhere in that file, because a
`where hash = $1` makes the database's own `memcmp` — which returns at the first differing byte —
the timing oracle that `ApiKey::verify_secret`'s constant-time comparison exists to remove. No
behavioural test can observe that difference, so the one that guards it reads the statements.
Revocation is a compare-and-set on `revoked_at is null`, so two operators revoking at once produce
one `true` and one `false`.

The `Principal` extractor resolves a presented bearer credential itself, so no tower layer is
required for `PrincipalKind::Token` or `ApiKey` to occur. Resolution reads, in order: a `Principal`
an application's own middleware left in the request extensions (which still wins — it may know a
tenant or a device this cannot); then, when an `AuthState` is registered, an
`Authorization: Bearer …` credential — an `mso_…` value against the `ApiKeyStore` through
`ApiKeyAuthenticator`, anything else as a signed access token against `AuthState::jwt`; then the
session. It is best-effort by contract — a `Principal` never refuses a request, it records what
turned up — so a credential that does not verify falls through to the session and then to anonymous,
and an endpoint that must reject an invalid token does so with `CurrentUser` or a `RequireKind`
guard. The two bearer shapes are told apart by `ApiKey::parse`, so a JWT is never fed to the
constant-time secret comparison and a key is never fed to the signature check.

> **`PrincipalKind::Service` is still application-produced.** The extractor mints `Token` and
> `ApiKey`; a `Service` principal — a service-to-service caller distinct from a user's API key —
> has no built-in credential shape, so an application that wants one inserts it from its own
> middleware, which the extension-first ordering above still honours.

## Extractors

```rust
// moso-auth/src/extract.rs — as built
pub struct CurrentUser<U: AuthUser = DefaultUser>(pub U);   // 401 if absent
pub struct MaybeUser<U: AuthUser = DefaultUser>(pub Option<U>);
pub struct AuthSession(pub Session);
pub struct Principal {                     // for audit logging
    pub kind: PrincipalKind,               // Anonymous | Session | Token | ApiKey | Service
    pub subject: Option<String>,
    pub credential: Option<String>,        // a key's prefix, a session id hash — never the secret
    pub scopes: Vec<String>,
}
```

All are `Dependency` impls, so they are memoised per request and contribute security schemes and 401
responses to the OpenAPI document automatically. `MaybeUser` and `Principal` also contribute the
*empty* requirement, so a generated client does not demand a token for an endpoint that works
without one. `AuthSession` contributes nothing: a session is not a credential requirement, because
an anonymous request has one too.

Two guards go with them: `RequireKind` (restrict an endpoint to particular credential kinds,
documenting its 403) and `Csrf` (double-submit on unsafe methods, cookie-authenticated requests
only).

```rust
// example — the response type is the documented type, so it is wrapped
#[endpoint]
async fn me(Depends(CurrentUser(u)): Depends<CurrentUser>) -> Result<Json<UserOut>> {
    Ok(Json(u.into()))
}
```

## Storage and migrations

Four stores keep a credential between two requests, and each has the same two-implementation shape:
a map, complete and per-process, and a table, the same semantics somewhere a second instance can
see. A deployment with two instances and a map-backed store is not slower, it is wrong.

| Trait | In memory | In a table | Its table |
| --- | --- | --- | --- |
| `SessionStore` | `MemorySessionStore` | `TableSessionStore` | `moso_auth_sessions` |
| `RefreshStore` | `MemoryRefreshStore` | `TableRefreshStore` | `moso_auth_refresh_tokens` |
| `ApiKeyStore` | `MemoryApiKeyStore` | `TableApiKeyStore` | `moso_auth_api_keys` |
| `PasskeyStore` | `MemoryPasskeyStore` | `TablePasskeyStore` | `moso_auth_passkeys` |

`KvSessionStore` is the fifth session store and the default. Every table statement is one string
that runs on PostgreSQL and on SQLite: timestamps are RFC 3339 text with a fixed sub-second width,
so `expires_at > $1` sorts lexicographically and needs no `timestamptz`/`datetime` divergence and no
cast, and `boolean` and `bigint` are used where a flag or a counter genuinely is one because both
backends spell them the same. `user_id`, `owner` and `subject` are indexed `text` that reference
nothing: this crate cannot know what an application's user table is called or whether its key is a
`uuid`, a `bigint` or a slug, and a foreign key it guessed wrong is a failed migration on somebody's
production database.

Non-negotiable N6 is that a migration is read before it is run, so nothing creates a table behind an
operator's back. There are two supported ways in:

1. **`moso db make-migration`.** `moso_auth::store::descriptors()` returns the four tables as
   `EntityDescriptor`s. Add it to the entity list the project's `src/db.rs` passes to
   `moso_migrate::command::make_migration` and the generator writes the migration, its reverse and
   the snapshot; from then on `moso db check` reports drift on these tables like any other.
2. **Copy the DDL.** `store::{SESSIONS_SCHEMA, REFRESH_TOKENS_SCHEMA, API_KEYS_SCHEMA,
   PASSKEYS_SCHEMA}` and the index constants beside them are the `create table` statements, for a
   project that writes its migrations by hand.

Both forms are checked against each other. `store::schema`'s own test reads every constant back and
compares it, column by column and index by index, with the descriptor beside it, because two
statements of one fact drift and the drift would be invisible until a deployment that ran
`create_table()` in development and the generated migration in production had two different schemas.
Each table store's `create_table()` runs the constants; it is for tests and for `moso dev`.

## Configuration and readiness

`AuthConfig` is the whole battery's configuration in one struct: `session`, `csrf`, `password`,
`hash_params`, `jwt`, `throttle`, `secret_keys`, `redirect_allowlist`, `allow_insecure_cookies`,
`require_verified_email`. `AuthConfig::validate` reports **every** problem in one error rather than
the first, each naming its field and the edit that fixes it, and folds in `SessionConfig::validate`
rather than restating the session rules.

`AuthHealthCheck` is the `/readyz` probe: it calls `SessionStore::probe()` under a one-second
`PROBE_TIMEOUT` — half of `READINESS_BUDGET`, so a hung store is named as the slow component while
the rest of the report still arrives — and is critical by default, because a process that cannot
read sessions cannot authenticate anybody.

`AuthConfig::from_env` is the twelve-factor loader, mirroring `KvConfig::from_env`: it reads
`AUTH_SECRET_KEYS` (comma-separated standard-base64 keys, the first signs), `AUTH_REDIRECT_ALLOWLIST`,
`AUTH_ALLOW_INSECURE_COOKIES` and `AUTH_REQUIRE_VERIFIED_EMAIL`, and — unlike the key-value loader —
runs `validate()` before it returns, so a configuration a first request would reject is a boot error
instead. A signing key is read straight into a `SecretBytes`, so it never reaches a log.

> **Still `#[derive(Config)]`-free by design.** `AuthConfig` carries no `#[derive(Config)]`: the
> derive lives in `moso-macros` and resolves against the `moso` facade *above* this crate, exactly as
> `moso-kv` documents for `KvConfig`. An application with its own configuration layer builds an
> `AuthConfig` from its `#[derive(Config)]` type — so `moso config` can see the settings — and then
> calls `validate()`; a binary with no such layer calls `from_env`, which validates for it.

## Security defaults summary (what you get without configuring anything)

| Threat | Default mitigation | Built |
| --- | --- | --- |
| Session fixation | ID cycled on login and privilege change | on login, by `Session::log_in`; any other privilege change is the application calling `cycle_id` |
| CSRF | Double-submit token on cookie-authenticated non-idempotent requests; `SameSite=Lax` | yes |
| Credential stuffing | Per-IP + per-identity rate limits, breach check, notification signal | yes; sending the mail is the application's |
| Timing oracle on user existence | Dummy verify; constant-time compare | yes, with a p95 test |
| Password DoS | Bounded blocking pool for hashing | yes |
| Token replay | Refresh-token reuse detection revokes the family | yes, over both stores, reachable at `POST /auth/refresh` |
| Enumeration via reset | Identical response and timing whether or not the account exists | yes |
| Open redirect after login | `next` parameter validated against an allowlist | yes |
| Stale sessions after password change | `auth_hash` epoch invalidates all sessions | yes |
| XSS stealing tokens | Session in an `HttpOnly` cookie; tokens not in `localStorage` by default | yes |

Every row above assumes the session layer is installed, which is the application's own
`stack.replace_custom(Slot::Session, layer)` — see [Sessions](#sessions).

## What is still owed

1. *(decided)* OpenAPI request and response bodies for the 34 mounted routes: the gap is accepted for
   the mounted (prototyping) tier by
   [ADR-0016](../adr/0016-battery-routes-documentation-and-boot-check-boundary.md); the copy-out tier
   is where the routes become documented `#[endpoint]` handlers. The bodies are still not *in the
   document* for the mounted set — the ADR records why that is the honest state, not a defect.
2. `moso new --auth`, the copy-out tier this document has always specified.
3. *(closed)* Both halves of `PasskeyStore`, `ApiKeyStore` and `RefreshStore` now ship, in memory
   and over a table, with `PASSKEYS_SCHEMA`, `API_KEYS_SCHEMA` and `REFRESH_TOKENS_SCHEMA` beside
   them and `store::descriptors` handing all three to `moso db make-migration`.
4. *(closed)* The `Principal` extractor resolves a presented bearer token or API key into a
   `Principal` itself — no tower layer needed — so `PrincipalKind::Token` and `ApiKey` occur without
   an application writing one; the extension-first ordering still lets an application's own middleware
   supply a `Service` principal or override the rest.
5. *(closed)* `routes().bearer()` mounts `POST /auth/token` and `POST /auth/refresh`, so issuance,
   rotation and reuse detection are reachable without writing an endpoint.
6. `moso auth calibrate`. *(Loading into `AuthConfig` now ships as `AuthConfig::from_env`, which
   validates before returning; a `#[derive(Config)]` on the struct itself remains out, by the same
   facade-dependency rule `moso-kv` follows.)*
7. A generic (not `DefaultUser`-fixed) mounted route set, or the copy-out tier that makes it moot.
8. `examples/crud` ported onto the battery — it still hand-rolls an `Actor` and an `ApiKeyGuard`.
9. The mounted `POST /auth/login` delegating its two-step login to `mfa::SecondFactorChallenges`
   rather than keeping a `PendingSecondFactor` in the session. The library path is the mechanism of
   record — bound, expiring and single-use, with the tests to prove all three — and the session-held
   copy is a second one that expires only with the session. The behaviour a client sees is
   identical either way, which is what makes the change a small one; two mechanisms for one concept
   is what makes it necessary.

## Acceptance criteria (WP-16, WP-17)

1. 🟡 All flows in the route table are implemented and covered by tests including failure paths.
   They are **not** fully documented in OpenAPI: tag and statuses only, bodies undocumented.
2. ✅ Session cookie attributes are asserted in every profile; `Secure` cannot be disabled in
   production without an explicit config flag, and even then `cookie_for` refuses outside
   development and logs a warning.
3. ✅ Login on a nonexistent account and a wrong password differ by < 10 ms at p95 (timing test).
4. ✅ Password hashing runs off the async runtime on a bounded pool.
5. ✅ Refresh-token reuse revokes the family and emits an audit event, against `MemoryRefreshStore`
   and against `TableRefreshStore` on SQLite and PostgreSQL, from one shared conformance suite. The
   table store's reuse detection is a compare-and-set inside a transaction, and the test that proves
   it starts two exchanges of one token concurrently and asserts exactly one rotation, exactly one
   `ReuseDetected`, and an empty family afterwards.
6. ✅ OAuth flow rejects mismatched `state`, missing PKCE verifier, and unverified-email auto-link.
7. ✅ `logout-all` invalidates existing sessions within one request.
8. ✅ A passkey registration + authentication round trip passes against a virtual authenticator —
   both at the library level and as four HTTP requests against the *mounted* routes, so the provider
   map, the session the ceremony state lives in and the store the credential lands in are what is
   under test rather than `webauthn-rs`.
