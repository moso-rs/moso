# RFC-0001 - Four dependency-budget cuts behind off-by-default features

Status: Draft
Date: 2026-08-19
Author: Alessandro Zucchiatti

## Context

`xtask check-deps` rule 6 - the third-party crate-count budget - is red, and
deliberately so. The budget is `DEFAULT_BUDGET = 90` and `FULL_BUDGET = 260`
(`xtask/src/deps.rs`), and the current resolution is **default 141 / full 292**.
AGENTS.md records this as a known-red that CI reports without enforcing
(`budget_advisory`), with one standing instruction: **close it by removing
crates, never by raising the number.** ADR-0019 restates the same discipline
from the other side - the vendored Swagger UI adds ~1.6 MB of binary but *zero*
crates, so it left rule 6 untouched by construction.

Two kinds of cut exist. The **safe** kind hides crates behind an off-by-default
feature without touching any public signature or the layout `moso new` emits;
those need no RFC. The ICU cut (call it A1) already landed there. But the "safe
batch A2-A5" that an earlier estimate leaned on has mostly evaporated on
inspection: **A2 (dropping `moka`) and A3 (gating `chrono-tz`) turned out to be
breaking**, so they reappear in this RFC as B3 and B4; the only genuinely-safe
remainder is **A4** (making the `sqlx` backend selectable - additive, zero crate
reduction) and **A5** (a `getrandom` version hygiene fix - zero crates, already
applied). The honest consequence, quantified in the bottom-line table, is that
the "safe" path saves **~0 crates**, and the full budget is unreachable without
the breaking cuts below.

This RFC therefore covers **four** cuts that cannot be made silently, because
each either changes a public signature or changes what `moso new` generates.
AGENTS.md is explicit that "an RFC is required before code for: any breaking
change ... and anything that changes the layout `moso new` generates." All four
clear that bar, so they are **one approval**, proposed here for the owner rather
than implemented. None weakens a documented security default; B2's relationship
to the cookie defaults is spelled out in full below.

- **B1** and **B2** pay off in the *default* build (`toml`, the AES-GCM stack).
- **B3** and **B4** pay off only in `full` (`moka`, `chrono-tz`), because the
  crates they remove reach the graph solely through the `kv`, `auth` and `jobs`
  batteries, all of which are off in a default build.

The framework is still unpublished (`0.0.1`, nothing on crates.io - ADR-0018),
which is the cheapest possible moment to make a breaking change: there is no
downstream to migrate and no contributor's assent to gather. The same one-way-
door logic ADR-0018 applied to the licence applies here in reverse - do the API
narrowing now, while it costs nothing.

## Proposal

### B1 - a `config-file` feature that makes `toml` optional

`toml` is a default, unconditional dependency of `moso-core`
(`crates/moso-core/Cargo.toml:119`, `toml.workspace = true`), pulled in by the
TOML config layer. It drags in a six-crate subtree that ships in **every** build,
default included. Verified with `cargo tree -i toml -p moso -e normal` and
`cargo tree -p toml -e normal`:

```text
toml
├── serde_spanned
├── toml_datetime
├── toml_parser ─→ winnow
├── toml_writer
└── winnow
```

That is `toml`, `serde_spanned`, `toml_datetime`, `toml_parser`, `toml_writer`,
`winnow` = **6 crates** (`serde_core` is shared and not counted).

The `toml` surface in `moso-core` is confined to one file,
`crates/moso-core/src/config/source.rs`, and every mention of `toml::Value` is
in a **private** function body or a private struct field - none of it is in a
public signature:

- `pub struct TomlSource` (line 573) with a private `root: Option<toml::Value>`
  field (line 575);
- `impl TomlSource` constructors calling `toml::from_str` (lines 610 and 632);
- the free helpers `fn walk` (659), `fn from_toml` (668) and `fn flatten` (688),
  all `toml::Value` in and out;
- `impl ConfigSource for TomlSource` (757);
- the mention of `TomlSource` in the module-doc list of built-in sources (line
  42).

