---
title: Configuration
description: Declare one typed struct per configuration section, load it from eight layered sources, keep secrets out of every rendering, and fail the boot with a report instead of a panic.
order: 10
status: shipped
---

Configuration in Moso is a struct you declare. You put `#[derive(Config)]` on it, the fields become
the keys, and one call resolves every key from a documented stack of sources before the server
binds. A missing or malformed value is a boot error naming the key, the expected type, every
environment variable that would have supplied it and the file key to write. There is no
`HashMap<String, String>`, no `config.get("port").unwrap()`, and no failure that waits for the first
request that touches the value.

None of this is behind a Cargo feature. `#[derive(Config)]`, the loader, profiles and secrets are
part of `moso-core` and are always compiled in.

> [!NOTE]
> Two things are scope choices rather than features. The reload mechanism ships, but you wire the
> `SIGHUP` (or any other) signal to it yourself; and feature flags are a plain `Config` bool rather
> than a dedicated macro. Each is described inline where you would reach for it. `moso config --check`,
> `moso config --generate-secret` and the `.env.example` drift check are part of `--check`, which
> exits non-zero, so CI can gate on it.

## The smallest working configuration

```rust title="src/lib.rs"
/// Everything this application can be configured with.
#[derive(Config, Debug)]
pub struct AppConfig {
    /// The word placed before the name. Override with `GREETING=Hei`.
    #[config(default = "Hello")]
    pub greeting: String,
}

/// The composition root: everything the application *is*, in one expression.
pub fn app() -> Result<AppBuilder> {
    Ok(App::new(AppConfig::load()?).mount(moso::routes! { GET "/hello/{name}" => hello }))
}
```

`App::new(config)` registers the struct as a provider, so any handler reaches it with `Inject`:

```rust title="src/routes/hello.rs"
/// Greet someone by name.
#[endpoint]
async fn hello(
    Path(name): Path<String>,
    Inject(config): Inject<AppConfig>,
) -> Result<Json<Greeting>> {
    Ok(Json(Greeting {
        message: format!("{}, {name}!", config.greeting),
        name,
    }))
}
```

Outside a request, `resolver.config::<AppConfig>()` and `ctx.config::<AppConfig>()` are the same
lookup. Resolution happens once; a handler pays for an `Arc` clone. See
[dependency injection](./dependency-injection.md) for how the provider map works.

`Config` is in `moso::prelude`, alongside `SecretString`. A configuration module usually imports
`moso::config::prelude::*` instead, which adds `Profile`, `Reloadable`, `SecretBytes`, `SocketAddr`,
`PathBuf` and `Duration`. `Url` is not in that prelude: a `Url` field needs `use moso::schema::Url;`.

## How a key is spelled

One field is one key, and that key has three spellings the framework generates for you.

| Where | Spelling for `database.max_connections` with prefix `shop` |
| --- | --- |
| Dotted key (TOML, `--set`) | `database.max_connections` |
| Environment, prefixed | `SHOP__DATABASE__MAX_CONNECTIONS` |
| Environment, unprefixed | `DATABASE__MAX_CONNECTIONS` |

Levels are separated by a double underscore, because a single one is already legal inside a field
name. The prefix comes from `ConfigLoader::with_prefix("shop")` or from the `MOSO_CONFIG_PREFIX`
environment variable; with no prefix, only the unprefixed spelling exists.

## The layers, highest wins

`ConfigLoader::standard()` builds this stack. Level 1 beats level 2, and so on down.

| Level | Source | `name()` | Notes |
| --- | --- | --- | --- |
| 1 | Overrides set in code | `code` | `OverrideSource::set(key, value)` |
| 2 | Command-line flags | `cli flags` | `--bind=0.0.0.0:8080`, `--log debug`, `--set a.b=c`, `--database.url=…` |
| 3 | Environment variables | `env` | Prefixed, unprefixed, explicit aliases, well-known aliases |
| 4 | `.env` | `.env` | Loaded in `dev` and `test` only, discovered by walking up from the working directory |
| 5 | `config/{profile}.toml` | the path | `config/production.toml` and friends |
| 6 | `config/default.toml` | the path | Profile-independent |
| 7 | `#[config(profile(..))]` | `profile defaults` | The entry for the active profile |
| 8 | `#[config(default = …)]` | `defaults` | The value written on the field |

Within a single source, Moso tries the prefixed environment spelling, then the canonical dotted key,
then an explicit `#[config(env = ..)]` alias. The canonical spelling is always tried before an alias
so an alias can never shadow the name your documentation gives. Across sources, a higher level wins
whatever the spelling.

