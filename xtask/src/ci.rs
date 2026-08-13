//! `ci` — the whole local gate set, in one command.
//!
//! `docs/05-delivery/53-quality-gates.md` lists the gates and its status note
//! says *"there is no CI configuration in the repository … every gate is
//! currently a convention, not a gate."* The first thing a contributor needs is
//! not a YAML file, it is a single command that runs what CI runs, in the order
//! CI runs it, and prints a table at the end. When that exists, the YAML file is
//! four lines.
//!
//! Every step runs even after an earlier one fails, because a contributor who
//! has to re-run the suite four times to find four problems stops running it.

use crate::util::{Cmd, Result, ui};
use crate::{bench_compile, crates, deps, diagnostics, expand, sealed, util};

/// One gate.
///
/// ```
/// use xtask::ci::Gate;
///
/// assert_eq!(Gate::parse("clippy")?, Gate::Clippy);
/// assert!(Gate::parse("lint").is_err());
/// assert_eq!(Gate::ALL.len(), 10);
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// `cargo fmt --all --check`.
    Fmt,
    /// `cargo clippy --workspace --all-targets -- -D warnings`.
    Clippy,
    /// `cargo check --workspace --all-targets`.
    Check,
    /// `cargo test --workspace`.
    Test,
    /// `cargo doc --workspace --no-deps` with warnings denied.
    Doc,
    /// The structural properties every crate must have.
    Crates,
    /// The six dependency rules and the crate budget.
    Deps,
    /// The sealed-facade check, including its self-test.
    Sealed,
    /// Diagnostic coverage over every public trait.
    Diagnostics,
    /// The macro expansion budgets.
    ExpandSize,
}

impl Gate {
    /// Every gate, in the order CI runs them: cheapest and most-likely-to-fail
    /// first, so a broken build is reported in seconds rather than minutes.
    ///
    /// ```
    /// use xtask::ci::Gate;
    ///
    /// assert_eq!(Gate::ALL[0], Gate::Fmt);
    /// ```
    pub const ALL: [Gate; 10] = [
        Self::Fmt,
        Self::Check,
        Self::Clippy,
        Self::Crates,
        Self::Deps,
        Self::Sealed,
        Self::Diagnostics,
        Self::ExpandSize,
        Self::Test,
        Self::Doc,
    ];