**Change.** In `crates/moso-core/Cargo.toml`, make the dependency optional
(`toml = { workspace = true, optional = true }`) and add an off-by-default
feature:

```toml
# File-based configuration: the `TomlSource` layer that reads config/*.toml.
# OFF by default so the base tree carries no TOML parser (6 crates). A build
# that loads config from files turns it on; env / CLI-arg / .env / #[config(
# default=…)] layers work with it off. `moso new` enables it (see Open questions).
config-file = ["dep:toml"]
```

Then gate `TomlSource`, its `impl`s and the three free helpers behind
`#[cfg(feature = "config-file")]`, and drop `TomlSource` from the built-in-source
doc list when the feature is off. Because `toml::Value` never escapes a private
body, the gate is a clean cut - no downstream type changes shape.

**What still works with the feature off.** Configuration itself is untouched:
the env layer (`EnvSource`), the `.env` layer (`DotEnvSource`), the CLI-argument
layer (`CliSource`), overrides (`OverrideSource`) and `#[config(default = …)]`
all remain. Only *reading config from a `.toml` file* moves behind a flag.

**`moso new`.** Turn the feature on in the generated project so a fresh app has
file-based config out of the box, by adding `config-file` to the `moso`
dependency's feature list in `crates/moso-cli/templates/new/Cargo.toml.tpl`
(the `@@MOSO_DEP@@` expansion). The six crates then land in the *user's* app,
which opted in, and stay out of the default facade resolution that rule 6
measures.

**Savings: default -6, full -6** (the subtree is unconditional today, so both
resolutions drop by the same six; `config-file` is not added to the facade
`full` bundle).

**Why it needs an RFC.** Two independent triggers: (1) `pub struct TomlSource`
becomes feature-conditional - a breaking change to `moso-core`'s public API;
(2) it changes what `moso new` generates. AGENTS.md gates both.

### B2 - a `private-cookies` feature that drops the AES-GCM stack

`moso-core` enables `cookie/private` (encrypted, authenticated cookies) in every
build via `crates/moso-core/Cargo.toml:97`:

```toml
cookie = { workspace = true, features = ["signed", "private", "key-expansion"] }
```

That `private` feature is the **sole** consumer of an AEAD stack. Verified with
`cargo tree -i aes-gcm -p moso` and `cargo tree -p aes-gcm -e normal`:

```text
aes-gcm
├── aead
├── aes ─→ cipher, cpufeatures
├── cipher ─→ inout
├── ctr
└── ghash ─→ polyval ─→ universal-hash, opaque-debug
```

That is `aes-gcm`, `aead`, `aes`, `cipher`, `ctr`, `ghash`, `polyval`,
`universal-hash`, `inout`, `opaque-debug` = **10 crates**, in every build.

**Change.** In `crates/moso-core/Cargo.toml:97`, drop `"private"` from the
`cookie` feature list, leaving `["signed", "key-expansion"]`, and add an
off-by-default feature:

```toml
# Encrypted (AES-GCM) cookies on top of the signed default. OFF by default: the
# AEAD stack is 10 crates and the signed default already authenticates. A build
# that stores confidential values in a cookie turns it on.
private-cookies = ["cookie/private"]
```

Gate the public encrypted-cookie surface in
`crates/moso-core/src/extract/cookies.rs` behind
`#[cfg(feature = "private-cookies")]`:

- `pub fn private(&self) -> PrivateCookies` (line 380);
- `pub fn try_private(&self) -> Result<PrivateCookies>` (line 399);
- `pub struct PrivateCookies(CookieView)` (line 746) and its `impl` (line 748).

Re-export it as an off-by-default facade feature in `crates/moso/Cargo.toml`
(`private-cookies = ["moso-core/private-cookies"]`), and - see Open questions -
keep it out of the `full` bundle so the full budget benefits too.

**Savings: default -10, full -10** - and the net is honestly **10, not ~16.**
Two families of crates that *look* like they might go, stay:

- **`chacha20` stays.** It is not pulled by `cookie` at all. `cargo tree -i
  chacha20 -p moso -e normal` shows it arriving through `rand → ulid →
  moso-core`, unrelated to cookies. `chacha20poly1305` is not in the graph.
- **The shared hash crates stay.** `signed` cookies are HMAC-SHA256, so
  `digest`, `hmac`, `sha2`, `crypto-common`, `generic-array`, `typenum`,
  `subtle`, `cpufeatures` and `zeroize` are still needed. Only the crates unique
  to the AEAD path leave.

**Why it needs an RFC - and why there is no security veto.** Gating
`Cookies::private`, `Cookies::try_private` and `PrivateCookies` off by default is
a breaking change to the public API, which is the trigger. It is **not** a
weakening of a documented security default: AGENTS.md's documented cookie
posture is "HttpOnly + Secure (prod) + SameSite=Lax + **signed**" (HMAC-SHA256
with HKDF domain separation), and that default is entirely untouched - `signed`
and `key-expansion` stay on. Encrypted/private cookies were always an *extra*
on top of the signed default, never the default themselves. So no security-owner
veto applies; the RFC requirement comes solely from the API break.

### B3 - make the in-memory KV cache (`moka`) opt-in

`moso-kv`'s default `memory` backend is `moka`
(`crates/moso-kv/Cargo.toml`: `default = ["memory"]` at line 23,
`memory = ["dep:moka"]` at line 24). `moka` drags in a seven-crate subtree that
is unique to it. Verified with `cargo tree -i moka -p moso --features full`, and
each of the seven confirmed sole-consumed by `moka` in the `full` graph with
`cargo tree -i <crate> -p moso --features full -e normal`:

```text
moka, tagptr, crossbeam-channel, crossbeam-epoch,
async-lock, event-listener-strategy, portable-atomic   = 7 crates
```

(`moka`'s other transitive deps - `crossbeam-utils`, `parking_lot`, the
`futures-*` family, `syn`, `smallvec`, … - are shared with the rest of the
`full` tree and stay; only these seven leave.)

`moka` is **absent from the zero-feature default build** (`cargo tree -i moka -p
moso` errors - no such package), because the `kv`, `auth` and `jobs` batteries
are all off by default. It reaches `full` through **three** edges, all pulling
`moso-kv` with its default `memory` feature: the facade's own `kv` feature
(`crates/moso/Cargo.toml:110` `kv = ["dep:moso-kv"]`, dependency at line 163 with
default features on), `moso-auth` (`crates/moso-auth/Cargo.toml:48`
`moso-kv.workspace = true`), and `moso-jobs` (`crates/moso-jobs/Cargo.toml:46`).

**Change.** Stop all three edges from inheriting `moso-kv`'s default backend,
while `moso-kv` itself keeps `default = ["memory"]` so it stays usable
standalone (a promise `docs/02-data/25-kv-cache.md` makes):

- `moso-auth` (line 48): `moso-kv = { workspace = true, default-features = false }`,
  and add a dev-dependency `moso-kv = { workspace = true, features = ["memory"] }`
  so its `#[cfg(test)]` code that calls `moso_kv::Kv::in_memory(..)` still
  compiles (`moso-auth` has no such dev-dependency today - its `[dev-dependencies]`
  at line 141 carries only `moso-migrate`, `tokio`, `serde_json`).
- `moso-jobs` (line 46): `moso-kv = { workspace = true, default-features = false }`;
  it **already** has the memory dev-dependency (line 70), so its tests are covered.
- `moso-jobs` `jobs-memory` feature (line 32, currently `jobs-memory = []`):
  wire it to `["moso-kv/memory"]` so turning the in-memory broker on turns the
  backend on.
- The facade: `moso-kv` dependency `default-features = false`, and a passthrough
  feature `kv-memory = ["moso-kv/memory"]` (plus `jobs-memory` passthrough) so an
  app opts the in-process backend back in explicitly. `full` deliberately does
  **not** enable it, which is what lets the seven crates leave `full`.