For a secret field with nothing in any source, one more thing is consulted before the defaults:
`${KEY}_FILE`. See [secrets](#secrets).

### Well-known aliases

Five unprefixed names are read because platforms set them and no amount of documentation changes
that:

| Variable | Key |
| --- | --- |
| `DATABASE_URL` | `database.url` |
| `REDIS_URL` | `kv.url` |
| `PORT` | `port` |
| `HOST` | `host` |
| `RUST_LOG` | `log` |

Turn them off for one source with `EnvSource::without_aliases()`, which matters on a host running
several services from one environment.

### The committed TOML files

Levels 5 and 6 are two files in a `config/` directory. Level 5 is named after the active profile,
level 6 is shared by all of them.

```toml title="config/default.toml"
name = "shop"

[database]
max_connections = 10
```

Values keep their TOML types, so `max_connections = 10` arrives as an integer rather than as the
string `"10"`. A file that does not exist is an empty, unavailable source and shows in reports as
`config/production.toml (not found)`. A file that exists and does not parse fails the boot naming
the file and the line.

Line numbers come from a small textual pass over the file rather than a second TOML parser, so a
key written inside an inline table honestly reports no line instead of a wrong one.

### Moving the sources around

```rust title="src/lib.rs"
/// The prefix every environment variable for this application carries.
pub const ENV_PREFIX: &str = "shop";

/// The configuration stack, in precedence order.
pub fn loader() -> Result<moso::config::ConfigLoader> {
    Ok(moso::config::ConfigLoader::standard()?.with_prefix(ENV_PREFIX))
}
```

`AppConfig::load()` is `load_from(&ConfigLoader::standard()?)`. Once you want a prefix, a pinned
profile or an extra source, build the loader yourself and call `AppConfig::load_from(&loader)`.

- `ConfigLoader::for_profile(Profile::Production)` pins the profile instead of detecting it.
- `ConfigLoader::with_source(Box::new(..))` appends a source at the bottom of the stack.
- `ConfigLoader::from_sources([..])` builds an arbitrary stack, with the profile set to `Test`, an
  empty prefix and no secret providers.

Three environment variables steer the loader itself, before any of your keys are read:

| Variable | Constant | Effect |
| --- | --- | --- |
| `MOSO_PROFILE` | `PROFILE_ENV` | Names the profile and beats every detection heuristic |
| `MOSO_CONFIG_PREFIX` | `PREFIX_ENV` | Sets the application prefix without a code change |
| `MOSO_CONFIG_DIR` | `CONFIG_DIR_ENV` | Moves the committed TOML directory away from `config` |

Two sharp edges on the builder. `with_prefix` rebuilds the `env` source from scratch, which resets
`without_aliases()` back to on, so call `with_prefix` first if you use both. And `with_prefix` and
`with_secret_provider` are not `#[must_use]` while `with_profile` and `with_source` are, so dropping
the result of the first two silently loses the setting.

## Declaring fields

```rust title="src/config.rs"
#[derive(Config, Debug)]
pub struct AppConfig {
    /// The name this instance reports at `/status` and in the API document.
    #[config(default = "moso blog")]
    pub name: String,

    /// The address to listen on. Override with `BIND=127.0.0.1:8080`.
    #[config(default = "0.0.0.0:3000")]
    pub bind: SocketAddr,

    /// The public base URL, used for `Location` headers and the OpenAPI server
    /// entry. Override with `PUBLIC_URL`.
    #[config(default = "http://localhost:3000")]
    pub public_url: Url,

    /// The shared key every write endpoint requires in `x-api-key`.
    #[config(default = "let-me-in", secret)]
    pub api_key: SecretString,

    /// Listing behaviour. A nested section: `POSTS__PAGE_SIZE=5`.
    #[config(nested)]
    pub posts: PostsConfig,
}

/// How the listing endpoint behaves.
#[derive(Config, Debug)]
pub struct PostsConfig {
    /// The page size used when a request does not ask for one.
    #[config(default = 20, range = 1..=100)]
    pub page_size: u32,
}
```

Doc comments are not decoration. They become the comment above each key in the generated
`.env.example`.

| Attribute | Form | Meaning |
| --- | --- | --- |
| `default` | `default = "shop"`, `= 20`, `= false`, `= 1.5`, `= 'x'`, `= -1` | Level 8. Rendered as text, so it is coerced by the same code as the environment |
| `env` | `env = "RUST_LOG"` | An explicit alias, tried after the canonical key within each source |
| `secret` | `secret` | Redacted everywhere; requires `SecretString` or `SecretBytes` |
| `nested` | `nested` | The field is another `Config`; its keys are rooted under this field name |
| `profile` | `profile(dev = true, production = false)` | Level 7, any subset of `dev`, `test`, `production` |
| `range` | `range = 1..=1000`, `= 0..100`, `= 0..`, `= ..=100` | Bound checked after coercion |
| `reloadable` | `reloadable` | Requires a `Reloadable<T>` field |
| `secret_from` | `secret_from = "file"` or `"env"` | Also implies `secret` |
| `parse` | `parse` | Read as `String`, then `FromStr` |

A field with no `default` is required: nothing supplies it and the boot fails naming the key. A
field typed `Option<T>` is optional: absence is a value, not an error. An empty or whitespace-only
string counts as absence everywhere, so `FOO=` in a platform UI means "I cleared this" rather than
`Some("")`.

### Types a field can have

`Coerce` decides what text a field accepts. The implementations that ship:

| Type | Accepts |
| --- | --- |
| `String` | Any scalar; an integer or bool renders to its text |
| `bool` | A TOML bool, or `1/true/yes/on/y` and `0/false/no/off/n`, case-insensitively |
| `i8` .. `i128`, `isize` | A TOML integer (range-checked), a whole float, a parsable string |
| `u8` .. `u128`, `usize` | The same, non-negative |
| `f32`, `f64` | A float, an integer, a parsable string |
| `SocketAddr`, `IpAddr`, `Ipv4Addr`, `Ipv6Addr` | Their `FromStr` |
| `PathBuf` | Any scalar, verbatim |
| `Duration` | A bare number as seconds, or humantime (`30s`, `5m`, `1h30m`); `0.5` is 500 ms |
| `moso::schema::Url` | Absolute URLs only |
| `SecretString`, `SecretBytes` | See below; the error never quotes the value |
| `Profile` | `dev`, `development`, `test`, `prod`, `production` |
| `LogFormat` | `pretty`, `json`, `compact` |
| `Option<T>` | Whatever `T` accepts; empty text is `None` |
| `Vec<T>` | A TOML array, or a comma-separated string with each element trimmed |
| `Reloadable<T>` | Whatever `T` accepts, then wrapped |

Anything else needs `#[config(parse)]` and a `FromStr` implementation. `parse` and `range` cannot be
combined, because a value produced by `FromStr` is not something the range check can compare.

### Attribute combinations that will not compile

The derive rejects these at compile time, each with a paste-able fix:

- `#[config(secret)]` on a type that is not `SecretString` or `SecretBytes` (or an `Option` of one).
- `#[config(reloadable)]` on a bare type instead of `Reloadable<T>`.
- `#[config(nested)]` combined with `env`, `range`, `profile`, `secret`, `parse` or `default`. A
  nested section is described by its own type.
- `#[config(nested, reloadable)]`. A battery reads its section once at boot, so a reloaded database
  URL would never reach the pool that was already built from the old one.
- `#[config(secret_from = "…")]` with anything other than `"file"` or `"env"`.
- A generic struct, a tuple struct or an enum. The derive stores one process-wide descriptor per
  type, and one `static` cannot describe every instantiation.

When a rule fires you get exactly one error. The derive still emits a placeholder `Config` impl so
`App::new(cfg)` does not add a second, misleading "trait bound is not satisfied".

## Profiles

There are three: `Dev`, `Test`, `Production`.

| Profile | `as_str()` | Loads `.env` | Exposes internal errors | Config file |
| --- | --- | --- | --- | --- |
| `Dev` | `dev` | yes | yes | `config/dev.toml` |
| `Test` | `test` | yes | no | `config/test.toml` |
| `Production` | `production` | no | no | `config/production.toml` |

`Profile::detect()` runs in this order:

1. `MOSO_PROFILE`, if it parses. A value that does not parse logs a `WARN` and falls through.
2. A `cargo test` harness, detected by `cfg!(test)` or a binary whose parent directory is `deps`,
   gives `Test`.
3. A debug build, or a process launched by cargo (`CARGO` is set), gives `Dev`.
4. A `.env` in the working directory or any ancestor gives `Dev`.
5. Otherwise `Production`.

Five spellings parse: `dev`, `development`, `test`, `prod` and `production`. Anything else is not a
profile, which is why an unrecognised `MOSO_PROFILE` warns instead of silently meaning something.

Override it in the composition root with `App::new(cfg).profile(Profile::Production)`, when building
the stack with `ConfigLoader::for_profile(..)`, or in a test with `TestApp::builder().profile(..)`.
The resolved profile is logged once per process at `INFO`, with `profile`, `dotenv`,
`exposes_errors` and `config_file` as structured fields. `Profile` also implements `Coerce`, so it
can be a field in your own configuration struct.

A field can carry a different default per profile. The derive picks the entry for the active profile
at load time, and any subset of the three keys is allowed:

```rust
/// Whether the interactive API docs are served.
#[config(default = false, profile(production = false, dev = true))]
pub expose_docs: bool,
```

**A profile changes defaults, never semantics.** `dev` does not disable validation, skip
authorization, or relax a limit that protects the process. The only thing the framework's own
profile defaults change is `expose_docs`, and there are tests asserting that no profile exposes
internal errors or loosens a limit. If you want "it worked in dev" to stop being a category of bug,
keep your own `#[config(profile(..))]` defaults to the same discipline.

`.env` is not merely ignored in production: the source is never pushed onto the stack, because a
source that exists but is never consulted is a source somebody eventually consults by accident. A
`.env` in production hides where a value really came from, and the platform's configuration UI and
the process then disagree during an incident. Note also that `.env` is parsed **into a source**, not
exported into `std::env`, so `moso config` can still attribute the value and nothing mutates the
process environment at runtime.

## Secrets

A `String` holding a database password is indistinguishable from any other `String`, so it ends up
in a `Debug` line, a tracing field, a serialised error or a crash dump. Moso makes it a distinct
type instead.

```rust
#[derive(Config, Debug)]
pub struct AppConfig {
    /// Connection string; never logged.
    #[config(secret)]
    pub database_url: SecretString,

    /// Cookie signing key, 32 bytes. Write it as `base64:…` or `hex:…`.
    #[config(secret)]
    pub cookie_key: SecretBytes,
}
```

What the types guarantee:

- `Debug` prints `SecretString(***)`, `Display` prints `***`.
- Serialising is a hard **error**, not a redaction. A redaction round-trips into `"***"` and gets
  written back to a database by the next person who deserialised it. Failing loudly at the one call
  site that wanted it beats leaking quietly at all of them. The fix is `#[serde(skip)]` on the field,
  or a `serialize_with` that writes `"***"` on purpose.
- The buffer is zeroed on drop.
- `==` does not return early on the first differing byte. Lengths still differ observably, which is
  not a secret worth protecting here.
- `expose()` and `expose_bytes()` are the only ways to read the value, which makes the read sites
  greppable at review time.

What they do not guarantee: that the value never reaches swap or a core dump (memory locking is an
operating-system control), and that a copy you made with `expose()` is zeroed. That copy is yours.

`SecretString::redact_within(text)` rewrites a secret inside a larger string, which is how
`postgres://user:pw@db/shop` becomes `postgres://user:***@db/shop`. Secrets under four bytes are
deliberately not substituted.

`SecretBytes` accepts `base64:aGk=` and `hex:00ff`, and the prefix is required. "Is this hex, or a
password that happens to be sixteen hex characters" is not a question a framework should answer by
guessing. `SecretBytes::from_hex` and `from_base64` do the same decoding in code.

### Mounted secret files

For every field marked `#[config(secret)]`, `${KEY}_FILE` is consulted when no source supplied the
value. Setting `SHOP__DATABASE__URL_FILE=/run/secrets/db` makes Moso read that file, trimming one
trailing newline. This is the Docker and Kubernetes convention and it needs no annotation:
`#[config(secret_from = "file")]` only implies `secret`, it does not switch this on.

> [!CAUTION]
> `${KEY}_FILE` is read from the process environment only, never from `.env` or a TOML file. An
> unreadable file logs a `WARN` and the field falls through to its default, which can look like a
> silent success. Give secret fields no default if you want that case to fail the boot.

### Asynchronous secret backends

Implement `SecretProvider` for Vault, AWS Secrets Manager or an internal service:

```rust
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
            // A real provider would call out here.
            Ok(SecretString::from(format!("secret-for-{}", reference.locator)))
        })
    }
}
```

Register it with `App::new(cfg).secret_provider(std::sync::Arc::new(Vault))` and resolve through
`loader.resolve_secret("vault", &reference).await`. Registered providers are also inserted into the
provider map as a `Vec<Arc<dyn SecretProvider>>`, for anything that needs to resolve a secret after
the synchronous load, such as a rotation task.

`ConfigLoader::standard()` and `for_profile()` install `FileSecretProvider` (scheme `file`).
`ConfigLoader::from_sources` installs none, so `resolve_secret` on such a loader always errors, even
for `file`. The `${KEY}_FILE` lookup is a separate path inside the loader and still works on a bare
loader.

## Reloadable values

A `Reloadable<T>` field can be swapped at runtime without a restart.

```rust
#[config(reloadable, default = "info")]
pub log: Reloadable<String>,
```

```rust
let held = config.log.get();          // Arc<String>, never a guard
config.log.set("debug".to_owned());   // the in-flight reader still sees "info"
assert_eq!(*held, "info");
assert_eq!(*config.log.get(), "debug");
```

`get()` is an `ArcSwap` load: a pointer read and a refcount bump with no lock, so a reload cannot
stall a request and a request cannot stall a reload. It returns an `Arc` rather than a guard
specifically so you can never hold a lock across an `.await`. A request that already read the old
value keeps it for its whole life.

`Reloadable::clone` makes an **independent** cell. Two clones do not track each other, which is a
real trap if you clone a config struct expecting shared reload state.

### SIGHUP

```rust
let handle = moso::config::on_sighup(|| {
    // re-read and call `Reloadable::set` on whatever changed
})?;
// dropping `handle` stops the process reacting to SIGHUP
```

`on_sighup` is Unix only; on other platforms it returns an error saying so.

> [!NOTE]
> Installing the listener is your call. `#[config(reloadable)]` records the flag in the descriptor
> and `on_sighup` delivers the signal; the composition root decides which fields to re-read and
> `set`, so a reload does exactly what you wrote and nothing implicit. To reload on `SIGHUP`, wire
> `on_sighup` to `Reloadable::set` in your own composition root. For development, `moso dev` watches
> `config/` and restarts the process rather than hot-swapping a `Reloadable`.

## Loading in tests

A test that loads through the standard stack can be broken by a variable somebody exported in their
shell. Pin the sources instead.

```rust
pub fn defaults() -> Result<Self> {
    Self::load_from(&moso::config::ConfigLoader::from_sources([]))
}
```

A loader with no sources falls straight through to the `#[config(default = …)]` layer, so this is
"give me exactly what the struct says". To supply values, use `MapSource`:

```rust
use moso::config::prelude::*;
use moso::config::{ConfigLoader, MapSource};

/// Everything this application reads from its environment.
#[derive(moso::Config, Debug)]
pub struct AppConfig {
    /// Where the server listens.
    pub bind: SocketAddr,
    /// Connection string; never logged.
    #[config(secret)]
    pub database_url: SecretString,
}

let source = MapSource::from([
    ("bind", "127.0.0.1:0"),
    ("database_url", "sqlite::memory:"),
]);
let loader = ConfigLoader::from_sources([Box::new(source) as _]);
let config = AppConfig::load_from(&loader).expect("a complete configuration");

assert_eq!(config.bind.port(), 0);
```

`MapSource` answers aliases on the dotted key, so a `#[config(env = "RUST_LOG")]` field can be
exercised without touching the process environment.

The test harness can replace the framework's own sections with `TestApp::builder().profile(..)`,
`.http_config(..)`, `.http_config_with(..)`, `.server_config(..)` and `.expose_internal_errors()`.
It cannot edit your own config type, because that value was constructed before `App::new` saw it;
pass an edited value to your `app()` function, or use `override_provider`. See
[testing](./testing.md).

## Writing a custom source

```rust
use moso::config::{ConfigKey, ConfigSource, ConfigValue, Origin, RawValue};

/// Every key answers with the same value.
#[derive(Debug)]
pub struct Constant(pub String);

impl ConfigSource for Constant {
    fn name(&self) -> &str {
        "constant"
    }

    fn get(&self, key: &ConfigKey) -> Option<ConfigValue> {
        Some(ConfigValue::new(
            RawValue::String(self.0.clone()),
            Origin::Env { name: key.to_string() },
        ))
    }
}
```

`name()` is what appears in the source column and the consulted-sources block. Implement
`available()` to return `false` when the backing thing is absent, and it renders as
`constant (not found)`. Implement `keys()` so `ConfigLoader::unused_keys` can see into it. Add the
source with `ConfigLoader::standard()?.with_source(Box::new(Constant("yes".to_owned())))`.

## Framework and battery sections

`HttpConfig`, `ServerConfig`, `TracingConfig` and `TlsConfig` are plain structs with `Default`
implementations, not `#[config(nested)]` sections: `moso-core` cannot depend on `moso-macros`, so it
cannot derive `Config` for its own types. Read the values out of your own config struct and pass them
on.

```rust title="src/lib.rs"
pub fn build() -> Result<App> {
    let config = AppConfig::load_from(&loader()?)?;

    // Read what the server needs before the configuration moves into the
    // builder, where it becomes a provider that handlers reach with `Inject`.
    let bind = config.bind;

    App::new(config)
        .server_config(moso::http_config::ServerConfig {
            bind,
            ..Default::default()
        })
        .mount(routes::router())
        .build()
}
```

The same is true of every battery. `DatabaseConfig`, `KvConfig`, `MailConfig`, `AuthConfig`,
`JobsConfig` and `StorageConfig` are plain structs with builder methods, not `Config` derives,
because the derive generates code that resolves against the `moso` facade and those crates
deliberately do not depend on it. The pattern is to declare your own `#[derive(Config)]` mirror
section and convert: the fields line up one for one and the conversion is a constructor call.

## Tooling

### `moso config`

`moso config` prints every key, the value that won and where it came from. It does not link your
crate: it runs `cargo run --quiet -- --dump-config` and reads one JSON document off stdout. The
`src/dump.rs` that `moso new` writes into your project is what answers.

```text
  profile: dev

  KEY                       ENVIRONMENT                      VALUE                   FROM
  name                      SHOP__NAME                       "shop"                  default
  bind                      SHOP__BIND                       "0.0.0.0:3000"          default
  public_url                SHOP__PUBLIC_URL                 "https://shop.example"  env SHOP__PUBLIC_URL
  database.url              SHOP__DATABASE__URL              ***                     .env DATABASE_URL
  database.max_connections  SHOP__DATABASE__MAX_CONNECTIONS  20                      config/dev.toml:4
```

Secrets are redacted by the application before the CLI sees them, so the CLI cannot leak one into a
terminal recording. A key that no source supplied shows `(not set)` as its value; its origin is
null, and the CLI prints the word `default` in `FROM` for a null origin, so read the value column
rather than the origin column for that case. The command counts those keys and warns at the end,
suggesting `moso config --env-example --out .env.example`. Resolution only: nothing is coerced, so a
key holding an unusable value is shown rather than hidden behind the error it would cause at boot.
Add the global `--json` flag for machine consumption.

The `ENVIRONMENT` column always shows the canonical prefixed spelling. When a well-known alias
supplied the value, the alias appears in `FROM`, as in the `database.url` row above. `FROM` is an
`Origin`, and there are seven of them:

| `Origin` | Rendered |
| --- | --- |
| `Code` | `code` |
| `Cli` | `cli --bind` |
| `Env` | `env SHOP__BIND` |
| `DotEnv` | `.env DATABASE_URL` |
| `File` | `config/production.toml:8`, or the bare path when the line is unknown |
| `ProfileDefault` | `profile default` |
| `Default` | `default` |

One limit on the command: it cannot attribute a value to a per-profile default. `resolve` builds its
lookup without one, because a descriptor does not know which profile is active, so level 7 never
appears in the output. The subcommand accepts `--env-example`, `--out`, `--check`,
`--generate-secret` (with `--format` and `--bytes`) and the shared application arguments, plus the
global `--json`.

### `moso config --check`

Resolves the configuration exactly as the application does (it drives the same `--dump-config` and
`--dump-env-example`) and then reports the mistakes that are *silent*, because the ones that are
not silent already stop the boot with a report naming the key, its type and every spelling that
would have supplied it.

| Finding | Level | What it caught |
| --- | --- | --- |
| `unread_environment_key` | fail | A prefixed variable, in the environment or in `.env`, that no field reads, with a "did you mean" when it is close to one that exists |
| `unread_file_key` | fail | The same typo in `config/default.toml` or the profile's file |
| `env_example_drift` | fail | The committed `.env.example` no longer matches the `Config` type |
| `secret_in_tracked_file` | fail | A `#[config(secret)]` value that came out of a file git tracks |
| `secret_in_file` | warn | The same, in a file git does not track, or with no repository to ask |
| `env_example_missing` | warn | There is no committed example to compare against |

Exit code 1 when anything failed and 0 when only warnings were printed, so `moso config --check` is
a CI gate. `--json` prints `{"ok", "profile", "failures", "warnings", "findings"}`, and every
finding carries a stable `check` slug to branch on.

A key with no value and no default, and a value that fails its type, are *not* in that table on
purpose: both stop the application before it can answer a dump, so `--check` exits non-zero with the
application's own boot report above it rather than reconstructing a worse one. The command cannot
report them from a successful resolution either: a null origin on an application that booted means
the field is `Option<T>`, and calling that missing would be inventing a problem.

### `moso config --generate-secret`

```bash
moso config --generate-secret                        # 32 bytes, base64
moso config --generate-secret --format hex --bytes 64
```

Reads the operating system's random number generator (`/dev/urandom`, and .NET's
`RandomNumberGenerator` through PowerShell on Windows) and prints the encoding you asked for on
standard output and nothing else, so `moso config --generate-secret > /dev/null` writes exactly the
key. The reminder that it must not be committed goes to standard error. There is no `--out`: a
secret that can be redirected into a file is a secret in the repository. Nothing is mixed, stretched
or hashed on the way out; when no generator can be reached the command fails and names
`openssl rand` rather than producing something weaker.

