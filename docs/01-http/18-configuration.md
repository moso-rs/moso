# 18 - Configuration

> **Status: implemented.** One signature changed - the `Config` trait is built around
> `load_nested(&ConfigLoader, &ConfigKey, &mut BootErrors)`, marked inline below (decision D10).
> `flags!` (typed feature flags) is ⛔ not implemented.

## Principles

- **Typed, not `HashMap<String, String>`.** Config is a struct; a missing or malformed value is a
  boot error naming the key, the source, and the expected type.
- **Layered, with a documented precedence.**
- **Profiles** (`dev`, `test`, `production`) change *defaults*, never *semantics*.
- **Secrets are a distinct type** that cannot be logged or serialised by accident.
- **Every key is discoverable.** `moso config` prints the resolved config with sources and
  redaction; `moso config --env-example` regenerates `.env.example`.

## Deriving config

```rust
// example - src/config.rs
use moso::config::prelude::*;

#[derive(Config, Clone)]
pub struct AppConfig {
    /// Human-readable service name, used in logs and the OpenAPI title.
    #[config(default = "shop")]
    pub name: String,

    #[config(default = "0.0.0.0:3000")]
    pub bind: SocketAddr,

    /// Base URL used to build absolute links in emails and Location headers.
    pub public_url: Url,

    #[config(nested)]
    pub database: DatabaseConfig,

    #[config(nested)]
    pub kv: KvConfig,

    #[config(nested)]
    pub mail: MailConfig,

    #[config(secret)]
    pub secret_key: SecretString,

    #[config(default = "info", env = "RUST_LOG")]
    pub log: String,

    #[config(default = false, profile(production = false, dev = true))]
    pub expose_docs: bool,
}
```

`DatabaseConfig`, `KvConfig`, `MailConfig` etc. are provided by the batteries and also derive
`Config`, so users compose rather than redeclare.

```rust
// spec
pub trait Config: Sized + Send + Sync + 'static {
    fn load() -> Result<Self>;
    // AS BUILT (decision D10) - the original `load_from(&[Box<dyn ConfigSource>])` could neither
    // report every bad field in one run nor express `#[config(nested)]`:
    //   fn load_nested(loader: &ConfigLoader, prefix: &ConfigKey, errors: &mut BootErrors)
    //       -> Option<Self>;                       // ← what the derive implements
    //   fn load_from(loader: &ConfigLoader) -> Result<Self> { .. }   // defaulted
    fn load_from(loader: &ConfigLoader) -> Result<Self>;
    /// Field metadata for `moso config` and `.env.example` generation.
    fn descriptor() -> &'static ConfigDescriptor;
}
```

## Precedence (highest wins)

1. **Explicit overrides in code** (`AppConfig::load()?.with_bind(addr)`) - used by tests.
2. **Command-line flags** - `--bind`, `--log`, and `--set database.max_connections=20`.
3. **Environment variables** - `SHOP__DATABASE__URL` (double underscore = nesting), or the
   `#[config(env = "...")]` alias (`DATABASE_URL`, `REDIS_URL`, `PORT` are aliased by default
   because platforms set them).
