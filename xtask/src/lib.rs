#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Moso's measurement and gate harness.
//!
//! Every claim in `docs/` that has a number in it — a compile-time budget, a
//! macro expansion size, a dependency count, "no `sea-query` type appears in any
//! public signature", "100% of public traits carry a diagnostic" — is either
//! measured by this crate or it is a wish. WP-01 was skipped in the previous
//! build, and `06-reference/63-implementation-status.md` records the
//! consequence: *"its absence is why every performance and compile-time claim in
//! these docs is unverified."*
//!
//! # The eight subcommands
//!
//! | Command | Answers |
//! | --- | --- |
//! | [`bench_compile`] | how long the edit loop takes, and whether it regressed |
//! | [`expand`] | how many lines each macro emits, against the budgets |
//! | [`sealed`] | whether a foreign type is reachable from `moso-sql`/`moso-orm` |
//! | [`crates`] | whether every crate has the structural properties G5 requires |
//! | [`deps`] | the six dependency rules and the crate-count budget |
//! | [`diagnostics`] | whether every public trait has a hand-written error message |
//! | [`release`] | version bump, changelog, publish order, tag |
//! | [`ci`] | all of the above, in the order CI runs them |
//!
//! # Two design rules
//!
//! **Nothing here writes to the checkout unless it is told to.** `bench-compile`
//! copies the workspace into `target/` before it edits anything; `release` needs
//! `--write`. A measurement tool that can corrupt a working tree is a tool
//! people run once.
//!
//! **A gate that cannot see is a failure, not a pass.** Where a check depends on
//! something that does not exist yet — `moso-sql`, a `full` cargo feature, a
//! committed baseline — it says so and skips, and the skip is visible in the
//! output and in the JSON. Silent success is the one outcome that would make the
//! harness worse than nothing.
//!
//! ```no_run
//! // The shape of every gate: options in, "did it pass" out, findings printed.
//! let passed = xtask::deps::run(&xtask::deps::Options::default())?;
//! assert!(passed);
//! # Ok::<(), xtask::util::Error>(())
//! ```

pub mod bench_compile;
pub mod ci;
pub mod crates;
pub mod deps;
pub mod diagnostics;
pub mod expand;
pub mod meta;
pub mod release;
pub mod rustdoc;
pub mod sealed;
pub mod util;

/// The exit code the binary uses when a gate fails, as opposed to when the
/// harness itself could not run.
///
/// Distinguishing the two matters in CI: `1` means "the code is wrong", `2`
/// means "the harness is wrong or the environment is missing something", and
/// they need different people.
///
/// ```
/// assert_eq!(xtask::GATE_FAILED, 1);
/// assert_eq!(xtask::HARNESS_FAILED, 2);
/// ```
pub const GATE_FAILED: i32 = 1;

/// The exit code the binary uses when the harness could not complete.
///
/// ```
/// assert_eq!(xtask::HARNESS_FAILED, 2);
/// ```
pub const HARNESS_FAILED: i32 = 2;