It needs no project: entropy is not configuration, and the first thing a new project needs is the
value that goes in its `.env`. A `SecretBytes` field wants the encoding named, so paste it as
`base64:…` or `hex:…`; a `SecretString` takes it bare.

### Generating `.env.example`

```bash
moso config --env-example --out .env.example
```

The file is regenerated from the descriptor, so it cannot drift from the struct:

```text title=".env.example"
# Human-readable service name, used in logs and the OpenAPI title.
SHOP__NAME=shop

# Where the server listens.
SHOP__BIND=0.0.0.0:3000

# Base URL used to build absolute links in emails and Location headers.  [required]
SHOP__PUBLIC_URL=

# Postgres connection string.  [required]
SHOP__DATABASE__URL=

# Upper bound on pooled connections.
SHOP__DATABASE__MAX_CONNECTIONS=10
```

Doc comments become the comments, defaults become the values, a field with no default gets
`[required]` appended to the last comment line, an explicit `#[config(env = ..)]` alias replaces the
prefixed spelling, and a secret never gets a value whatever its default. Without `--out` the text
goes to stdout. The output is byte-stable, so running it when nothing changed produces no diff,
which is what lets `moso config --check` compare the committed file against the type and exit
non-zero when they disagree.

### The dump protocol

Neither `moso config` nor `moso openapi` links your crate or parses your source. They run
`cargo run --quiet -- --dump-<kind>` and read exactly one document off standard output. The
`src/dump.rs` that `moso new` writes into your project is what answers, which is why you can read
and change it.

