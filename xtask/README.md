# `xtask` - Moso's measurement and gate harness

Every claim in `docs/` with a number in it is either measured here or it is a
wish. Run the whole set the way CI does:

```
cargo run -p xtask -- ci            # add --fast to skip test, doc and clippy
```

There is no `cargo xtask` alias yet: `.cargo/config.toml` does not exist in this
repository. When it lands, add

```toml
[alias]
xtask = "run --package xtask --"
```

and every command below can drop `run -p xtask --`.

## The seven subcommands

| Command | What it measures or enforces |
| --- | --- |
| `bench-compile` | the edit loop: seven scenarios, p50/p95 over N runs, diffed against a committed baseline |
| `expand-size` | lines of code each macro emits, against the budgets in `docs/06-reference/62-macro-reference.md` |
| `check-sealed` | that no foreign type is reachable from `moso-sql`'s or `moso-orm`'s public API (ADR-0005) |
| `check-deps` | the six dependency rules in `docs/00-foundations/03-crate-layout.md` and the crate-count budget |
| `check-diagnostics` | that every public trait carries `#[diagnostic::on_unimplemented]` |
| `release` | version bump in lockstep, changelog assembly, publish dry-run in dependency order, tag |
| `ci` | all of the above, in the order CI runs them, with a summary table |

Exit codes: `0` everything passed, `1` a gate failed, `2` the harness itself
could not run. The two are separated because they need different people.

## `bench-compile`

```
cargo run -p xtask -- bench-compile --runs 5 --json target/compile-bench.json
cargo run -p xtask -- bench-compile --scenarios handler-body-edit,new-endpoint
cargo run -p xtask -- bench-compile --update-baseline      # after a deliberate change
```

Scenarios: `cold-build`, `handler-body-edit`, `check-handler-edit`,
`new-endpoint`, `new-entity`, `cargo-toml-touch`, `test-build`.

Three things worth knowing before reading a number:

- **It never edits the checkout.** The sources are copied to
  `target/xtask/bench-ws` and every edit happens there, so the benchmark cannot
  corrupt a working tree and cannot fight another `cargo` for the build lock.
- **`cold-build` cleans the Moso crates, not the world.** The budgets in
  `docs/04-devex/42-compile-times.md` are stated for a warm cargo cache.
- **Every scenario runs once unmeasured first**, in its own mode, because
  `cargo check`, `cargo build` and `cargo test` keep different artefacts and the
  first run would otherwise pay for whatever the previous scenario deleted.

The baseline is `xtask/bench/compile-baseline.json`. It records the host triple,
and a baseline from a different host is reported but not enforced - comparing an
arm64 laptop with the x86\_64 reference machine would fail every PR made on the
wrong one.

## `check-sealed`

```
cargo run -p xtask -- check-sealed --self-test
```

Reads `cargo rustdoc --output-format json` and fails when a path from outside
`moso_*`/`std`/`core`/`alloc` plus `xtask/allow/sealed.toml` appears in a public
signature - a parameter, a return type, a public field, an alias target, a
bound, an associated type's value, a re-export, or a generic argument of an
implemented trait.

`--self-test` proves the check works, in both directions, against
`xtask/fixtures/`: `leaky-sql` puts a stand-in query engine into eleven public
positions and must be caught; `sealed-sql` wraps the same engine correctly and
must come back clean. A checker with no false negatives is easy - flag
everything - so the second fixture is the one that makes the gate keepable.

`moso-sql` and `moso-orm` do not exist yet; the gate warns and skips.

## `check-diagnostics`

```
cargo run -p xtask -- check-diagnostics
```

`xtask/allow/diagnostics.toml` has two tables. `[[exempt]]` silences a trait and
needs a reason - reserve it for traits no user can name. `[[known_gap]]` records
a trait that should have a message and does not; it does **not** silence the
failure, it only turns "unknown trait" into "known, here is the fix".

## `release`

Nothing writes without `--write`. `release publish` is a dry run and prints the
real commands rather than uploading anything: an `--execute` flag that pushes
eight crates to a permanent registry from inside a build tool is a way to publish
`0.1.0` twice.

```
cargo run -p xtask -- release plan --version 0.2.0
cargo run -p xtask -- release bump --version 0.2.0 --write
cargo run -p xtask -- release changelog --version 0.2.0 --write
cargo run -p xtask -- release publish --version 0.2.0
cargo run -p xtask -- release tag --version 0.2.0 --write
```

## Notes on the implementation

- **Four dependencies**, all already in `[workspace.dependencies]`: `clap`,
  `serde`, `serde_json`, `toml`. No error-handling crate, no `regex`, no
  `rustdoc-types` - the rustdoc JSON is read as `serde_json::Value`, which is
  what lets one binary read more than one format version.
- **rustdoc JSON and macro expansion on stable.** Both need `-Z` flags, and Moso
  is stable-only, so both run with `RUSTC_BOOTSTRAP=1` - the same thing
  `cargo-expand` does. Not a supported interface: the format version is printed
  with every result so that a gate which starts passing for the wrong reason has
  a visible cause. `cargo expand` itself is deliberately *not* used, because it
  pipes the output through `rustfmt` and a budget that moves when a formatter is
  upgraded is not a budget.
- **A gate that cannot see reports a skip, not a pass.** Missing crate, missing
  cargo feature, missing baseline: each says so, on screen and in the JSON.
- **Everything worth testing is separable from the process management.**
  `check_doc`, `measure`, `bump_manifest`, `release_changelog`, the six rules and
  the statistics are pure functions over data, and are tested as such.
