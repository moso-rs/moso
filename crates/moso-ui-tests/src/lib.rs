#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "The Moso diagnostics regression corpus."]
//!
//! This crate has no runtime contents. It exists to own `tests/ui/`: a corpus of
//! deliberately wrong Moso programs, each paired with a `.stderr` snapshot of the
//! exact compiler output it must produce.
//!
//! `docs/04-devex/41-diagnostics.md` promises that error quality is a product
//! surface with an owner, a budget and a regression suite. This is the regression
//! suite. A change that degrades a diagnostic shows up as a `.stderr` diff in
//! review, which is the whole point.
//!
//! # Running it
//!
//! ```text
//! cargo test -p moso-ui-tests                    # check the snapshots
//! TRYBUILD=overwrite cargo test -p moso-ui-tests # re-record them
//! ```
//!
//! `tests/ui.rs` also lints the snapshots themselves: every `.stderr` must carry
//! a `help:` line containing a paste-able fix, and no line may exceed 120
//! characters.
//!
//! # Why a separate package
//!
//! `trybuild` compiles each case as a throwaway binary in a scratch crate that
//! depends on this package's dev-dependencies. Keeping that in its own member
//! means the `trybuild` dependency never reaches `moso`'s dependency graph, and
//! `cargo test -p moso` stays fast.