| Flag | Standard output |
| --- | --- |
| `--dump-openapi` | The OpenAPI document, as JSON |
| `--dump-routes` | `{"routes": [ .. ]}` |
| `--dump-config` | `{"profile": .., "entries": [ .. ]}` |
| `--dump-env-example` | The text of `.env.example` |

The configuration half is a dozen lines, and it is where the redaction happens:

```rust title="src/dump.rs"
/// Every configuration key, with the value that won and where it came from.
fn config() -> Result<Value> {
    let loader = crate::loader()?;
    let resolved = crate::AppConfig::descriptor().resolve(&loader);

    let entries: Vec<Value> = resolved
        .entries
        .iter()
        .map(|entry| {
            json!({
                "key": entry.key.dotted(),
                "env": entry.key.env_name(crate::ENV_PREFIX),
                "value": entry.value,
                "origin": entry.origin.as_ref().map(ToString::to_string),
                "secret": entry.secret,
            })
        })
        .collect();

    Ok(json!({
        "profile": resolved.profile.to_string(),
        "entries": entries,
    }))
}
```

Everything else the process writes, including logs, warnings and panics, must go to standard error
or the CLI cannot parse the answer. Moso's tracing layer already writes to stderr, so this holds by
default.

### Reaching the descriptor directly

`AppConfig::descriptor()` returns a `&'static ConfigDescriptor` without an instance and without
booting the application. From it:

