# 53 — Quality Gates, Benchmarks & Stability Policy

> ⛔ **Status: every gate is currently a convention, not a gate.** `cargo fmt --check`,
> `cargo clippy --all-targets -- -D warnings`, `#![deny(missing_docs)]` and `#![forbid(unsafe_code)]`
> all hold, and are checked by hand. **There is no `.github/` directory, no CI configuration, no
> `deny.toml`, no `rust-toolchain.toml`, no benchmark suite and no semver checking.** Until WP-00 is
> finished, nothing in this document is enforced by anything.

## The gate table

Every gate is automated and blocks a merge or a release. A gate that requires a human to remember is
not a gate.

| # | Gate | Blocks | Tool |
| --- | --- | --- | --- |
| G1 | `cargo fmt --check`, `clippy -D warnings` | merge | CI |
| G2 | All tests pass on Linux/macOS/Windows, stable + MSRV | merge | nextest |
| G3 | Feature powerset builds (`cargo hack --feature-powerset --depth 2`) | merge | cargo-hack |
| G4 | UI corpus matches exactly | merge | trybuild |
| G5 | Structural properties of every crate: both lints declared, opted into and restated in order; no `unsafe`; a `//!` on every file; no banned direct dependency; registry metadata present | merge | `xtask check-crates` |
| G6 | `#![deny(missing_docs)]`; `cargo test --doc` passes | merge | CI |
| G7 | Public traits all carry `on_unimplemented` | merge | `xtask check-diagnostics` |
| G8 | No foreign type in `moso-orm`/`moso-sql` public API | merge | `xtask check-sealed` |
| G9 | Crate dependency edges respect the layering rules | merge | `xtask check-deps` |
| G10 | Dependency count within budget (90 default / 260 full) | merge | `xtask check-deps` |
| G11 | `cargo deny check` (licences, advisories, bans, duplicates) | merge | cargo-deny |
| G12 | Compile-time budgets not regressed > 5% | merge | `xtask bench-compile` |
| G13 | Runtime benchmarks not regressed > 5% | merge | criterion + `xtask bench` |
| G14 | Derive expansion size within budget | merge | `xtask expand-size` |
| G15 | OpenAPI meta-schema validation of the reference apps | merge | CI |
| G16 | Canary-secret grep over all test output | merge | CI |
| G17 | `examples/realworld` passes the RealWorld API suite | merge | CI |
| G18 | Semver check against the previous release | release | cargo-semver-checks |
| G19 | Changelog entry present for every user-visible change | merge | CI |
| G20 | Fuzz targets clean over a nightly run | nightly | cargo-fuzz |
| G21 | External security review resolved | 0.1 release | manual, recorded |
| G22 | Binary size within budget | release | `xtask` |

## Benchmarks

All benchmarks live in-repo, are runnable by anyone with one command, and publish their machine
specification alongside results. **We publish where we lose.** A benchmark page that shows only
wins is correctly read as marketing.

### Runtime (`xtask bench --runtime`)

| Scenario | Compared against | Target |
| --- | --- | --- |
| JSON echo (no DB) | hand-written Axum | ≤ 5% p99 latency overhead, ≤ 10% throughput |
| Validated JSON body → typed response | Axum + serde + validator + utoipa | ≤ 5% |
| Single-row DB read | hand-written sqlx | ≤ 15% |
| List + preload (100 parents, 1000 children) | SeaORM, Diesel, hand sqlx | equal statement count; ≤ 15% time |
| Dynamic 5-filter query construction | SeaORM, Diesel | ≤ 2 µs construction |
| Session-authenticated request | axum-login + tower-sessions | ≤ 10% |
| Full middleware stack overhead | bare router | ≤ 3 µs/request |
| DI resolution (2 injects + 1 dependency) | — | ≤ 200 ns |
| Authorization check | — | ≤ 1 µs |
| Job enqueue + execute (Postgres) | — | ≥ 1000/s sustained |
| Idle RSS, reference app | — | ≤ 30 MB |
| Boot time, 200 endpoints | — | ≤ 100 ms incl. OpenAPI assembly |

Cross-framework comparisons use the **RealWorld** implementation so the workload is identical and
independently specified, plus a JSON-echo microbenchmark for the framework floor. Load generation
with `oha`/`k6`, three runs, median reported, full raw data committed.

### Compile time (`xtask bench-compile`)

Budgets are in `04-devex/42-compile-times.md § Budgets`. Tracked per commit and plotted on a public
dashboard, because a slow, invisible drift is how frameworks become painful.

## Stability policy

### Before 1.0
- Breaking changes may occur in minor versions (0.x → 0.y), but **only** with: a migration guide, a
  codemod (`moso migrate`) where mechanically possible, and at least one release of deprecation
  warning where the old API can coexist.
- The bar rises each release. By 0.3, a breaking change requires an RFC.

### After 1.0
- Semver, strictly, enforced by `cargo-semver-checks` in CI.
- **Public dependency majors are part of the contract.** `axum`, `tower`, `http`, `sqlx`, `serde`,
  `tokio` appear in Moso's public API; bumping any of their majors is a Moso major. They are
  re-exported under `moso::deps::*` so users never add them directly and never hit a version
  mismatch. This directly addresses the axum 0.6→0.7 forced-hyper-1.0 upgrade that our target users
  remember.
- LTS: one release line supported for 18 months with security and critical fixes.
- Deprecations live for two minor versions minimum before removal.

### MSRV
Stable minus two releases (~12 weeks of leeway). Bumped only in minor versions, always noted in the
changelog, always tested in CI. Never bumped in a patch.

### `#[non_exhaustive]` discipline
Applied to every public enum and every struct users construct with a literal, unless the type is
deliberately closed and documented as such. Getting this wrong is the most common cause of
accidental breaking changes in Rust libraries.

## Release process (`xtask release`)

1. All gates green on `main`.
2. Version bumped in lockstep across every crate; internal deps pinned `=x.y.z`.
3. Changelog assembled from labelled PRs, hand-edited for narrative.
4. `cargo semver-checks` against the previous release.
5. `cargo publish --dry-run` for every crate in dependency order.
6. Tag, sign, publish; SLSA provenance attestation via CI OIDC.
7. Prebuilt CLI binaries built and published for all targets.
8. Docs site deployed for the new version; the version switcher updated.
9. Release notes posted; the announcement includes benchmark deltas and any known regressions.

Releases are **never** cut by hand, and never from a laptop.

## Incident response

For a bug that corrupts data, leaks data across tenants, or bypasses authorization:

1. Reproduce and write a failing test **first**.
2. Assess blast radius: which versions, which configurations, what evidence a user could look for.
3. Patch the current minor and the previous minor.
4. Publish an advisory (GHSA + RUSTSEC) with detection guidance, not just a fix version.
5. Post-mortem in `docs/postmortems/`, public, blameless, with the systemic fix — usually a new
   gate in the table above.

Publishing post-mortems is unusual for a framework and is worth doing: it is the strongest available
signal that the project is run by adults.

## The quality dashboard

A public page, updated per commit: compile-time trend, runtime benchmark trend, binary size, test
count and coverage, dependency count, open `confusing-error` issues, docs coverage, and days since
the last release. Making these visible to the community is both accountability and marketing —
and it makes regressions socially expensive, which is more effective than any policy.