4. **`.env` file** - loaded only in `dev` and `test` profiles. Never in production; the docs
   explain why (it hides the real source of a value from your platform's config UI).
5. **`config/{profile}.toml`** - committed, profile-specific.
6. **`config/default.toml`** - committed, shared.
7. **`#[config(profile(...))]` defaults** - per-profile defaults in code.
8. **`#[config(default = ...)]`** - the base default.

Profile is chosen by `MOSO_PROFILE`, defaulting to `dev` under `cargo run`/`moso dev`, `test` under
`cargo test`, and `production` in a release binary with no `.env` present. The resolved profile is
logged at boot, prominently, because "which config am I running" is a recurring production
confusion.

## Secrets

```rust
// spec - moso-core/src/config/secret.rs
pub struct SecretString(/* zeroising */);
impl Debug   for SecretString { /* "SecretString(***)" */ }
impl Display for SecretString { /* "***" */ }
impl Serialize for SecretString { /* errors unless `serde_secret` feature */ }
impl SecretString {
    pub fn expose(&self) -> &str;         // deliberately verbose at the call site
}
```

- `#[config(secret)]` requires the field type to be `SecretString`/`SecretBytes`; using `String`
  is a compile error with a fix-it.
- A `DATABASE_URL` containing a password is parsed into `DatabaseConfig` with the password held as
  a secret, and `Display` for the config renders `postgres://user:***@host/db`.
- Secret sources beyond env: `#[config(secret_from = "file")]` reads `${KEY}_FILE` (Docker/K8s
  secret mounts), and a `SecretProvider` trait allows Vault/AWS Secrets Manager integrations
  without Moso depending on them.

## Boot-time config errors

```
error: configuration is invalid (2 problems)

  ✗ missing required value: `public_url`
      env       SHOP__PUBLIC_URL
      or file   config/production.toml  →  public_url = "https://…"
      type      Url

  ✗ invalid value for `database.max_connections`
      source    env SHOP__DATABASE__MAX_CONNECTIONS = "many"
      expected  integer in 1..=1000
      note      also settable as DATABASE_MAX_CONNECTIONS

  4 sources were consulted, in order:
      cli flags, env, .env (not found), config/production.toml, config/default.toml
```

Requirements: all problems at once; name the exact env var *and* the file key for each; show the
source chain that was consulted.

## Inspecting configuration

```
$ moso config
profile: production

name                       "shop"                       config/default.toml:2
bind                       0.0.0.0:3000                 default
public_url                 https://api.shop.example     env SHOP__PUBLIC_URL
database.url               postgres://app:***@db/shop   env DATABASE_URL
database.max_connections   20                           config/production.toml:8
secret_key                 ***                          env SHOP__SECRET_KEY
expose_docs                false                        profile default (production)
```

`moso config --json` for tooling. `moso config --check` validates without booting the app - useful
as a deploy pre-flight step, and it is what the generated CI workflow runs.

## Hot reload

Config is immutable after boot, with one exception: values marked `#[config(reloadable)]` are stored
in an `ArcSwap` and re-read on `SIGHUP` or when the config file changes in `dev`. Reloadable values
are read through `Config::get()` rather than a plain field:

```rust
// example
#[config(reloadable, default = "info")]
pub log: Reloadable<String>,
```

Only a handful of things should be reloadable (log level, feature flags, rate limits). Making the
DB URL reloadable is a trap; the derive rejects `reloadable` on nested config that batteries
consume at boot.

## Feature flags

Small, built-in, no external service required - because every app grows one and rolling your own is
a week nobody has:

```rust
// spec
pub struct Flags(/* ArcSwap<HashMap<FlagKey, FlagRule>> */);
impl Flags {
    pub fn enabled(&self, flag: FlagKey, ctx: &FlagCtx) -> bool;
}
```

⛔ **Feature flags are not implemented.** There is no `flags!`, no `Flags`, no `FlagSource` and no
admin UI to expose them in. Intended behaviour: boolean, percentage rollout (hashed on a stable key
so a user's bucket is sticky), allow/deny lists and time windows, sourced from a config file, a DB
table or a custom `FlagSource`, with `flags!{}` generating a typed key enum so a typo is a compile
error.

## The generated `.env.example`

`moso config --env-example` regenerates it from the descriptor, including doc comments and
defaults, so it never rots:

```
# Human-readable service name, used in logs and the OpenAPI title.
SHOP__NAME=shop

# Base URL used to build absolute links in emails and Location headers.  [required]
SHOP__PUBLIC_URL=

# Postgres connection string.  [required]
DATABASE_URL=postgres://postgres:postgres@localhost:5432/shop_dev
```

CI checks it is up to date, same as OpenAPI drift.

## Acceptance criteria (WP-10a)

1. Precedence order is verified by a test that sets the same key at all eight levels.
2. A missing required key and a malformed key are reported together, with env name and file key.
3. `SecretString` does not appear in `Debug`, `Display`, `serde_json::to_string`, or a tracing
   field; enforced by a test that greps a canary.
4. `moso config` output matches the resolved values, with correct source attribution.
5. `.env.example` drift check fails when a config field is added without regenerating.
6. `Reloadable<T>` picks up a SIGHUP change without dropping in-flight requests.
7. `#[config(secret)]` on a `String` field is a compile error suggesting `SecretString`.