| Call | Returns |
| --- | --- |
| `descriptor.keys(&ConfigKey::root())` | Every key, flattened through nested sections, in declaration order |
| `descriptor.leaves(&ConfigKey::root())` | The leaf fields paired with their full keys |
| `descriptor.resolve(&loader)` | A `ResolvedConfig`: value, origin and secrecy per key, nothing coerced |
| `descriptor.render_env_example("shop")` | The `.env.example` text |
| `descriptor.render_table(&resolved)` | An aligned `key value origin` table |
| `resolved.defaulted()` | The entries still sitting on a default |
| `loader.unused_keys(&descriptor)` | Keys present in a source that no field consumes |

`unused_keys` is the typo-in-a-committed-TOML detector, and nothing calls it for you. It is a
warning by design: a shared file may legitimately carry keys for a sibling service.

## When configuration is invalid

Boot stops before the server binds and you get every problem at once. The derive reads every field
into a local before the first `?`, so four bad keys are one report rather than four
compile-run-fix cycles.

```text
error: application failed to build (3 problems)

  x missing configuration: database.url
      key          database.url
      type         secret string
      env          SHOP__DATABASE__URL or DATABASE__URL or DATABASE_URL or SHOP__DATABASE__URL_FILE
      file         config/production.toml  ->  database.url = …
      fix          supply it from the environment or the profile's config file
                   export SHOP__DATABASE__URL or DATABASE__URL or DATABASE_URL or SHOP__DATABASE__URL_FILE=…
                   # or, in config/<profile>.toml
                   database.url = …

  x invalid configuration: database.max_connections
      key          database.max_connections
      source       env SHOP__DATABASE__MAX_CONNECTIONS
      expected     integer in 1..=100
      found        0
      fix          set `database.max_connections` to integer in 1..=100

  x 5 configuration sources were consulted, in order
      note         code, cli flags (not found), env, config/production.toml (not found), config/default.toml (not found)
```