    /// The gate's name on the command line.
    ///
    /// ```
    /// assert_eq!(xtask::ci::Gate::ExpandSize.id(), "expand-size");
    /// ```
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Fmt => "fmt",
            Self::Clippy => "clippy",
            Self::Check => "check",
            Self::Test => "test",
            Self::Doc => "doc",
            Self::Crates => "check-crates",
            Self::Deps => "check-deps",
            Self::Sealed => "check-sealed",
            Self::Diagnostics => "check-diagnostics",
            Self::ExpandSize => "expand-size",
        }
    }

    /// Whether the gate takes minutes rather than seconds, and is therefore
    /// skipped by `--fast`.
    ///
    /// ```
    /// use xtask::ci::Gate;
    ///
    /// assert!(Gate::Test.is_slow());
    /// assert!(!Gate::Fmt.is_slow());
    /// ```
    #[must_use]
    pub fn is_slow(self) -> bool {
        matches!(self, Self::Test | Self::Doc | Self::Clippy)
    }

    /// Parses a gate name, listing the alternatives when it is not one.
    ///
    /// ```
    /// use xtask::ci::Gate;
    ///
    /// let error = Gate::parse("sealed").expect_err("the id is check-sealed");
    /// assert!(error.to_string().contains("check-sealed"), "{error}");
    /// ```
    pub fn parse(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|gate| gate.id() == name)
            .ok_or_else(|| {
                util::Error::new(format!(
                    "`{name}` is not a gate; the gates are {}",
                    Self::ALL
                        .iter()
                        .map(|gate| gate.id())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    /// Parses a comma-separated list.
    ///
    /// ```
    /// use xtask::ci::Gate;
    ///
    /// assert_eq!(Gate::parse_list("fmt,check")?.len(), 2);
    /// assert!(Gate::parse_list("").unwrap().is_empty());
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn parse_list(list: &str) -> Result<Vec<Self>> {
        list.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(Self::parse)
            .collect()
    }
}

/// Options for one run of the suite.
///
/// ```
/// let options = xtask::ci::Options::default();
/// assert!(options.only.is_empty());
/// assert!(!options.fast);
/// ```
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Run only these gates. Empty means all of them.
    pub only: Vec<Gate>,
    /// Skip these gates.
    pub skip: Vec<Gate>,
    /// Skip the slow gates.
    pub fast: bool,
    /// Also run `bench-compile`, which takes minutes.
    pub bench: bool,
}

impl Options {
    /// The gates this configuration will run, in order.
    ///
    /// ```
    /// use xtask::ci::{Gate, Options};
    ///
    /// let options = Options { fast: true, ..Options::default() };
    /// let gates = options.selected();
    /// assert!(!gates.contains(&Gate::Test));
    /// assert!(gates.contains(&Gate::Fmt));
    ///
    /// let one = Options { only: vec![Gate::Deps], ..Options::default() };
    /// assert_eq!(one.selected(), vec![Gate::Deps]);
    /// ```
    #[must_use]
    pub fn selected(&self) -> Vec<Gate> {
        Gate::ALL
            .into_iter()
            .filter(|gate| self.only.is_empty() || self.only.contains(gate))
            .filter(|gate| !self.skip.contains(gate))
            .filter(|gate| !(self.fast && gate.is_slow()))
            .collect()
    }
}

/// One gate's outcome.
///
/// The name is a string rather than a [`Gate`], because `--bench` adds a row
/// that is not one of the nine gates and a summary that mislabels a row is worse
/// than no summary.
///
/// ```
/// use xtask::ci::{Gate, GateResult};
///
/// let result = GateResult { name: Gate::Fmt.id().to_owned(), passed: true, note: None };
/// assert!(result.passed);
/// assert_eq!(result.name, "fmt");
/// ```
#[derive(Clone, Debug)]
pub struct GateResult {
    /// What ran.
    pub name: String,
    /// Whether it passed.
    pub passed: bool,
    /// Why it failed, when the reason is not already on the screen.
    pub note: Option<String>,
}

/// Runs the selected gates and prints a summary.
///
/// ```no_run
/// let options = xtask::ci::Options { fast: true, ..Default::default() };
/// let passed = xtask::ci::run(&options)?;
/// assert!(passed);
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn run(options: &Options) -> Result<bool> {
    let root = util::workspace_root()?;
    let gates = options.selected();
    ui::headline(&format!(
        "ci — {} gate(s): {}",
        gates.len(),
        gates
            .iter()
            .map(|gate| gate.id())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    let mut results: Vec<GateResult> = Vec::new();
    for gate in gates {
        let outcome = match gate {
            Gate::Fmt => cargo_gate(&root, &["fmt", "--all", "--check"], &[]),
            Gate::Check => cargo_gate(&root, &["check", "--workspace", "--all-targets"], &[]),
            Gate::Clippy => cargo_gate(
                &root,
                &[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ],
                &[],
            ),
            Gate::Test => cargo_gate(&root, &["test", "--workspace"], &[]),
            Gate::Doc => cargo_gate(
                &root,
                &["doc", "--workspace", "--no-deps"],
                &[("RUSTDOCFLAGS", "-D warnings")],
            ),
            Gate::Crates => crates::run(&crates::Options::default()),
            Gate::Deps => deps::run(&deps::Options::default()),
            Gate::Sealed => sealed::run(&sealed::Options {
                self_test: true,
                ..sealed::Options::default()
            }),
            Gate::Diagnostics => diagnostics::run(&diagnostics::Options {
                check_blanket_impls: true,
                ..diagnostics::Options::default()
            }),
            Gate::ExpandSize => expand::run(&expand::Options::default()),
        };
        let (passed, note) = match outcome {
            Ok(passed) => (passed, None),
            Err(error) => (false, Some(error.to_string())),
        };
        if let Some(note) = &note {
            ui::fail(&format!("{}: {note}", gate.id()));
        }
        results.push(GateResult {
            name: gate.id().to_owned(),
            passed,
            note,
        });
    }

    if options.bench {
        let outcome = bench_compile::run(&bench_compile::Options::default());
        let (passed, note) = match outcome {
            Ok(passed) => (passed, None),
            Err(error) => (false, Some(error.to_string())),
        };
        if let Some(note) = &note {
            ui::fail(&format!("bench-compile: {note}"));
        }
        results.push(GateResult {
            name: "bench-compile".to_owned(),
            passed,
            note,
        });
    }

    ui::headline("ci summary");
    for result in &results {
        let line = result.name.clone();
        if result.passed {
            ui::ok(&line);
        } else {
            ui::fail(&line);
        }
    }
    let failed = results.iter().filter(|result| !result.passed).count();
    println!(
        "\n{} of {} gate(s) passed",
        results.len() - failed,
        results.len()
    );
    Ok(failed == 0)
}

fn cargo_gate(root: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> Result<bool> {
    let mut cmd = Cmd::cargo().cwd(root).args(args.iter().copied());
    for (key, value) in env {
        cmd = cmd.env(*key, *value);
    }
    ui::headline(&cmd.rendered());
    let code = cmd.stream()?;
    Ok(code == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_gate_has_a_unique_id_that_round_trips() {
        let mut ids: Vec<&str> = Gate::ALL.iter().map(|gate| gate.id()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
        for gate in Gate::ALL {
            assert_eq!(Gate::parse(gate.id()).expect("round trip"), gate);
        }
    }

    #[test]
    fn cheap_gates_run_before_expensive_ones() {
        let order: Vec<&str> = Gate::ALL.iter().map(|gate| gate.id()).collect();
        let fmt = order.iter().position(|id| *id == "fmt").expect("fmt");
        let test = order.iter().position(|id| *id == "test").expect("test");
        assert!(fmt < test, "a formatting failure must be reported first");
    }

    #[test]
    fn skip_wins_over_only() {
        let options = Options {
            only: vec![Gate::Fmt, Gate::Check],
            skip: vec![Gate::Check],
            ..Options::default()
        };
        assert_eq!(options.selected(), vec![Gate::Fmt]);
    }

    #[test]
    fn fast_drops_exactly_the_slow_gates() {
        let options = Options {
            fast: true,
            ..Options::default()
        };
        let selected = options.selected();
        assert!(selected.iter().all(|gate| !gate.is_slow()));
        assert_eq!(
            selected.len(),
            Gate::ALL.iter().filter(|gate| !gate.is_slow()).count()
        );
    }

    #[test]
    fn selecting_nothing_still_runs_everything() {
        assert_eq!(Options::default().selected().len(), Gate::ALL.len());
    }
}
