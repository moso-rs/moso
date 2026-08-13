# moso-ui-tests

**The diagnostics regression corpus.**

This crate has no runtime contents and is not published. It exists to own
`tests/ui/`: a corpus of deliberately wrong Moso programs, each paired with a
`.stderr` snapshot of the exact compiler output it must produce.

`docs/04-devex/41-diagnostics.md` promises that error quality is a product
surface with an owner, a budget and a regression suite. This is the regression
suite. A change that degrades a diagnostic shows up as a `.stderr` diff in
review, which is the whole point.

## Running it

```sh
cargo test -p moso-ui-tests                     # check the snapshots
TRYBUILD=overwrite cargo test -p moso-ui-tests  # re-record them
```

`tests/ui.rs` also lints the snapshots themselves: every `.stderr` must carry a
`help:` line containing a paste-able fix, and no line may exceed 120 characters.

## Why a separate package

`trybuild` compiles each case as a throwaway binary in a scratch crate that
depends on this package's dev-dependencies. Keeping that in its own workspace
member means the `trybuild` dependency never reaches `moso`'s dependency graph,
and `cargo test -p moso` stays fast.

## Adding a case

1. Write the wrong program in `tests/ui/<area>/<what-is-wrong>.rs`.
2. Run `TRYBUILD=overwrite cargo test -p moso-ui-tests`.
3. **Read the recorded `.stderr`.** If it is not a message you would want to
   receive at 2am, the fix belongs in the diagnostic, not in the snapshot.

## Licence

MIT - see the root [`LICENSE`](../../LICENSE).