Three things to read off it. The `env` line lists every spelling that would have worked, including
`${KEY}_FILE` for a secret. The `source` line names where the bad value came from, down to the file
and line for TOML. The last block is the whole stack in precedence order with `(not found)` markers,
because "where would this value have come from" is the first question a configuration error raises.

A value that failed to coerce (as opposed to one that failed a range check) also gets a
`note  also settable as …` line listing the other spellings, in case the one you edited was not the
one that won.

A `found` value is `***` when the field is secret: a boot report is written to a log that outlives
the process. A `#[config(range = ..)]` violation is checked after coercion, so `POSTS__PAGE_SIZE=0`
is a boot error naming the key rather than a runtime division by zero.

A TOML file that does not exist is an empty, unavailable source. A TOML file that exists and does
not parse is a hard error naming the file and the line. A malformed `.env` line is skipped with a
`WARN` rather than failing the boot.

## Failure modes worth knowing

- **`--check --json` sets neither key.** CLI parsing treats `--key value` as a pair only when the
  next argument does not start with `-`. Conversely, a subcommand's own option can be captured:
  `worker --queue emails` leaves `worker` alone but does record `queue = "emails"`. Use `--set` when
  you want to be unambiguous, and `--` to stop parsing.
- **An empty value means unset**, in the environment, in `.env` and for `Option<T>` fields. `FOO=`
  cannot set an empty string, and a `Vec<T>` from an empty variable is an empty vector rather than
  one element containing `""`.