**Why it is breaking - two triggers.** For a jobs-enabled build to actually shed
`moka`, `jobs-memory` must leave `moso-jobs`'s default feature set
(`crates/moso-jobs/Cargo.toml:23` `default = ["jobs-pg", "jobs-memory"]` becomes
`default = ["jobs-pg"]`). (1) Removing a default feature is SemVer-breaking.
(2) It changes what `moso new` generates: a jobs app no longer ships a working
in-memory broker by default - it requires Postgres (or Redis via `jobs-redis`).
The `moso-jobs` manifest comment currently justifies `jobs-memory` being on by
default precisely because "a test suite that needs a broker is a test suite
people stop running"; that concern is met by the dev-dependency (tests keep their
broker), but the **runtime** default genuinely changes, which is the honest cost.

**Savings: default 0, full -7.** `moka` was never in the default; the whole
reduction is in `full` and in any app that turns on `kv` / `auth` / `jobs`.

### B4 - gate the IANA timezone database (`chrono-tz`) behind `jobs-tz`

`moso-jobs` depends on `chrono-tz` unconditionally
(`crates/moso-jobs/Cargo.toml:57`,
`chrono-tz = { version = "0.10.4", default-features = false, features = ["std"] }`),
used only by the cron scheduler in `crates/moso-jobs/src/cron.rs` (lines 234, 250
and 269). It pulls a four-crate subtree. Verified with
`cargo tree -i chrono-tz -p moso --features full`, each sole-consumed by
`chrono-tz`:

```text
chrono-tz, phf, phf_shared, siphasher   = 4 crates
```

(`chrono` itself is shared - `moso-jobs` uses it directly - so it stays; only
these four leave.) Like `moka`, `chrono-tz` is **absent from the default build**
(the `jobs` battery is off by default).

**Change.** Make the dependency optional
(`chrono-tz = { version = "0.10.4", optional = true, default-features = false, features = ["std"] }`),
add an off-by-default feature `jobs-tz = ["dep:chrono-tz"]`, and gate the
timezone-parsing path in `cron.rs` behind `#[cfg(feature = "jobs-tz")]`. With the
feature off, `Cron::timezone` accepts only UTC (a fixed schedule), which is the
common case; a schedule that must follow a named zone's clock changes turns
`jobs-tz` on. Add a facade passthrough (`jobs-tz = ["moso-jobs/jobs-tz"]`), off
in `full`.

**Why it is breaking.** `pub struct Timezone(chrono_tz::Tz)` at
`crates/moso-jobs/src/cron.rs:234` is **public API**, and `Timezone::parse`
(line 250) and `Timezone::utc` (line 269, `chrono_tz::UTC`) are public methods on
it. Putting `Timezone` behind an off-by-default feature makes a public type
feature-conditional - a SemVer break - and loses tz-aware cron by default. Same
governance class as B1/B2; not a free cut.

**Savings: default 0, full -4.**

## Bottom line

The figures below are cumulative and start from the post-A1 (ICU cut) reading.
All four rows B1-B4 are what this RFC asks the owner to approve, as one decision.
Every per-cut delta below was verified directly from the reverse-dependency graph
(`cargo tree -i`); the totals are the rule-6 reading they move.

| Configuration | default crates | full crates |
| --- | --- | --- |
| Today (post-A1 ICU cut) | 141 | 292 |
| + B1 `config-file` optional (default -6) | 135 | 286 |
| + B2 `private-cookies` optional (default -10) | 125 | 276 |
| + B3 `moka` opt-in (full -7, **breaking**) | 125 | 269 |
| + B4 `chrono-tz`/`jobs-tz` opt-in (full -4, **breaking**) | 125 | 265 |
| A4 `sqlx` backend selectable (additive) | 125 | 265 |
| A5 `getrandom` hygiene (applied) | 125 | 265 |
| **Budget (rule 6)** | **90** | **260** |

