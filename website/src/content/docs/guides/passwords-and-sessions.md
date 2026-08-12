---
title: Passwords and sessions
description: Hash passwords with calibrated argon2 parameters on a bounded pool, run the login flow, and configure the signed session cookie, its rotation and its revocation.
order: 23
status: shipped
---

This is the browser half of the auth battery: a password the user types, a signed cookie that
remembers them, and every mechanism that keeps the second from outliving the first. Read
[authentication](./authentication.md) first for the principal type and the wiring; this page assumes
you have an `AuthUser` and a backend.

Everything here is implemented and tested, the [login throttle](#throttling-failed-attempts)
included. One thing you have to do yourself: nothing installs `SessionLayer` for you, though
[installing it](#installing-the-layer) is one line. `AuthConfig::from_env` now loads and validates
the configuration at boot, so you no longer have to fill the struct in by hand in the composition
root.

## Hashing a password

Passwords are argon2id in PHC string format, wrapped in `PasswordHash`, whose `Debug` prints
`PasswordHash(<redacted>)` because a `Debug` in a log line is a credential in a log aggregator.

```rust
use moso_auth::{HashParams, PasswordHash, VerifyOutcome};
use moso_schema::Password;

#[tokio::main(flavor = "current_thread")]
async fn main() -> moso_auth::Result<()> {
    let plain = Password::new("correct horse battery staple").unwrap();
    // Deliberately weak parameters: a doctest is not a deployment.
    let params = HashParams::new(8, 1, 1);
    let hash = PasswordHash::with_params(&plain, params).await?;

    assert!(hash.as_str().starts_with("$argon2id$"));
    assert!(hash.verify(&plain).await?.is_valid());
    Ok(())
}
```

Production code calls `PasswordHash::new(&plain)`, which reads the process-wide parameters.
`with_params` is for tests and migrations, and it is the one entry point that does **not** clamp to
the OWASP floor.

The input is `moso_schema::Password`, not `String`. It is length bounded before it reaches a hasher,
which is why an unbounded password field is not a way to make your server hash a megabyte.

Both hashing and verification run on `moso_core::task::blocking`, a bounded pool, never on the async
runtime. Argon2id is deliberately slow and deliberately memory-hungry: one hash per request on the
runtime turns a login flood into a full outage. On a bounded pool it becomes backpressure, and pool
saturation surfaces as `Error::Unavailable`, which is a 503, rather than an unbounded queue.

`verify` returns three states rather than a bool:

| `VerifyOutcome` | Meaning | What to do |
| --- | --- | --- |
| `Ok` | the password matches | sign them in |
| `OkNeedsRehash` | it matches, and the stored hash is weaker than the current parameters | sign them in, then rewrite the hash |
| `Invalid` | it does not match | the same failure as a missing account |

`PasswordHash::needs_rehash()` answers the same question without a verify, and `params()` reads the
parameters back out of a stored hash. `DatabaseBackend` does the upgrade for you when
`rehash_on_login` is on, which it is by default. A hash whose parameter section does not parse counts
as needing a rehash, because the safe reading of "I cannot tell how strong this is" is "not strong
enough".

Two more functions matter only if you wrote your own `AuthBackend`. `dummy_verify().await` runs a
verification that always fails, at the cost of a real one, and is what the "no such account" path
must call so a missing account and a wrong password take the same time. `password::constant_time_eq`
compares two secrets without leaking their contents through the clock. `DatabaseBackend` calls both
for you.

### Choosing parameters

A work factor chosen in 2019 is a rounding error on 2026 hardware, and the right answer differs by
an order of magnitude between a laptop and a shared container. So parameters are calibrated, not
constant.

| Item | What it is |
| --- | --- |
| `HashParams { memory_kib, iterations, parallelism }` | the three argon2 knobs |
| `HashParams::OWASP_MINIMUM` | 19 MiB, 2 passes, 1 lane. Also `Default` |
| `HashParams::at_least(other)` | conjunctive: every knob at least the other's |
| `HashParams::at_least_owasp()` | raise to the floor |
| `TARGET_HASH_TIME` | 250 ms |
| `calibrate(target).await` | search this machine for parameters near `target` |
| `password::install_params(p)` | install process-wide, raised to the floor first; returns the previous set |
| `password::current_params()` | read back what is installed |

`calibrate` raises memory first, doubling from the floor while a hash still fits the budget, then
adds passes, with a 1 GiB memory ceiling and a 12-pass ceiling. It never returns less than the OWASP
minimum, even on hardware slow enough that the minimum takes longer than the target: being slow is
not a reason to be weak.

Parameters are process-wide rather than threaded through every call site, because a set threaded
through every call site is wrong in exactly one of them, and that one writes the weak hash. Install
them once at boot.

Measuring them is `moso auth calibrate`. It runs your own binary and calls `calibrate` inside it,
because the answer is a property of the hardware the hash will run on: parameters that take 250 ms
on a laptop take three times that in a container with half a CPU, so a number the CLI carried would
be wrong on every machine but one. Run it on the machine that will serve logins.

```sh
$ moso auth calibrate
  ✓ one hash takes 243 ms here      (target 250 ms)

  PARAMETER    VALUE   NOTE
  memory_kib   65536   64 MiB, 3.4x OWASP's minimum
  iterations   3
  parallelism  1
```

It prints the configuration keys your application reads them from, and the
`HashParams::new(..)` line if you build the `AuthConfig` in code. It refuses to print anything below
`HashParams::OWASP_MINIMUM`: a calibration that recommends weaker parameters is worse than none,
because it is a plausible instruction to make an application less safe with a tool's authority
behind it. A project created with `moso new --auth` answers the command out of the box; any other
project answers it by filling in `fn auth` in its `src/dump.rs`, which is a dozen lines and is in a
comment above the stub.

> [!WARNING]
> `install_params` is process-global mutable state. A test that installs weak parameters has to
> restore them, or it silently weakens whatever test runs next in the same process.

## The password policy

`PasswordPolicy` is length plus breach plus strength. There are no composition rules, because NIST
SP 800-63B dropped them: they produce `Password1!` and nothing else.

| Field | Default | Effect |
| --- | --- | --- |
| `min_length` | 12 | shorter is refused with code `"len"` |
| `banned_words` | empty | your product name, your domain, code `"banned"` |
| `breach_check` | `true` | the embedded filter, code `"breached"` |
| `breach_api` | `false` | additionally do a k-anonymity range lookup |
| `min_strength` | 2 | the zxcvbn-style score floor, code `"weak"` |

Checks run in that order, and the first failure is an `Error::PasswordPolicy { code, detail }`. The
codes are stable and machine-readable, so a client can localise them. The lifecycle flows add one
more, `"reused"`, when `refuse_password_reuse` catches a password change back to the current one.

```rust
use moso_auth::PasswordPolicy;
use moso_schema::Password;

#[tokio::main(flavor = "current_thread")]
async fn main() -> moso_auth::Result<()> {
    let policy = PasswordPolicy::default();

    let breached = Password::new("password1234").unwrap();
    assert!(policy.check(&breached, &[]).await.is_err());

    let fine = Password::new("wharf-lentil-oxide-77").unwrap();
    policy.check(&fine, &["ada@example.com"]).await?;
    Ok(())
}
```

The second argument is account context. Pass the address and the display name: `Strength::estimate`
subtracts them, so `adaexample123` scores zero for `ada@example.com`. `Strength` also carries
`feedback()` and `suggestion()` strings that are safe to show a user.

### What the breach check actually checks

This is the one place the implementation deliberately departs from the design, and it says so in a
public constant, `password::EMBEDDED_CORPUS_NOTE`. Shipping somebody else's breach corpus in the
source tree is a licensing question and a multi-megabyte blob in every build. What is embedded is a
seed list of 346 words plus the suffix, capitalisation and leet-substitution rules that dominate
every published corpus (four base forms times twenty-four suffixes and sixty-six years, so about
125,000 entries), expanded on first use into a Bloom filter. Building it costs about 15 ms and
256 KB, lazily, on first use.

Two consequences. The filter has about a 1% false-positive rate, so a legitimate password is
occasionally rejected as breached. And the corpus is smaller than a real breach list, so add your own
with `BreachCheck::with_extra_list` (process-wide) and cover the long tail with the k-anonymity API.

There is no HTTP client here for that API, on purpose: adding one would put TLS, a connection pool
and a DNS resolver into every application that only wanted a login form. You supply a `RangeFetcher`.
Configuring `BreachCheck::api(endpoint)` without a `fetcher` is a configuration *error*, not a silent
skip, because a breach check that quietly does nothing is worse than none. A network failure at
request time fails *open* with a warning: the embedded filter has already run, and a slow third party
must not stop signups.

## The login flow

Four steps, in this order.

```rust title="src/routes/login.rs"
use std::time::Duration;

use moso::extract::ClientIp;
use moso::prelude::*;
use moso::schema::Password;
use moso_auth::{
    AuthBackend, AuthCtx, AuthSession, DatabaseBackend, LoginThrottle, PasswordCredentials,
    ThrottleDecision,
};

/// What a login posts. Your own type, so its fields are yours to choose. The
/// crate's own `LoginRequest` does implement `Schema`, so `Json<LoginRequest>`
/// compiles, but it is `#[non_exhaustive]` and carries the two-step fields.
#[derive(Schema)]
pub struct LoginBody {
    /// The address or username.
    pub identity: String,
    /// The password.
    pub password: Password,
}

/// Exchange an identity and a password for a session cookie.
#[endpoint]
async fn login(
    address: Option<ClientIp>,
    Depends(AuthSession(session)): Depends<AuthSession>,
    Inject(throttle): Inject<LoginThrottle>,
    Inject(backend): Inject<DatabaseBackend<Account>>,
    Json(body): Json<LoginBody>,
) -> Result<NoContent> {
    let mut ctx = AuthCtx::new().with_identity(body.identity.as_str());
    if let Some(ClientIp(address)) = address {
        ctx = ctx.with_ip(address.to_string());
    }

    // 1. Before the hash, because the hash is what an attacker wants you to pay for.
    match throttle.check(&ctx).await? {
        ThrottleDecision::Allow => {}
        ThrottleDecision::Deny { retry_after } => return Err(Error::too_many(retry_after)),
        // Nothing here verifies a CAPTCHA, so the safe reading of a challenge
        // is a refusal. `ThrottleDecision` is `#[non_exhaustive]`, so the
        // wildcard is required outside the crate, and refusing is the right
        // default for a tier this handler has not been taught.
        _ => return Err(Error::too_many(Duration::from_secs(2))),
    }

    // 2. Wrong password, no such account and inactive account: one answer.
    let credentials = PasswordCredentials::new(body.identity, body.password);
    let found = backend.authenticate(credentials, &ctx).await?;

    // 3. `log_in` loads, cycles the id, records the subject and the auth hash,
    //    and restarts the absolute window, in one step.
    if let Some(user) = &found {
        session.log_in(user).await?;
    }

    // 4. Record it either way, and never fail the request for it, because the
    //    attempt has already happened by now.
    let _ = throttle.record(&ctx, found.is_some()).await;

    found.map(|_| NoContent).ok_or_else(Error::unauthenticated)
}
```

1. **Check the throttle** before doing any work. See [below](#throttling-failed-attempts).
2. **Authenticate.** `backend.authenticate` returns `Ok(None)` for every kind of miss, having spent
   the same time on each. Do not turn that into distinct responses. "No such account" and "wrong
   password" as separate messages is a user-enumeration oracle, and enumeration is step one of every
   credential-stuffing campaign.
3. **Bind the session** with `Session::log_in`, which is also the fixation defence.
4. **Record the attempt**, successful or not, for the notification and the backoff.

The `?`s on `moso_auth` results are the ordinary kind: `From<moso_auth::Error> for
moso_core::Error` collapses the enumeration-sensitive variants first and then maps onto the HTTP
problem. It is 401 with `WWW-Authenticate` for every credential failure, 429 with `Retry-After` for
`RateLimited`, 422 with a `/password` pointer for `PasswordPolicy`, 503 for `Unavailable`. You do
not convert at the boundary yourself, and there is nothing left on this path that panics.

`PasswordCredentials` also carries an optional TOTP code (`with_totp`) and the challenge that goes
with it (`with_challenge`), and `DatabaseBackend` reads both once you call `second_factor(secret,
last_period, pending)`. A correct password on an enrolled account then answers
`Error::SecondFactorRequired`, which maps to a 401 carrying a `challenge` extension member, and the
second request presents the same identity and password plus the code and that challenge:

```rust title="src/routes/login.rs"
match backend.authenticate(credentials, &ctx).await {
    Ok(Some(user)) => { /* signed in */ }
    Ok(None) => { /* the same 401 a wrong password gets */ }
    Err(moso_auth::Error::SecondFactorRequired { challenge }) => { /* 401 + challenge */ }
    Err(other) => return Err(other.into()),
}
```

The challenge is a `moso_auth::mfa::SecondFactorChallenge`, minted and claimed by a
`SecondFactorChallenges` store over `moso-kv`. Three properties make it safe and all three are the
store's job rather than your handler's: it is bound to one account (redemption hands the subject
back so you compare it against the account you just verified), it expires after
`SECOND_FACTOR_TTL`, and it is claimed exactly once, because the claim *is* the store's delete and
the delete answers whether this caller removed it.

## Sessions

### Choosing a store

| Store | Backing | Notes |
| --- | --- | --- |
| `KvSessionStore::new(kv)` | any `moso_kv::Kv`: memory, Redis, PostgreSQL | the usual choice |
| `TableSessionStore::new(db)` | a real SQL table, `moso_auth_sessions` | `create_table()`, `sweep()`, `len()` |
| `MemorySessionStore` | an instrumented in-process map | tests: `round_trips()`, `writes()`, `set_load_delay()` |

`TableSessionStore` ships the DDL as constants (`SESSIONS_SCHEMA`, `SESSIONS_USER_INDEX`,
`SESSIONS_EXPIRY_INDEX`) so it can be created from a [migration](./migrations.md) rather than at
boot. Nothing sweeps expired rows for you; `sweep()` is a method waiting for a
[scheduled job](./scheduled-jobs.md).

Whatever you pick, the store fails loudly. Its `moso-kv` namespaces are declared `on_failure = fail`,
so an unreachable store is a 503 and never a silent logout, which would be worse and much harder to
diagnose. This is the one place in the cache layer where degrading is the wrong answer.

A custom store implements `SessionStore`: `load`, `save`, `delete`, `rename`, `list_for_user`,
`delete_for_user` and an optional `probe`. `rename` has to be atomic, because it is what makes id
cycling safe.

### Reading and writing session data

```rust
use moso_auth::{Session, SessionConfig};
use moso_auth::store::MemorySessionStore;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> moso_auth::Result<()> {
    let store = Arc::new(MemorySessionStore::new());
    let session = Session::detached(store, SessionConfig::default());

    session.load().await?;
    session.insert("locale", "it-IT")?;
    assert_eq!(session.get::<String>("locale")?.as_deref(), Some("it-IT"));
    Ok(())
}
```

`take` reads and removes in one call, which is what a flash message wants. In a handler, get the
session by taking `Depends<AuthSession>` rather than building a detached one.

Loading is lazy and that is a design commitment, not an optimisation. The layer builds the handle
from the cookie and touches nothing; the first round trip happens when something calls `load`. An
endpoint that names no auth extractor costs zero.

Three sharp edges follow from that:

- `get`, `insert`, `remove` and `take` return `Error::Config` with a message naming the fix if you
  never loaded. They are synchronous, so they cannot load for you.
- `user_id()` and `auth_hash()` return `None` on an unloaded session, which looks like an answer.
- `get` on a value whose shape changed across a deploy is an `Error::Config` naming the key and the
  type, not a `None`. Keep session values small and stable, or version the key.

### The cookie

| `CookieConfig` field | Default | Notes |
| --- | --- | --- |
| `name` | `"id"` | the visible name is `full_name()` |
| `path` | `"/"` | anything else disables the `__Host-` prefix |
| `domain` | `None` | setting it disables the prefix |
| `http_only` | `true` | |
| `secure` | `true` | disabling it disables the prefix |
| `same_site` | `Lax` | `Lax`, `Strict` or `None` |
| `host_prefix` | `true` | `__Host-` when secure, path `/`, no domain |

So `CookieConfig::default().full_name()` is `__Host-id`. The prefix is the browser-enforced version
of "this cookie belongs to exactly this origin", and it is why the defaults refuse a domain.

| `SessionConfig` field | Default | Notes |
| --- | --- | --- |
| `idle_timeout` | 14 days | rolling |
| `absolute_timeout` | 90 days | restarted by `log_in`, not by activity |
| `touch_interval` | 60 s | a read becomes a write at most this often |
| `track_devices` | `true` | set false to stop recording user agent and IP |

`SessionConfig::validate()` checks the combination, and `AuthConfig::validate()` folds its verdict
into the boot report rather than restating the rules. The value is signed with HMAC-SHA256 against a
key set, and every installed key is tried in constant time, so rotating a key does not log anybody
out:

```rust
use std::sync::Arc;
use moso_auth::{SessionConfig, SessionId, SessionLayer, SessionStore};
use moso_auth::store::MemorySessionStore;
use moso_core::config::SecretBytes;

let store = || Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>;
let old = SecretBytes::new(vec![1; 32]);
let new = SecretBytes::new(vec![2; 32]);

let before = SessionLayer::new(store(), SessionConfig::default()).keys(vec![old.clone()]);
let after = SessionLayer::new(store(), SessionConfig::default()).keys(vec![new, old]);

let id = SessionId::generate();
assert_eq!(after.verify(&before.sign(&id)).as_ref(), Some(&id));
```

The first key signs, the rest only verify. To rotate: deploy with the new key first and the old one
second, wait longer than `idle_timeout`, then drop the old one. `SessionLayer::validate()` refuses an
empty key set and any key under 32 bytes. Session ids are 256 bits of CSPRNG output, not UUIDs,
because a session id is a bearer token and 122 bits is thin margin for something that can live 90
days.

On the way out, the layer writes the record if it is dirty or if `last_seen_at` is older than
`touch_interval`, and returns a `Set-Cookie`. An untouched session emits no header at all. A
destroyed session emits a clearing cookie with `Max-Age=0`. If the write fails, the whole response
is replaced with a 503 rather than losing the session quietly.

### Installing the layer

Per request the layer does two things. `begin(request.headers())` reads the cookie named by
`CookieConfig::full_name()`, verifies its signature, and hands back a `Session` handle that goes into
the request extensions, touching no store. `finish(&session)` then saves if there is anything to save
and returns the `Set-Cookie` value, or `None`. `SessionService`, the service `SessionLayer` wraps a
route in, does both, and turns a failed write into a 503 rather than losing the session quietly.

`Slot::Session` is a position in the [middleware stack](./middleware.md) with no built-in, waiting
for exactly this. `SessionLayer` implements `moso_core::middleware::CustomLayer` rather than
`tower::Layer<Route>`, so it goes in through `replace_custom`:

```rust title="src/lib.rs"
use std::sync::Arc;

use moso::middleware::Slot;
use moso::prelude::*;
use moso_auth::{KvSessionStore, SessionLayer, SessionStore};

let store: Arc<dyn SessionStore> = Arc::new(KvSessionStore::new(kv));
let layer = SessionLayer::new(store, auth.session.clone()).keys(auth.secret_keys.clone());
layer.validate()?;

App::new(config)
    .with_middleware(|stack| {
        stack.replace_custom(Slot::Session, layer);
    })
    .mount(routes())
```

The slot keeps its own name, so `moso middleware` still prints `session` with the layer's summary
beside it (the cookie name, the two timeouts and how many signing keys are installed) rather than
a type name. `stack.validate()` stops reporting the slot as empty the moment it is filled.

> [!NOTE]
> `replace_custom`, `insert_before_custom`, `insert_after_custom` and `append_custom` are the
> `CustomLayer` siblings of `replace`, `insert_before`, `insert_after` and `append`, which take a
> `tower::Layer<Route>`. They cannot be one method each: widening the `tower` version to accept
> either needs two blanket impls that overlap, and the compiler rejects the pair (E0119). A
> `#[middleware]` function generates a real `tower::Layer`, so that half goes in through the
> unsuffixed installers.

### CSRF

Cookies are sent by the browser whether or not your page asked, so a cookie-authenticated unsafe
request needs a second proof. `Csrf` is a `Guard`, so it documents its header parameter and its 403
in the OpenAPI document.

```rust
use moso_auth::store::MemorySessionStore;
use moso_auth::{Csrf, CsrfConfig, Session, SessionConfig};

#[tokio::main(flavor = "current_thread")]
async fn main() -> moso_auth::Result<()> {
    let session = Session::detached(MemorySessionStore::shared(), SessionConfig::default());
    session.load().await?;

    let csrf = Csrf::new(CsrfConfig::default());
    let token = csrf.token(&session)?;
    assert_eq!(csrf.token(&session)?, token, "minted once, then stable");
    Ok(())
}
```

`CsrfConfig` defaults: header `x-csrf-token`, query field `csrf_token`, session key `_csrf`,
`check_origin` true. `Csrf::applies` is true only for an unsafe method on a request that carries a
`Cookie` header and **no** `Authorization` header, so a client presenting a bearer token or an API
key in `Authorization` is exempt automatically. A key presented in `x-api-key` is not: that header
is invisible to `applies`, so a request carrying both a cookie and an `x-api-key` is still checked.
Two things to know:
the form field is read only from the query string, because reading it from the body would mean
buffering the body inside a guard that runs before the handler has said how large a body it accepts.
And with `check_origin` on, a request carrying neither `Origin` nor `Referer` is refused, since every
browser sends one on a cross-origin state-changing request. A non-browser client that posts with a
cookie and no `Origin` will be rejected.

## Session fixation and rotation

An attacker who can plant a cookie in a victim's browser before login owns the session after it,
unless the id changes at the moment the privilege does. `Session::cycle_id` issues a new id and
keeps the contents, through an atomic `rename` in the store.

```rust
session.load().await?;
let before = session.id();
session.cycle_id().await?;
assert_ne!(session.id(), before);
```

`Session::log_in` calls it for you and also restarts the absolute window, so the 90 days run from
the login rather than from whenever the anonymous session that held the CSRF token was created. Call
`cycle_id` yourself on any other privilege change: elevating to an admin mode, completing a second
factor, accepting an invitation that changes what the session can reach.

Do not confuse the two rotations. Cycling the **id** is per-session and defends against fixation.
Rotating the signing **key** is deployment-wide and defends against a leaked key, and because every
key verifies, it logs nobody out.

## Logging out

| Goal | Call | Effect |
| --- | --- | --- |
| This session | `Session::destroy` or `Accounts::log_out` | deletes the record, clears the cookie |
| Everywhere | `Accounts::log_out_everywhere(id, keep)` | bumps the epoch, then deletes the rows |
| Everywhere but here | the same, with `keep: Some(&session_id)` | the current session survives |
| List devices | `Accounts::sessions_of(id)` | `Vec<SessionRecord>` with `DeviceInfo` |

"Log out everywhere" does two things because they cover different failures. Bumping the epoch
through `AccountStore::bump_epoch` changes what `AuthUser::auth_hash` returns, so every session is
invalid at its *next* request, including sessions in a store this process cannot reach, with no scan
and nothing to forget. The eager delete then empties the "your devices" list immediately, which is
what the user expects to see. Neither alone is enough.

`DeviceInfo::from_request(user_agent, ip)` derives a label like `"Firefox on macOS"` or `"curl"`,
truncating the user agent at 256 characters. Set `track_devices: false` to stop recording it. When
you surface the list, use the `SessionSummary` DTO: its `handle` is deliberately not the session id,
because a page that lists live session ids is a page that leaks bearer tokens.

The lifecycle flows revoke on their own schedule: `reset_password` bumps the epoch and deletes every
session **including the one that asked**, because a reset is what you do when you think somebody
else has your account. `change_password` keeps the current session and revokes the rest, returning
how many it killed.

## Throttling failed attempts

`LoginThrottle` is per-address *and* per-identity, with backoff rather than a lock. A hard lockout
is itself an attack: anybody who knows your address can lock you out with five bad logins.
Exponential backoff slows an attacker to nothing and leaves the victim able to sign in after a wait.
The state lives in `moso-kv` rather than in the process, because a per-process limiter multiplies
the real limit by the pod count, which is how a rate limit quietly stops being one.

| `ThrottleConfig` field | Default |
| --- | --- |
| `per_ip_burst` / `per_ip_period` | 10 at once, then one more every 60 s |
| `per_identity_free` | 3 consecutive failures before backoff starts |
| `per_identity_base` / `per_identity_max` | 2 s, doubling, capped at 600 s |
| `notify_after` / `notify_window` | 5 failures in 900 s makes `should_notify` return true, once |
| `challenge_after` | `ThrottleConfig::CHALLENGE_OFF`: no challenge tier until you turn it on |

Build one over any `moso_kv::Kv` and register it, so `Inject<LoginThrottle>` resolves in the
handler:

```rust title="src/lib.rs"
use moso_auth::{LoginThrottle, ThrottleConfig};

App::new(config)
    .provide(LoginThrottle::new(kv.clone(), ThrottleConfig::default()))
```

### What `check` decides

Three tiers, in this order, and all of it before any credential work. Hashing is the expensive
operation an attacker is trying to make the server do, so a refused attempt must not pay for one.

1. **The address quota**, charged first and unconditionally, so an attacker spraying one address
   across a thousand identities runs out whichever identities they name. It is `moso-kv`'s own GCRA
   limiter rather than a second implementation of the same algorithm: `per_ip_burst` attempts at
   once, then one more every `per_ip_period`. A spent quota is `Deny { retry_after }`.
2. **The per-identity backoff.** Past `per_identity_free` consecutive failures the wait is
   `per_identity_base · 2^(failures − free − 1)`, saturating at `per_identity_max`, and an attempt
   inside it is a `Deny` carrying whatever is left of it. Every step is checked arithmetic, so a
   failure count an attacker chooses cannot overflow the shift.
3. **The challenge tier**, which is what `challenge_after` consecutive failures buys: not yet worth
   refusing, and a challenge costs an attacker far more than it costs somebody who mistyped. It is
   off by default (`ThrottleConfig::CHALLENGE_OFF`), because a challenge nobody can answer is a
   lockout rather than a challenge. Turn it on in the same edit that registers a verifier.

An `AuthCtx` carrying no identity has no per-identity state and is covered by the address quota
alone. An `AuthCtx` carrying no address falls into one shared `unknown` bucket rather than escaping
the quota, because an absent address is the state an attacker arranges: a stripped
`X-Forwarded-For`, a proxy the deployment does not trust.

`record(&ctx, succeeded)` is the other half. A success clears the identity's consecutive-failure
counter and its timestamp; a failure advances the counter through the backend's own `INCR` (a
read-modify-write here would let two racing attempts cost an attacker only one failure) and stamps
the time of it. The counters expire the larger of `per_identity_max` and `notify_window` after their
last write, so an identity that stops being attacked stops being throttled. Both outcomes are
appended to a bounded attempt list (`ATTEMPT_HISTORY` is 20 entries, kept for `ATTEMPT_RETENTION`,
30 days), which is what `recent(identity, limit)` reads back, newest first, for an account's
security page. A success clears the *consecutive* counter and not the windowed one: guessing right
on the sixth try is still five failures, and that is the case most worth an email.

Six keys hold all of it, under the `moso:v1:<app>:` prefix `moso-kv` gives every key: the GCRA
bucket, `throttle_failures`, `throttle_last_failure`, `throttle_window`, `throttle_notified` and
`throttle_attempts`. **An identity is never a key.** Every per-identity key is the lowercase hex
SHA-256 of the normalised identity and the address bucket is the digest of the address, because a
key leaks in ways a value does not: a `SCAN` over a shared Redis, a slow-log entry, a backend error
that quotes the key it failed on. Hashing the address is a key-*shape* decision and not
anonymisation (a 32-bit address space is exhaustible in seconds). What it buys is a segment of
fixed length, so a hostile or very long `AuthCtx::ip` cannot push a key past `moso-kv`'s length
limit and turn a throttle into an error.

> [!IMPORTANT]
> Every namespace here declares `on_failure = fail`, so an unreachable store is
> `Error::Unavailable` (a 503) and never a decision. Treat an `Err` from `check` as a refusal.
> A limiter that stops limiting when its store blinks is a limiter an attacker can remove by making
> the store blink.

`LoginThrottle` is not a `Guard`, and cannot be: `Guard::check` receives `&http::request::Parts`,
and the per-identity tier keys on a field in the body. That is why it runs inside the handler, and
why the address tier alone is also available as `moso-kv`'s `RateLimit` guard when a route has no
identity to key on:

```rust title="src/routes/mod.rs"
use std::time::Duration;

use moso::{Router, ep};
use moso_kv::{RateKey, RateLimit, RateQuota};

pub fn routes() -> Router {
    Router::new()
        .post("/auth/magic-link", ep!(request_magic_link))
        .guard(
            RateLimit::new(RateQuota::new(10, Duration::from_secs(60)))
                .key(RateKey::Ip)
                .scope("magic-link"),
        )
}
```

Naming the scope matters: a login limited to 10 a minute and a search limited to 100 must not
consume each other's buckets, and `LoginThrottle`'s own bucket is scoped `auth-login-ip` for the
same reason. The full limiter surface is in [rate limiting and locks](./rate-limiting.md).

### The notification, and who sends it

`should_notify(identity)` answers "have there been `notify_after` failures inside `notify_window`,
and has nobody been told yet". It returns true **once** per window: the marker is claimed with a
set-if-absent, one atomic operation on every backend, so a sustained attack sends one email rather
than one per attempt, which would itself be a way to use your application as a mail bomb.

`notice(identity)` is the call to make, and it is the one the mounted login path makes. It claims
that same once-per-window marker and hands back a `SecurityNotice` carrying the failure count, the
window and the recent attempts, so the marker and the evidence that goes in the mail come from the
same moment. `should_notify` is still there for a caller that wants only the signal.

**Nothing in `moso-auth` sends the mail.** The crate does not depend on `moso-mail`, deliberately,
because which provider you use, what the template says and whether the send goes through a job
queue are yours. What it gives you instead is a `NoticeSink`: the same
`Arc<dyn Fn(..) -> BoxFuture<'static, ()>>` shape as the `TokenSink` that carries verification and
reset tokens, registered with `AuthState::notice_sink`. With none registered the notice is dropped
with a warning, because an alert that silently sends nothing is worse than one that says so.

```rust title="src/lib.rs"
let notices: NoticeSink = Arc::new(|notice| {
    Box::pin(async move {
        SuspiciousActivityJob::enqueue(&jobs, notice.destination().to_owned()).await.ok();
    })
});
```

A `SecurityNotice` carries no token and has no `expose()`. That is structural, not an oversight: an
alert sink can never be handed a live credential, which is the same separation `DeliveryPurpose`
keeps from `TokenPurpose`. Send it through a [job](./jobs.md) rather than inline, for the same
reason a token delivery goes through one: a mail provider that is slow or down must not make a
login slow or down.

Driving it yourself, from your own login handler, is two lines after `record` (not before, or the
failure you just recorded is not yet in the count):

```rust title="src/routes/login.rs"
if let Some(notice) = throttle.notice(&identity).await? {
    SuspiciousActivityJob::enqueue(&jobs, notice.destination().to_owned()).await?;
}
```

### The CAPTCHA hook

`ThrottleDecision::Challenge` is resolved by whoever calls `check`. The mounted routes resolve it in
one place, `routes::support::gate`, against a `CaptchaVerifier` registered with
`AuthState::captcha`, and the resolution is deliberately strict: **with no verifier configured, or
with one that says the token did not check out, a challenge becomes a refusal.** Treating "we cannot
check" as "let them through" would make the challenge tier a way to *skip* the throttle rather than
a way to slow it down.

The token arrives in the `x-captcha-response` header, which the mounted routes read through a
declared `Headers<..>` field so it appears in the OpenAPI document as a parameter instead of being
folklore. `CaptchaVerifier` is dyn-compatible and has two methods, `provider()` for the log and
`verify(token, ip)`; a failed verification is `Ok(false)` rather than an error, so a `?` cannot turn
a failed CAPTCHA into a 500 that some middleware retries.

One implementation ships, `moso_auth::captcha::HttpCaptchaVerifier`, and it covers Cloudflare
Turnstile, hCaptcha and reCAPTCHA, because all three verify the same way: a form `POST` carrying
`secret` and `response`, and a JSON `success` field to read. It goes out over the hyper and rustls
transport this crate already uses for OAuth, with that transport's deadline, body cap and redirect
cap, rather than pulling in a second HTTP client.

```rust title="src/lib.rs"
use moso_auth::captcha::{CaptchaProvider, HttpCaptchaVerifier};

let verifier = HttpCaptchaVerifier::new(CaptchaProvider::turnstile(), config.turnstile_secret)?;

let mut throttle_config = ThrottleConfig::default();
throttle_config.challenge_after = 3;   // the tier is off until you say this

let state = AuthState::new(sessions)
    .throttle(LoginThrottle::new(kv, throttle_config))
    .captcha(verifier.shared());
```

It has three answers and none of them collapse into another. A provider it cannot reach is
`Error::Unavailable` (a 503), never `Ok(false)`, which would lock every throttled user out while a
third party is down, and never `Ok(true)`, which would make "break the provider" the way past the
tier. A provider that rejects your *secret* is `Error::Config`, not a failed token, because
reporting a typo in an environment variable as a refused CAPTCHA produces a permanent lockout that
looks exactly like an attack in progress. Only a token the provider actually judged and rejected is
`Ok(false)`.

## Failure modes

| Symptom | Cause | Fix |
| --- | --- | --- |
| A 429 on a login nobody was attacking | the address quota is shared by everyone behind one NAT or one untrusted proxy, and an unresolved address shares the `unknown` bucket | configure `http.trusted_proxies` so `ClientIp` resolves; raise `per_ip_burst` |
| A 429 that only time clears | `challenge_after` was turned on without registering a verifier, so the decision is `Challenge` and a challenge with nothing to answer it is a refusal | register `HttpCaptchaVerifier` with `AuthState::captcha`, or set `challenge_after` back to `ThrottleConfig::CHALLENGE_OFF` |
| Every login 503s | the throttle's key-value store is unreachable, and it fails closed on purpose | fix the store; do not make the limiter fail open |
| `Error::Config` from `session.get` | nothing called `load()`, or the stored value's shape changed | take `Depends<AuthSession>`; version the key |
| Everyone logged out after a deploy | `AuthUser::Id` changed type, or the signing key set lost its old key | keep the old key in the set for longer than `idle_timeout` |
| 503 on every authenticated request | the session store is unreachable, and it fails loudly on purpose | fix the store; do not make it degrade |
| A 500 naming `provide_dyn` | no `dyn UserStore<U>` registered | register it in the composition root |
| A good password rejected as breached | the Bloom filter's roughly 1% false-positive rate | tell the user to pick another, or lower `breach_check` for that flow |
| Logins slow to a crawl under load | the blocking pool is saturated by argon2 | that is backpressure working; lower the parameters or add capacity |
| The cookie is named `id`, not `__Host-id` | `secure` is false, `path` is not `/`, or a `domain` is set | `host_prefix_applies()` names the three conditions; `full_name()` shows the result |
| No `Set-Cookie` on a login response | the middleware never ran `finish`, or the session was never dirtied | install the layer as shown above; `Session::log_in` always dirties it |
| A non-browser client gets a 403 on a POST | `check_origin` refuses a request with neither `Origin` nor `Referer` | give it an `Authorization` header, which `Csrf::applies` exempts |

## See also

- [Authentication](./authentication.md) for the principal, the backends and the lifecycle flows.
- [JWT and API keys](./jwt-and-api-keys.md) for credentials that are not cookies.
- [Rate limiting and locks](./rate-limiting.md) for the GCRA limiter the throttle's address tier is.
- [Security](./security.md) for the headers and defaults that surround all of this.
