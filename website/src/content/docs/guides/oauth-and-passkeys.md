---
title: OAuth and passkeys
description: Configure an OAuth2 or OIDC provider, run the callback flow safely, decide when an external identity may attach to an existing account, and register and authenticate passkeys.
order: 25
status: shipped
---

Two ways to sign in without a password, and both are ceremonies: a sequence of requests where the
security lives in what you check between the steps rather than in any single call. `moso-auth` owns
those checks. PKCE is always on and always S256, `state` is bound to the session and compared in
constant time, a WebAuthn challenge has a server-side lifetime and a ceremony tag, and linking an
external identity to an existing account is refused by default when the provider will not vouch for
the address.

You can mount both flows or write them. `moso_auth::routes().oauth([..])` mounts two routes and
`.passkeys()` mounts four; underneath, the `Provider` and `WebAuthn` types are complete and tested,
and an application that needs its own `User` type or an extra step calls them from two handlers per
flow. This page is both: what the mounted routes actually do, and the shape of the handlers you would
write instead.

> [!IMPORTANT]
> One shape to know up front: the mounted routes are **fixed to `DefaultUser`** with
> request and response **bodies** stamped `x-moso-undocumented`, because they are registered through
> `Router::get`/`post` rather than `#[endpoint]`, which is the honest marking for a hand-mounted route.
>
> What is no longer a gap: both halves of `PasskeyStore` ship, so `routes().passkeys()` serves all
> four routes with nothing more than `AuthState::passkeys(store::MemoryPasskeyStore::shared())`, and
> `store::TablePasskeyStore` is the swap for a second instance.
> `AuthRoutes` mounts real routes, `moso_auth::routes::validate_next` is
> implemented, and `?` on a `moso_auth::Result` inside a `moso_core::Result` handler works.
> `From<moso_auth::Error> for moso_core::Error` collapses `Error::Ceremony` onto the same 401 an
> `Error::InvalidCredentials` gets, which is exactly the behaviour these flows want: the reason
> survives in the log and never in the response. The full mapping is in
> [JWT and API keys](./jwt-and-api-keys.md#errors-at-the-boundary).

## Adding the crate

`moso-auth` is reached through the `moso` facade, behind an `auth` feature that is **off by default**
and which implies `orm`.

```toml title="Cargo.toml"
[dependencies]
moso = { path = "/absolute/path/to/moso/crates/moso", features = ["auth"] }
```

The battery is then `moso::auth`. Depending on `moso-auth` directly also works, and is what the
examples below name because it is shorter:

```toml title="Cargo.toml"
[dependencies]
moso = { path = "/absolute/path/to/moso/crates/moso" }
moso-auth = { path = "/absolute/path/to/moso/crates/moso-auth" }
```

`moso-auth` gates passkeys behind an off-by-default `passkeys` Cargo feature, which the mounted
`.passkeys()` routes and the `WebAuthn` type on this page need. Turning it on pulls `webauthn-rs`
and, through `webauthn-rs-core`, OpenSSL, so that build needs a C toolchain and libssl headers
(ADR-0015); leave the feature off and neither is compiled. See
[installation](../start/installation.md).

## The mounted routes

Nothing is mounted until a flag asks for it. Two flags matter here, and they mount six routes between
them.

| Flag | Route | What it does |
| --- | --- | --- |
| `.oauth([providers])` | `GET /auth/oauth/{provider}` | mints the authorization request, stores it in the session, 303 to the provider |
| | `GET /auth/oauth/{provider}/callback` | redeems the code, links or creates the account, signs in, 303 to `next` |
| `.passkeys()` | `POST /auth/passkeys/register/start` | options for a signed-in user to enrol a key |
| | `POST /auth/passkeys/register/finish` | store the credential the browser made |
| | `POST /auth/passkeys/login/start` | options for the discoverable flow, with no identity asked for |
| | `POST /auth/passkeys/login/finish` | verify the assertion, load the account, sign in (204) |

`{provider}` is one path parameter, not one route per vendor: the name is matched against
`Provider::id().as_str()` for each provider you passed to `.oauth([..])`, and a name nobody configured
is a **404**. Anything else would enumerate which providers a deployment has.

> [!NOTE]
> `moso new --auth` scaffolds these handlers into your project's own `src/auth.rs`, with an
> end-to-end `tests/auth.rs` beside them, so you get documented `#[endpoint]` handlers over a user
> type in your crate rather than the battery's undocumented mount. `moso auth calibrate` measures
> argon2id parameters inside your own binary and refuses to print anything below the OWASP minimum.

```rust
use moso_auth::{AuthState, MemorySessionStore, Provider, WebAuthn, routes};

fn auth_router(google: Provider) -> moso::Router {
    routes()
        .oauth([google])
        .passkeys()
        .redirect_allowlist(["https://app.example.com"])
        .build()
}

fn auth_state(relying_party: WebAuthn) -> AuthState {
    AuthState::new(MemorySessionStore::shared()).webauthn(relying_party)
    // .accounts(store, tokens) and .passkeys(store) are what make these routes work.
}
```

### What each route needs on `AuthState`

A handler that needs a dependency takes `Inject<AuthState>`, so `AuthState` is one `provide` in the
composition root. A dependency that was never configured is an `Error::Config` naming the builder call
that fixes it, which renders as a 500, not a boot failure, because these handlers are registered
without a provider requirement.

| Route | Needs on `AuthState` |
| --- | --- |
| `GET /auth/oauth/{provider}` | nothing. It does not take `Inject<AuthState>` at all: the provider list and the redirect allowlist belong to the *router*, and the handler captures them |
| `GET /auth/oauth/{provider}/callback` | `accounts(store, tokens)`, to find or create the account and sign it in |
| `POST /auth/passkeys/register/{start,finish}` | `webauthn(..)` and `passkeys(..)` |
| `POST /auth/passkeys/login/start` | `webauthn(..)` and `passkeys(..)` |
| `POST /auth/passkeys/login/finish` | `webauthn(..)`, `passkeys(..)` and `accounts(..)` |

The passkey routes are the exception to the 500 above: a missing `webauthn(..)` or `passkeys(..)`
is a **501**, on all four of them. `login/start` needs only the relying party to produce ceremony
options, so it would otherwise be the one route that succeeded on a deployment where nothing could
finish the ceremony: a button that spins, an authenticator prompt the user answers, and then a 500.
It requires the store as well, before it writes anything into the session, and 501 is the status
that names the condition (the operation is routed and not implemented here) so a front end can
branch on it and hide the button. The sentence naming the builder call stays in the log, because 501
is a server error and server errors do not carry their detail to the client. A missing
`accounts(..)` stays a 500: that is the crate-wide dependency eight other routes share, and a
deployment without it is not a deployment without passkeys.

Every one of them also needs a session, which is what the callout below is about.

### What the OAuth callback does with a new identity

The join key is `OAuthProfile::verified_email()` when the provider gave one, and
`identity_key()` (`"<provider>:<subject>"`) otherwise. Never the unverified address. If the
identity is unknown, the handler **creates an account**: `Provider::check_link` runs first, then a
`NewAccount` is written carrying the raw profile, with a password hash of a 32-byte random value
nobody holds. That is not a placeholder to tidy up later; it is what makes every password login
against a social-only account fail verification rather than fall through a `None`.

If you do not want signup by social login, this is the behaviour you replace by copying the handler.

> [!IMPORTANT]
> All six routes read a `Session` out of the request extensions, and only `SessionLayer` puts one
> there. Nothing installs it for you: without `stack.replace_custom(Slot::Session, layer)` in your
> composition root, these routes resolve `AuthSession` to the 500 whose message says
> `install SessionLayer in Slot::Session`. The OAuth flow needs it for more than the principal: the
> PKCE verifier, the `state` and the `nonce` live in the session between the two requests, so
> without it the callback has nothing to compare against. See
> [passwords and sessions](./passwords-and-sessions.md#installing-the-layer).

## Configuring a provider

`OAuthConfig` carries the credentials and `Provider::<name>` picks the endpoint table.

```rust
use moso_auth::{LinkPolicy, OAuthConfig, Provider};
use moso_core::config::SecretString;

fn google() -> moso_auth::Result<Provider> {
    let google = Provider::google(OAuthConfig::new(
        "client-id",
        SecretString::new(std::env::var("GOOGLE_CLIENT_SECRET").unwrap_or_default()),
        "https://app.example.com/auth/oauth/google/callback",
    ))
    .scopes(["openid", "email", "profile"])
    .redirect_allowlist(["https://app.example.com"])
    .link_policy(LinkPolicy::VerifiedEmailOrSession);

    // Call this at boot, not on the first login.
    google.validate()?;
    Ok(google)
}
```

The redirect URI must be the exact string registered with the provider. `validate()` checks that it
is an absolute http(s) URL, that the client id and secret are non-empty (they are usually environment
variables that were not set), that a generic OIDC provider has a discovery URL, that no scope contains
a space, and for Apple that the client secret is a JWT that has not expired.

### The built-in providers

| Constructor | Protocol | Default scopes | What it asserts about an address |
| --- | --- | --- | --- |
| `Provider::google` | OIDC | `openid email profile` | `email_verified`, and means it |
| `Provider::github` | OAuth2 | `read:user user:email` | needs a second request; verified flag from `/emails` |
| `Provider::microsoft` | OIDC | `openid email profile` | nothing. Always `email_verified: false` |
| `Provider::apple` | OIDC | `name email` | the identity token is the whole profile |
| `Provider::gitlab` | OIDC | `openid email profile` | gitlab.com only |
| `Provider::discord` | OAuth2 | `identify email` | reports `verified` |
| `Provider::slack` | OIDC | `openid email profile` | Sign in with Slack |
| `Provider::oidc` | OIDC | `openid email profile` | whatever the discovery document says |

`ProviderId::parse` turns an unknown name into `ProviderId::Oidc(name)`, so a route parameter can pick
a provider without a match arm per vendor.

### Builder options

| Method | Effect |
| --- | --- |
| `.scopes([..])` | Adds to the provider's defaults. |
| `.only_scopes([..])` | Replaces them entirely. |
| `.param(name, value)` | Adds a query parameter to the authorization URL: `access_type=offline`, `prompt=consent`, `hd`, `domain_hint`. |
| `.link_policy(policy)` | See [linking](#linking-an-external-identity). |
| `.redirect_allowlist([..])` | Origins a post-login `next` may point at. |
| `.transport(arc)` | Your own `HttpTransport`, for an egress proxy or a test. |
| `.self_hosted(auth, token, userinfo)` | Point a built-in provider at another hostname, skipping discovery. |

Scopes are passed one per item, never as one space-separated string. `validate()` refuses a scope
containing a space rather than sending a request that will fail confusingly.

### Self-hosted and generic providers

For a self-hosted instance that speaks OpenID Connect (Keycloak, a GitLab instance, one Entra tenant),
use `Provider::oidc` with the discovery URL. The document pins the `iss` an identity token must carry,
which `self_hosted` cannot.

```rust
use moso_auth::{OAuthConfig, Provider};

fn keycloak(config: OAuthConfig) -> Provider {
    Provider::oidc(
        "keycloak",
        "https://id.example.com/realms/main/.well-known/openid-configuration",
        config,
    )
}
```

`self_hosted` exists for GitHub Enterprise Server, which is the same non-OIDC flow on a different
hostname and has no discovery document to point at.

```rust
use moso_auth::{OAuthConfig, Provider};

fn github_enterprise(config: OAuthConfig) -> Provider {
    Provider::github(config).self_hosted(
        "https://github.example.com/login/oauth/authorize",
        "https://github.example.com/login/oauth/access_token",
        Some("https://github.example.com/api/v3/user"),
    )
}
```

> [!WARNING]
> `OAuthConfig::tenant` is the difference between "our staff can sign in" and "anyone in the world
> with a Microsoft account can". Without it, Entra uses the multi-tenant `common` endpoint, whose
> issuer contains the tenant id and therefore cannot be pinned against a fixed string.

Apple's client secret is not a string but a rotating ES256 JWT with a six month maximum lifetime. Put
the signed token in `OAuthConfig::client_secret` and `Provider::validate` will check its shape and
expiry at boot rather than letting it become `invalid_client` at login. Apple has no userinfo endpoint
and sends the user's name only on the first authorization.

## The callback flow

Two handlers: the pair the mounted routes are, and the pair you write when you copy them. The first
mints the authorization request and stashes the half that must not leave the server; the second
redeems the code.

```rust
use moso_auth::{AuthorizationRequest, CallbackParams, OAuthProfile, Provider, Session};

// GET /auth/oauth/google: send the browser on, keep the rest in the session.
async fn start(google: &Provider, session: &Session) -> moso_auth::Result<String> {
    let request = google.authorize(Some("/dashboard")).await?;

    // Write the object by hand: see the note below on why `&request` will not do.
    session.insert(
        "oauth",
        serde_json::json!({
            "url": request.url.to_string(),
            "verifier": request.verifier.expose(),
            "state": request.state.expose(),
            "nonce": request.nonce.as_ref().map(|nonce| nonce.expose()),
            "next": request.next,
            "provider": request.provider,
        }),
    )?;

    Ok(request.url.to_string())
}

// GET /auth/oauth/google/callback: everything in the query string is untrusted.
async fn finish(
    google: &Provider,
    session: &Session,
    code: Option<String>,
    state: String,
) -> moso_auth::Result<OAuthProfile> {
    let request: AuthorizationRequest = session
        .take("oauth")?
        .ok_or(moso_auth::Error::InvalidCredentials)?;
    let params = CallbackParams::new(code.as_deref(), state);
    google.exchange(&request, &params).await
}
```

> [!WARNING]
> `AuthorizationRequest` derives `Serialize`, but three of its fields are `SecretString`s and
> `SecretString::serialize` **always fails**, deliberately, so that a secret cannot be written into a
> config dump. `session.insert("oauth", &request)` therefore returns
> `Error::Config("session value for `oauth` does not serialise: a SecretString cannot be serialised;
> mark the field `#[serde(skip)]`")`, and it compiles, so you find out halfway through a real login.
> Write a value that holds no secret type, as above. Reading back is fine: `Deserialize` works, so
> `session.take::<AuthorizationRequest>("oauth")` reconstructs the type.
>
> The mounted route does the same step through a named conversion rather than an inline object: a
> private `StoredFlow` struct in `crates/moso-auth/src/routes/oauth.rs` carrying the same six fields
> with the URL and the three secrets as plain `String`s, an `of(&request)` that widens them and a
> `restore()` that narrows them back and re-parses the stored URL. A URL that no longer parses is a
> tampered session, and gets the same refusal a mismatched `state` gets. `StoredFlow` is
> crate-private, so copy the shape rather than importing it. Keep it in one named place: the crate
> asserts in a test that `serde_json::to_value(&request)` still fails, precisely because the fix
> looks like redundant boilerplate to whoever tidies up next.

Those three fields must never reach the client: `verifier` (the PKCE secret), `state` and `nonce`.
The type's `Debug` redacts them and strips the URL's query string, because a derived `Debug` would
print a live authorization anybody could complete.

Use `session.take` rather than `session.get`: an authorization request is single use, and leaving it
in the session lets a second callback replay it.

When the user cancels at the provider, the callback carries `error` instead of `code`. Build the
params with `CallbackParams::refused(error, description, state)` and `exchange` will report it after
checking the state.

### What `exchange` does, in order

1. The provider matches. A session that started a Google flow cannot redeem a code at GitHub's token
   endpoint, whatever the state says, so this comes first.
2. `state` is compared in constant time against the one the session holds.
3. A provider-reported `error` is surfaced.
4. The code must be present and non-empty.
5. The code is redeemed at the token endpoint with the PKCE verifier. There is no path that omits it:
   a session that lost its verifier cannot be completed, because redeeming without one is exactly what
   a stolen code needs.
6. For an OIDC provider, the identity token is parsed and checked: `iss` where the issuer is knowable,
   `aud` and `azp` against the client id, `exp` and `iat` with a 60 second skew allowance
   (`oauth::CLOCK_SKEW`), and the `nonce` against the one in the session. An OIDC provider that
   returns no identity token is a ceremony failure, because the nonce is then bound to nothing.
7. The userinfo endpoint is called, and for GitHub a second request to `<userinfo>/emails`.

`IdToken::parse` refuses `alg: none` and an empty signature. Every failure in that list is
`Error::Ceremony { ceremony: "oauth", .. }` with a distinct reason in the log and the same response to
the client.

`check_callback` is steps 1 to 4 on their own, returning the code, for a handler that wants to do
something between the state check and the redemption.

### PKCE

`Pkce::generate` is the only constructor and `authorize` calls it unconditionally, including for a
confidential client where the OAuth 2.1 draft requires it anyway. The method is always `S256`;
`plain` is in the RFC and is worth nothing, since the verifier and the challenge are the same string.
A provider whose discovery document advertises PKCE support without listing `S256` is refused rather
than downgraded.

### The `next` parameter

`authorize(Some("/dashboard"))` validates the target *before* it is stored, so a tampered session
cannot smuggle one past the check on the way out.

With a `redirect_allowlist` configured, the candidate is compared against each entry on origin and
path boundaries: scheme, host and port must match, and the path must equal the allowed path or
continue it after a `/`. A prefix comparison would let `https://app.example.com.evil.test` through
against an entry of `https://app.example.com`, which is the classic way this check is written wrong.

With an empty allowlist, `next` must be a path starting with exactly one `/` and containing no
backslash. `//evil.example` is a protocol-relative URL and `/\evil.example` is treated as one by
several browsers.

### The other `next` check

`moso_auth::routes::validate_next(next, allowlist)` is a second, public check, and it is **not** the
same rule as the one `Provider::authorize` runs above: that one is a private `check_redirect` on
`Provider`, reachable only by passing a `next` to `authorize`. It is what the mounted routes call,
twice per flow (once before the target is stored and once after it comes back out of the session, so
a tampered session store is useless as an open redirect), and it reads the allowlist configured on
`AuthRoutes::redirect_allowlist`, which is a different list from the provider's.

| Target | `validate_next` |
| --- | --- |
| `/dashboard`, `/a/b?c=d#e` | allowed. One leading `/`, and not two |
| `//evil.example`, `//evil.example/path` | refused. Protocol-relative is absolute wearing a path's clothes |
| `/\evil.example`, any value containing `\` | refused. A browser reads `\` as `/` |
| `/\tevil`, `/ evil`, any control character or space | refused. A browser strips them, so `/\tevil` resolves as `//evil` |
| `/%2f%2fevil.example` | refused. It parses here as a path and navigates there as an origin |
| `https://app.example.com/welcome`, allowlist `["https://app.example.com"]` | allowed |
| `https://evil.example`, same allowlist | refused |
| `https://user:pw@app.example.com` | refused. Credentials in the authority |

The comparison is on the **origin** (`scheme://host[:port]`, everything after the authority dropped
from both sides), so an allowlist entry permits an origin rather than a page. That is the one place
it is looser than the provider's own check, which honours a path on an allowlist entry. It is
stricter everywhere else. On the mounted routes both checks run, so what gets through is the
intersection.

Failure is `Error::Ceremony { ceremony: "redirect", .. }`, which collapses to the same 401 everything
else in these flows produces. A probe learns nothing about which rule it tripped.

```rust
use moso_auth::routes::validate_next;

fn allowed() -> moso_auth::Result<()> {
    validate_next("/dashboard", &[])?;
    validate_next("https://app.example.com/welcome", &["https://app.example.com".to_owned()])
}
```

## Linking an external identity

`OAuthProfile::identity_key()` is `"<provider>:<subject>"`, and that is the join key you store. Never
the address. A user who changes their Google address is the same account; a user whose address is
reassigned to somebody else at a corporate provider is not.

```rust
use moso_auth::{check_link, LinkPolicy, OAuthProfile};

fn may_link(profile: &OAuthProfile, authenticated_as_this_user: bool) -> moso_auth::Result<()> {
    check_link(profile, LinkPolicy::default(), authenticated_as_this_user)
}
```

`Provider::check_link(&profile, has_session)` is the same call using the provider's configured policy.

| Policy | Links when | Notes |
| --- | --- | --- |
| `VerifiedEmailOrSession` | the provider asserts a verified address that matches a local account, or the request is already authenticated | the default |
| `SessionOnly` | only from an authenticated session | strictest. A new social login always creates a new account |
| `AnyEmail` | any matching address, verified or not | dangerous. Only safe if every configured provider verifies addresses |

`LinkPolicy::as_str()` gives the audit spelling (`verified_email_or_session`, `session_only`,
`any_email`) and `matches_by_email()` is what a boot report reads to say "this deployment never links
by email".

> [!CAUTION]
> `has_session` means "this request is already authenticated as the account being linked to", not
> "some session exists". Passing `true` for a session belonging to a different user disables the check
> for that request, which is the mistake the function exists to prevent.

The attack the default refuses: the attacker signs up at the provider claiming the victim's address,
the provider does not verify it or lets a user set it freely on a self-hosted instance, and the
application hands over the account. It has been found in production at a long list of companies, and
it is why the OAuth security best current practice tells relying parties not to key on `email`.

A callback handler therefore branches three ways:

1. `identity_key()` already exists in your links table. Log the linked user in.
2. No link, and `check_link` succeeds with a matching local account. Attach the link, then log in.
3. No link and no match. Create an account, or refuse, depending on whether you allow signup by
   social login.

Use `OAuthProfile::verified_email()`, which returns the address only when the provider verified it.
`TokenSet::granted(&["repo"])` tells you whether the scopes you asked for were actually granted, which
is worth checking before you store a refresh token you plan to use.

## Registering a passkey

`WebAuthn::new(rp_id, origin, rp_name)` builds the relying party. The `rp_id` must be a registrable
suffix of the origin's host: `example.com` for an origin of `https://app.example.com`. A mismatch is
an `Error::Config` that says so.

```rust
use moso_auth::{PasskeyCredential, WebAuthn, WebAuthnChallenge};

fn relying_party() -> WebAuthn {
    WebAuthn::new("example.com", "https://example.com", "Example")
        .require_user_verification(true)
        .timeout(std::time::Duration::from_secs(60))
}

fn begin(rp: &WebAuthn, existing: &[PasskeyCredential]) -> moso_auth::Result<WebAuthnChallenge> {
    rp.start_registration("usr_1", "ada@example.com", "Ada", existing)
}
```

`existing` becomes the exclude list, so an authenticator that already holds a credential for this
account says so instead of silently making a second one. Send `challenge.options` to the browser as
JSON (`options_json()` is the string form), store `challenge.state` server-side, and post the browser's
response back.

"Server-side" means the session, in the mounted routes: under `_passkey_register` and
`_passkey_login`, so nothing accumulates a table of half-finished ceremonies for anybody to race.
`WebAuthnChallenge::state` is a `SecretString` and hits the same refusal `AuthorizationRequest` does,
so the mounted version converts through a private `StoredChallenge` (options, state as a `String`,
and `expires_at`) for the reason `webauthn-rs` ships `danger-allow-state-serialisation` at all:
without persisting it, passkeys work only on a single process with sticky sessions. The challenge is
**consumed by the attempt that reads it**, succeed or fail, and an expired one is
`Error::Expired { kind: "webauthn challenge" }` rather than a retry.

Here is the full round trip, from the crate's own test against a virtual authenticator:

```rust
let rp = rp();                       // WebAuthn::new("example.com", "https://example.com", "Example")
let mut browser = VirtualBrowser::new();

let challenge = rp
    .start_registration("usr_1", "ada@example.com", "Ada", &[])
    .expect("registration starts");
let response = browser.register(&challenge);
let credential = rp
    .finish_registration_for("usr_1", &challenge, &response)
    .expect("registration finishes");

assert_eq!(credential.user_id, "usr_1");
assert!(credential.user_verified);
assert_eq!(credential.algorithm, -7);
assert!(credential.is_active());

let challenge = rp
    .start_authentication(std::slice::from_ref(&credential))
    .expect("authentication starts");
let response = browser.authenticate(&challenge);
let counter = rp
    .finish_authentication(&challenge, &response, &credential)
    .expect("authentication finishes");

assert!(counter > credential.sign_count);
```

`finish_registration` attributes the credential to the account that *started* the ceremony, because a
registration response carries no account of its own. The alternative would be trusting one the request
supplies, and a request that supplies the wrong one attaches a working credential to somebody else's
account. `finish_registration_for(user_id, ..)` adds an assertion for handlers that read the session
and the challenge from two different places.

### What to persist

`PasskeyCredential` is the storage schema. Every field is public.

| Field | Notes |
| --- | --- |
| `credential_id` | base64url. The lookup key. |
| `user_id` | Whose it is. |
| `public_key` | Canonical CBOR `COSE_Key`. |
| `sign_count` | The authoritative counter. Overrides the one inside `record` when rebuilt. |
| `aaguid` | Which authenticator model, when it says. |
| `discoverable` | From the unsigned `credProps` extension. A UI hint, never a security property. |
| `label`, `created_at`, `last_used_at` | For the "your passkeys" screen. |
| `user_handle` | base64url, 16 bytes. |
| `user_verified`, `backup_eligible`, `backup_state` | What the ceremony established. |
| `algorithm` | COSE alg id: `-7` ES256, `-8` EdDSA, `-257` RS256. |
| `transports` | `usb`, `nfc`, `ble`, `internal`, `hybrid`. |
| `disabled` | Set by `disable()`. `is_active()` reads it. |
| `record` | Opaque ceremony state. Store as `jsonb`. |

The counter column is the single authority: a store that updates `sign_count` and leaves `record`
alone is still correct.

## Authenticating with a passkey

```rust
use moso_auth::{PasskeyCredential, WebAuthn, WebAuthnChallenge};

fn begin_auth(rp: &WebAuthn, allow: &[PasskeyCredential]) -> moso_auth::Result<WebAuthnChallenge> {
    rp.start_authentication(allow)
}
```

With a non-empty `allow` list, credentials registered as verifying win and non-verifying ones are
dropped from the challenge. A challenge where every offered credential is disabled is an
`Error::Ceremony`.

Three flavours of the usernameless flow:

| Call | Mediation | Use |
| --- | --- | --- |
| `start_authentication(&[])` | none | a "sign in with a passkey" button |
| `start_conditional_authentication()` | conditional | autofill in a username field |
| `start_authentication(&creds)` | none | the user already told you who they are |

The usernameless flow has one extra step: you have a signature and no idea whose account it belongs
to. `identify_discoverable(&response)` reads the credential id and user handle out of the response so
you can look the credential up, then you finish the ceremony with it.

```rust
use moso_auth::{PasskeyStore, WebAuthn, WebAuthnChallenge};

async fn finish_discoverable(
    rp: &WebAuthn,
    store: &dyn PasskeyStore,
    challenge: &WebAuthnChallenge,
    response: serde_json::Value,
) -> moso_auth::Result<u32> {
    // Nothing here is verified yet. This is unauthenticated client input.
    let discovered = rp.identify_discoverable(&response)?;
    let credential = store
        .find(&discovered.credential_id)
        .await?
        .ok_or(moso_auth::Error::InvalidCredentials)?;

    rp.finish_authentication(challenge, &response, &credential)
}
```

`finish_authentication` returns the new counter. `assert` returns a `PasskeyAssertion` with the same
work plus `user_verified`, `backup_eligible`, `backup_state` and a `needs_update` flag telling you
whether the stored credential should be written back. Use `assert` when you persist backup state,
`finish_authentication` when you only need the counter.

Both check, before the signature: that the credential is not disabled, that the response was signed by
the credential you supplied, that the challenge has not expired (`WebAuthnChallenge::has_expired`),
and that the ceremony tag matches. WebAuthn's own `timeout` is advice to the browser and binds nothing
on the server, so the server-side lifetime is the one that counts. The ceremony tag is what stops a
registration state being handed to `finish_authentication`, which would be a type confusion with a
signature attached.

### Clone detection

One authentication failure is not "try again". A signature counter that goes backwards means two
devices are presenting one private key, and which of them is the legitimate one is not knowable from
the request. Refuse both until a person has looked:

```rust
use std::sync::Arc;

use moso_auth::webauthn::is_clone_detected;
use moso_auth::{PasskeyCredential, PasskeyStore, WebAuthn, WebAuthnChallenge};

async fn handle(
    rp: &WebAuthn,
    store: &Arc<dyn PasskeyStore>,
    challenge: &WebAuthnChallenge,
    response: &serde_json::Value,
    credential: &PasskeyCredential,
) -> moso_auth::Result<u32> {
    match rp.finish_authentication(challenge, response, credential) {
        Ok(counter) => Ok(counter),
        Err(error) if is_clone_detected(&error) => {
            // Somebody holds a copy of a key that was supposed to be unextractable.
            store.disable(&credential.credential_id).await?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}
```

The mounted `POST /auth/passkeys/login/finish` already does exactly this. Leaving the row live would
make clone detection a log line rather than a defence: the next attempt with a plausible counter
walks straight in. A disabled credential is refused before its signature is even looked at, so the
copy that *was* legitimate cannot silently resume either.

The client is told nothing extra, deliberately: the response is the same 401 a wrong credential gets,
because whether a key was copied is a fact about somebody else's account. The channel to alert on is
the log. `moso_auth::webauthn::CLONE_EVENT` (`passkey.clone_detected`) is the stable `event` field on
the `ERROR` line the mounted route writes, carrying the account and the credential id;
`CLONE_DETECTED` is the exact reason string, exported so the comparison cannot drift away from the
message. Notifying the user is your job: `moso-auth` does not depend on `moso-mail`.

### Passkeys as one factor or two

`require_user_verification(true)` (the default) selects the passkey ceremony: the authenticator must
demand a PIN or a biometric, so the credential is two factors on its own. `require_user_verification(false)`
selects the security-key ceremony, where verification is preferred rather than required and the
credential is one factor of two. That is a different product decision, not a relaxation, and it is why
the switch selects a ceremony rather than silently doing nothing.

## Storing credentials

`PasskeyStore` is the trait, and both halves ship: `MemoryPasskeyStore` for a process with no
database yet, and `moso_auth::store::TablePasskeyStore` over `moso_auth_passkeys`. Hand either to
`AuthState::passkeys` and the four mounted routes work:

```rust
use std::sync::Arc;

use moso_auth::store::MemoryPasskeyStore;
use moso_auth::{AuthState, PasskeyStore, SessionStore, WebAuthn};

fn state(sessions: Arc<dyn SessionStore>) -> AuthState {
    AuthState::new(sessions)
        .webauthn(WebAuthn::new("example.com", "https://example.com", "Example"))
        .passkeys(MemoryPasskeyStore::shared() as Arc<dyn PasskeyStore>)
}
```

The memory store is complete, not a test double: one conformance suite runs against it and against
the table store, so the unique index on the credential id, the authoritative counter column and the
quarantine bit behave identically. What it is not is shared. It is **single process**, and its
`Debug` says so (`single_process: true`): two instances behind a load balancer each keep their own
map, so a passkey registered against one is unknown to the other and a credential quarantined on one
still authenticates on the other. Switch to `TablePasskeyStore` before you run a second instance.

```rust
pub trait PasskeyStore: Send + Sync + 'static {
    fn insert<'a>(&'a self, credential: &'a PasskeyCredential) -> BoxFuture<'a, Result<()>>;
    fn find<'a>(&'a self, credential_id: &'a str) -> BoxFuture<'a, Result<Option<PasskeyCredential>>>;
    fn list_for_user<'a>(&'a self, user_id: &'a str) -> BoxFuture<'a, Result<Vec<PasskeyCredential>>>;
    fn update_counter<'a>(&'a self, credential_id: &'a str, sign_count: u32) -> BoxFuture<'a, Result<()>>;
    fn disable<'a>(&'a self, credential_id: &'a str) -> BoxFuture<'a, Result<bool>>;
    fn delete<'a>(&'a self, credential_id: &'a str) -> BoxFuture<'a, Result<bool>>;
}
```

`find` takes a credential id and no user id, which is what makes the usernameless flow possible, and
it returns disabled rows: `WebAuthn::assert` refuses those by name, and a store that hid them would
report a quarantined key as an unknown key. `insert` is not an upsert, because a credential id is a
primary key and overwriting one would move a credential somebody else holds onto a new account.
`update_counter` writes what it is told in both directions, since `WebAuthn::assert` has already
refused a regressed counter and a store that clamped would only be hiding a caller that skipped the
verifier. `disable` is what clone detection calls: a compare-and-set, so two requests quarantining
one credential produce one alert and not two, and it keeps the row rather than deleting it, so an
audit can still resolve the credential id and the user can be told which key stopped working.
`delete` is for a user removing a key they no longer own.

The futures are boxed so the trait is dyn-compatible: your store lives behind an `Arc<dyn
PasskeyStore>`, and one allocation per ceremony is not a number anyone will measure next to a
signature verification.

`WebAuthn::user_handle_for(user_id)` is the deterministic mapping from your user key to the 16-byte
handle: a UUID passes through, anything else becomes a v8 UUID derived from its SHA-256.

## Failure modes

**Every OAuth failure looks the same to the client.** Mismatched state, a missing PKCE verifier, a
nonce that does not match, an unverified address on an auto-link: all `Error::Ceremony` with a
distinct `reason` in the server log. Log the reason, return a generic failure.

**GitHub's address needs `user:email`.** `/user` returns only the public address, which most users
have not set, so the flow makes a second call to `<userinfo>/emails`. A token without that scope gets
a 403 there, which is not a login failure, only a login without a verified address.

**Entra never auto-links under the default policy.** It asserts nothing about an address, so an Entra
profile always reports `email_verified: false`.

**The bundled HTTP client has limits.** `oauth::http::DEFAULT_TIMEOUT` is 10 seconds,
`MAX_BODY` is 1 MiB and `MAX_REDIRECTS` is 3. `RustlsTransport::shared()` is the default client;
`.transport(..)` replaces it with your own `HttpTransport` for an egress proxy or a test.

**Discovery is a network call.** `authorize` resolves endpoints, which for a discovery-configured
provider means fetching the document. Call `validate()` at boot so a typo in a URL is a boot error, and
expect the first login after a restart to pay for the fetch.

**`Pkce::generate` panics if the OS CSPRNG fails.** So does `SessionId::generate`. Both are documented
as deliberate: there is no safe fallback.

**A passkey `record` is only readable by `webauthn-rs`.** It is stored because `Credential` is the
storage schema, behind the crate's `danger-credential-internals` feature. Treat it as opaque, keep it
next to the counter, and do not try to migrate it by hand.

**Session state is required for both flows.** The OAuth `AuthorizationRequest` and the WebAuthn
challenge both live in the session between requests. That means a session store that survives a
process restart, or a deployment with sticky sessions. See
[passwords and sessions](./passwords-and-sessions.md).

## See also

- [Authentication](./authentication.md) for the whole battery.
- [JWT and API keys](./jwt-and-api-keys.md), including the identity-token checks reused here.
- [Passwords and sessions](./passwords-and-sessions.md) for what happens after a successful ceremony.
- [Security](./security.md) for the rest of the defaults.