**The honest reckoning:** even with all four breaking cuts applied, **default
lands at 125 (35 over the 90 budget) and full at 265 (5 over the 260 budget) -
neither budget is reached.** B1+B2 take 16 crates off `default`; B3+B4 take a
further 11 off `full`. The genuinely-safe leftovers of the old "safe batch" -
A4 (make the `sqlx` backend selectable) and A5 (a `getrandom` version fix,
already applied) - remove **zero** crates from either resolution, so the earlier
"safe batch -> full ~278" estimate was wrong: the real `moka` + `chrono-tz`
savings (-11 full) exist only *behind* the breaking changes B3 and B4. Closing
rule 6 outright will need cuts beyond these four; this RFC is the largest honest
step available without touching an abstraction the framework promises.

## Alternatives considered

**B1 - keep `toml` always-on.** Accept the 6 crates in every build in exchange
for zero-configuration file-based config. Rejected: those crates are on the
default critical path, file-based config is not universal (a twelve-factor
service configures from the environment), and the cut is clean because
`toml::Value` never appears in a public signature. Making it opt-in costs the
file-config user one feature flag - which `moso new` sets for them anyway.

**B1 - split reader and writer, or hand-roll a minimal TOML parser.** Would trim
`toml_writer` or `winnow` while keeping file config always-on. Rejected: a
bespoke parser is a maintenance liability and a correctness risk for a
first-floor concern, and half-cutting the subtree keeps most of the crates for
most of the complexity. All-or-nothing behind one flag is simpler and honest.

**B2 - keep `private` cookies always-on.** Accept the 10-crate AEAD stack in
every build so `Cookies::private()` is unconditionally available. Rejected: the
signed default already authenticates cookies, encryption is needed only when a
cookie must also be *confidential* (a minority case), and 10 crates is a large
toll to levy on every user for a feature most never call. Opt-in matches who
pays to who benefits.

**B2 - swap AES-GCM for a ChaCha20-Poly1305 AEAD to reuse `chacha20`.** Since
`chacha20` is already in the tree via `ulid`, a ChaCha-based private cookie might
share it. Rejected: `cookie`'s `private` feature is AES-GCM and not
configurable, reusing `chacha20` would still pull `chacha20poly1305` +
`universal-hash` + `aead` (not currently present), and re-implementing cookie
encryption to chase a shared crate is exactly the "never implement a
cryptographic primitive" line AGENTS.md draws. The clean move is to gate the
existing, audited AES-GCM path, not to reinvent it.

**B3 - keep `moka` in `moso-jobs`/`moso-auth` defaults.** Leave the in-memory
broker on by default, accept the 7 crates in every `full`/`kv`/`auth`/`jobs`
build, and avoid the SemVer break. Rejected because it was the "safe A2" that
turned out not to be safe: there is no way to remove `moka` from `full` without
removing it from a default feature set, and doing that is breaking either way -
so the choice is between an honest breaking change now (pre-publish, free) and
paying 7 crates forever. Postgres is the production job broker regardless; the
in-memory one is a test and local-dev convenience that a dev-dependency and a
one-line feature preserve.

**B3 - keep `jobs-memory` in the default but drop `moka` for a std-only broker.**
Re-implement the in-memory queue over `std`/`tokio` primitives instead of `moka`.
Rejected: it is real concurrent-datastructure work to replace an audited cache
for a convenience backend, and it still leaves `jobs-memory` semantics subtly
different from the crate it replaced. Gating the existing backend is smaller and
truthful.

**B4 - keep `chrono-tz` always-on.** Accept the 4 crates so `Timezone` and
tz-aware cron are unconditional. Rejected for the same shape as B1: most cron
schedules are fixed or UTC, the IANA database is a large table to bundle for the
minority that needs `Europe/Rome` to survive a clock change, and the cut is clean
because it is one feature and one public type. Unlike B1/B2 it saves nothing on
`default` (jobs is off there), but it is 4 crates off every `full` build.

## Consequences