- **A `SecretString` in a struct that derives `Serialize` breaks serialisation of the whole struct**,
  by design. Add `#[serde(skip)]`.
- **`OverrideSource::set` takes `&mut self`**, unlike the other builders here, so it needs a `let mut`
  binding. Setting the same key twice replaces the value rather than shadowing it.
- **`ConfigLoader::from_sources` registers no secret providers**, so `resolve_secret` on a loader
  built for a test fails for every scheme, `file` included.
- **`#[derive(Config)]` is expensive to expand.** It currently costs 21.6 to 61.0 lines of generated
  code per field against a 20-line internal budget, which the workspace's own `expand-size` check
  fails on. It affects compile time, not behaviour.
- **`moso new` does not scaffold a `config/` directory.** The TOML layers work; create the directory
  and the files yourself if you want them.
- **`Profile::detect` treats any binary whose parent directory is named `deps` as a test harness.**
  That is how `cargo test` gets `Profile::Test`, and it misfires for anything else run from a
  directory called `deps`. Set `MOSO_PROFILE` when you care.
- **Feature flags are a `Config` bool by design.** Moso has no dedicated `flags!` macro, `Flags`
  type or `FlagSource`; a boolean config field, optionally wrapped in `Reloadable<bool>` for
  runtime toggling, is the intended way to gate a feature.

## See also

- [Errors](./errors.md) for how the boot report relates to the runtime error model.
- [Dependency injection](./dependency-injection.md) for reaching the config from a handler.
- [Security](./security.md) for the secret canary test and key rotation.
- [Observability](./observability.md) for `TracingConfig` and `LogFormat`.
- [Project layout](../start/project-layout.md) for where `src/config.rs` and `src/dump.rs` live.
