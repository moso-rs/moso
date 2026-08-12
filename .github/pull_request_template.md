<!--
Thank you. Before the checklist: none of these boxes are bureaucracy, and none of
them are checked by a human who has to remember. Every one is either enforced by
`.github/workflows/ci.yml` — in which case ticking it means "I already ran it and
it passed", not "I intend to" — or it is a decision only you can make, in which
case it is here because a reviewer cannot recover it from the diff.

Small pull requests get reviewed. A 2,000-line diff waits for a reviewer with a
free afternoon, and there aren't many of those.
-->

## What this changes

<!-- One paragraph, in the language a user would use. Not "refactored the
extractor pipeline" but "a `Query<T>` with a missing required field now reports
which field". -->

## Why

<!-- The problem, not the solution. Link the issue, the RFC, or the design
document section this implements (`docs/…`). If this changes behaviour that a
design document specifies, that document is updated in *this* pull request:
`docs/05-delivery/51-work-packages.md` — "The documents are the source of truth
and must not rot." -->

Closes #

## Kind of change

- [ ] Bug fix (no API change)
- [ ] New feature (additive API)
- [ ] Breaking change — **needs an RFC** (`docs/05-delivery/52-governance.md § The RFC process`)
- [ ] Documentation
- [ ] Internal / refactor / tests / CI (no user-visible change)

## Checklist

Everything below is a gate in `docs/05-delivery/53-quality-gates.md`. `cargo lint`,
`cargo docs` and `cargo ui` are aliases defined in `.cargo/config.toml`.

- [ ] `cargo fmt --all` (G1)
- [ ] `cargo lint` — clippy with `-D warnings`, all targets, all features (G1)
- [ ] `cargo nextest run --workspace --all-features` (G2)
- [ ] `cargo test --workspace --all-features --doc` (G6)
- [ ] `cargo docs` — rustdoc, no warnings, no broken intra-doc links (G6)
- [ ] New public items have real documentation and a **runnable** example (no `ignore`, no `no_run` without a reason in a comment)
- [ ] New public traits carry `#[diagnostic::on_unimplemented]`; new blanket impls carry `#[diagnostic::do_not_recommend]` (G7)
- [ ] A new compile error a user can hit has a `moso-ui-tests` case with a reviewed `.stderr` (G4)
- [ ] `#[non_exhaustive]` on new public enums and literal-constructed structs, or a comment saying why the type is deliberately closed

### If this touches the data layer

- [ ] Tests run against **real** Postgres and SQLite, gated on `DATABASE_URL` and skipping with a clear message when it is unset
- [ ] Every new SQL construct has a snapshot test on **both** dialects (D9)
- [ ] No foreign type (`sea_query::*`, `sqlx::*`, …) appears in a public signature of `moso-sql` or `moso-orm` (ADR-0005, G8)
- [ ] Eager loading is still batched: a statement-count assertion, not an inspection (N3)

### If this adds a dependency

- [ ] Justified here, in one sentence, including what it replaces or why hand-rolling is worse
- [ ] Version hoisted into `[workspace.dependencies]` with the rationale as a comment above it
- [ ] Licence is on the `deny.toml` allowlist, and `cargo deny check` passes (G11)
- [ ] Dependency count still within the budget in `docs/00-foundations/03-crate-layout.md` (G10)

## Notes for the reviewer

<!-- The part you are unsure about. The alternative you rejected and why. The
thing that looks wrong but is deliberate. Reviewers spend their attention where
you point it, so point it at the risky part. -->