- **Four public-API / generated-layout breaks, at the cheapest possible moment.**
  `TomlSource` (`moso-core::config`), `Cookies::private` / `try_private` /
  `PrivateCookies` (`moso-core::extract`) and `Timezone` (`moso-jobs::cron`)
  become feature-conditional; `moso-jobs`'s `default` loses `jobs-memory`. Because
  nothing is published (ADR-0018), there is no ecosystem to migrate; a
  hand-written `Cargo.toml` that relied on an always-on surface adds the matching
  feature (`config-file`, `private-cookies`, `kv-memory` / `jobs-memory`,
  `jobs-tz`). `moso new` sets `config-file`, so the common path needs no action.
- **-16 crates on `default` (141 -> 125) and -27 on `full` (292 -> 265).** The
  AGENTS.md discipline is honoured: the budget moves by cutting crates, not by
  raising the number. It is not enough on its own - see the bottom-line reckoning
  - but it is the largest honest step currently available.
- **A jobs app's default runtime changes.** With `jobs-memory` out of
  `moso-jobs`'s default, a generated jobs app needs Postgres (or `jobs-redis`) to
  run its broker; the in-memory broker is one feature away and is still wired for
  every test suite via a dev-dependency. This is the one behaviour change a user
  will actually notice, and it is called out in Open questions.
- **No security default weakens.** Signed cookies (HMAC-SHA256 + HKDF) remain the
  default and remain on; file-based config, the KV backend and the timezone
  database carry no security posture. B2 is an availability change to an *extra*,
  not a default.
- **The docs move in the same PR as the code.** `docs/00-foundations/03-crate-
  layout.md`, `docs/02-data/25-kv-cache.md`, `docs/03-batteries/32-jobs.md` and
  the implementation-status ledger describe the new feature topology; each new
  feature is documented where every other feature already is (its `[features]`
  table), so there is no second home to keep in sync.
- **Four more feature flags.** `config-file`, `private-cookies`, the
  `kv-memory` / `jobs-memory` passthroughs and `jobs-tz` join the off-by-default
  set. Each is named for exactly what it gates and follows the established pattern
  (`subscriber`, `compression`, `cors`, …) of a feature whose comment states the
  crate cost it defers.

## Open questions

These are for the owner to decide; the RFC does not implement anything until they
are answered. B1-B4 are proposed as **one approval** - each is a breaking change
of the same governance class - but each can be accepted or held independently.

- **(a) Approve B1** - `config-file = ["dep:toml"]`, `toml` optional,
  `TomlSource` and its helpers gated - as specified?
- **(b) Approve B2** - drop `cookie/private` from the default,
  `private-cookies = ["cookie/private"]`, the encrypted-cookie surface gated - as
  specified?
- **(c) Approve B3** - `moka` opt-in, `moso-kv` default backend dropped from the
  `auth`/`jobs`/facade edges, `jobs-memory` out of `moso-jobs`'s default and
  wired to `moso-kv/memory` - as specified?
- **(d) Approve B4** - `chrono-tz` optional behind `jobs-tz`, `Timezone` and the
  cron tz path gated - as specified?
- **(e) Should `config-file` be ON in the `moso new` template?** *Recommended:
  yes.* A generated app is expected to read `config/*.toml` out of the box, and
  the 6 crates then land only in the user's project, not in the default facade
  resolution rule 6 measures.
- **(f) Should the facade keep any of `private-cookies`, `kv-memory` /
  `jobs-memory` or `jobs-tz` in a default bundle such as `full`?** *Recommended:
  no.* Keeping all four opt-ins out of `full` is exactly what lets the full budget
  benefit from the -27; a project that needs encrypted cookies, an in-process KV
  backend or named-timezone cron opts in explicitly. (`full` keeping them would
  forfeit the B2/B3/B4 savings.)
- **(g) Is the jobs-app runtime default change acceptable?** *Recommended: yes.*
  `moso-jobs`'s `default` becomes `["jobs-pg"]`; a generated jobs app requires
  Postgres (or `jobs-redis`) for its broker, with the in-memory broker one
  feature away and still wired for every test suite. This is the only
  user-visible behaviour change in the four cuts, so it is called out on its own.
