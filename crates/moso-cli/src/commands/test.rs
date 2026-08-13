//! `moso test` — run the suite, and say what it did **not** run.
//!
//! ```text
//! $ moso test
//!   ✓ runner                          cargo-nextest 0.9.90
//!   ⚠ DATABASE_URL, REDIS_URL         unset — every suite that needs one will skip
//!       → ./scripts/test-db.sh up, then eval "$(./scripts/test-db.sh env)"
//!   … cargo nextest run
//!   … cargo test --doc
//!   ✓ 2 passes                        tests, doctests
//! ```
//!
//! # The reason this is not `cargo test`
//!
//! Three, and the third is the one that matters.
//!
//! **It uses the runner you installed.** `cargo nextest run` when
//! `cargo-nextest` is on `PATH`, `cargo test` when it is not, and it says which
//! — a suite that ran under a different runner than you assumed is a suite whose
//! output you are reading wrong. `--no-nextest` forces the fallback.
//!
//! **It runs doctests either way.** Nextest cannot run them (it drives test
//! binaries, and a doctest is not one), so a project that switched to nextest
//! quietly stopped compiling the examples in its rustdoc. Pass one is the test
//! binaries — `cargo nextest run`, or `cargo test --all-targets`, which is the
//! same set — and pass two is always `cargo test --doc`. Both runners therefore
//! cover the same ground.
//!
//! **It says which suites skipped.** Moso's data-layer tests are required to
//! *skip*, not fail, when `DATABASE_URL` is unset: the macOS CI leg runs the
//! whole suite with it deliberately unset, and a test that failed without a
//! database would be a broken test. The cost of that design is a trap — a green
//! run that never touched Postgres looks exactly like a green run that did — and
//! the trap is worse for `REDIS_URL`, because exporting one of the two produces
//! a run in which the *other* suite silently skipped. So the state of both is
//! reported before anything runs and again in the summary, and it is a warning
//! rather than a note, because "passed" is what it is being mistaken for.
//!
//! # What it does not do
//!
//! Manage a database. `40-cli.md` describes `moso test` creating a template
//! database and cloning it per test; that needs a database lifecycle this build
//! has no front end for, and inventing one here would put a second, divergent
//! copy of `43-testing.md`'s strategy in the CLI. This command reports the
//! environment and runs the tests.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::TestArgs;
use crate::exit::{CliError, Outcome};
use crate::project::{Project, cargo};
use crate::ui::{Level, Ui};

use super::doctor::capture;

/// The variables whose absence turns a suite into a skip.
///
/// Named as a pair because they are only ever right as a pair: the workspace's
/// own `scripts/test-db.sh env` prints both in one block for exactly this
/// reason, and a run with one of them set is the failure this reports on.
const DATA_LAYER: [&str; 2] = ["DATABASE_URL", "REDIS_URL"];

/// The helper a Moso checkout ships for starting both stores.
const TEST_DB_SCRIPT: &str = "scripts/test-db.sh";

/// Which runner executed the test binaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    /// `cargo nextest run` or `cargo test`, as the reader sees it.
    pub name: &'static str,
    /// The version string the tool reported, when there is one.
    pub version: Option<String>,
}

impl Runner {
    /// How this reads on the `runner` line.
    fn describe(&self) -> String {
        match &self.version {
            Some(version) => version.clone(),
            None => self.name.to_owned(),
        }
    }
}

/// One invocation of a test runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pass {
    /// What this pass covers, for the summary.
    pub label: &'static str,
    /// The arguments handed to cargo, `cargo` itself excluded.
    pub arguments: Vec<String>,
}

impl Pass {
    /// The command as it would be typed, for `--verbose` and for `--json`.
    fn command(&self) -> String {
        format!("cargo {}", self.arguments.join(" "))
    }
}

/// Run `moso test`.
///
/// [`Project::require_moso`] is deliberately not called: running a package's
/// tests needs nothing from the framework, and refusing to run them because a
/// manifest does not name `moso` would be a rule with no reason behind it.
///
/// # Errors
/// [`Fault::Environment`](crate::exit::Fault::Environment) when the project
/// cannot be found or cargo cannot be run, and
/// [`Fault::User`](crate::exit::Fault::User) when a pass fails — which is what
/// makes this usable as a CI gate.
pub fn run(ui: &Ui, args: &TestArgs) -> Outcome<()> {
    let project = Project::discover(args.manifest_path.as_deref())?;
    let runner = choose_runner(args);
    let environment = DataLayer::detect();

    ui.blank();
    ui.status(Level::Ok, "runner", &runner.describe());
    report_environment(ui, &project, &environment);

    let passes = plan(args, &project, &runner);
    let mut failed = Vec::new();
    for pass in &passes {
        if ui.is_verbose() {
            ui.line(&ui.dim(&format!("      {}", pass.command())));
        }
        if !execute(ui, &project, pass)? {
            failed.push(pass.label);
        }
    }

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": failed.is_empty(),
            "package": project.name,
            "runner": runner.name,
            "runner_version": runner.version,
            "passes": passes.iter().map(|pass| serde_json::json!({
                "label": pass.label,
                "command": pass.command(),
                "ok": !failed.contains(&pass.label),
            })).collect::<Vec<_>>(),
            "environment": {
                "DATABASE_URL": environment.database,
                "REDIS_URL": environment.redis,
            },
            "skipped_suites": environment.skipped(),
        }));
    } else {
        ui.blank();
        let labels: Vec<&str> = passes.iter().map(|pass| pass.label).collect();
        if failed.is_empty() {
            ui.status(
                Level::Ok,
                &format!("{} passes", passes.len()),
                &labels.join(", "),
            );
        } else {
            ui.status(Level::Fail, "failed", &failed.join(", "));
        }
        // Repeated after the run on purpose. The line printed before a
        // three-minute suite is not the line anybody is looking at when they
        // decide the change is safe to merge.
        report_environment(ui, &project, &environment);
    }

    if failed.is_empty() {
        return Ok(());
    }
    Err(CliError::user(format!(
        "{} of {} passes failed",
        failed.len(),
        passes.len()
    ))
    .with_help("the runner's output is above; it names the failing tests"))
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// Which runner to use, and what it calls itself.
fn choose_runner(args: &TestArgs) -> Runner {
    if args.no_nextest {
        return Runner {
            name: "cargo test",
            version: None,
        };
    }
    match capture("cargo", &["nextest", "--version"]) {
        Some(text) => Runner {
            name: "cargo nextest",
            version: Some(
                text.lines()
                    .next()
                    .unwrap_or("cargo nextest")
                    .trim()
                    .to_owned(),
            ),
        },
        None => Runner {
            name: "cargo test",
            version: None,
        },
    }
}

/// The passes to run, in order.
fn plan(args: &TestArgs, project: &Project, runner: &Runner) -> Vec<Pass> {
    let mut passes = Vec::with_capacity(2);

    let mut binaries: Vec<String> = if runner.name == "cargo nextest" {
        vec!["nextest".to_owned(), "run".to_owned()]
    } else {
        // `--all-targets` is every target *except* doctests, which keeps the two
        // runners covering exactly the same ground in pass one.
        vec!["test".to_owned(), "--all-targets".to_owned()]
    };
    binaries.extend(common(args, project));
    if let Some(filter) = &args.filter {
        binaries.push(filter.clone());
    }
    binaries.extend(trailing(args));
    passes.push(Pass {
        label: "tests",
        arguments: binaries,
    });

    if !args.no_doc {
        let mut doc = vec!["test".to_owned(), "--doc".to_owned()];
        doc.extend(common(args, project));
        if let Some(filter) = &args.filter {
            doc.push(filter.clone());
        }
        doc.extend(trailing(args));
        passes.push(Pass {
            label: "doctests",
            arguments: doc,
        });
    }

    passes
}

/// The flags both passes carry.
fn common(args: &TestArgs, project: &Project) -> Vec<String> {
    let mut out = vec![
        "--manifest-path".to_owned(),
        project.manifest_path.display().to_string(),
    ];
    if args.workspace {
        out.push("--workspace".to_owned());
    }
    if args.all_features {
        out.push("--all-features".to_owned());
    } else if let Some(features) = &args.features {
        out.push("--features".to_owned());
        out.push(features.clone());
    }
    out
}

/// Whatever followed `--`, forwarded to the test binaries.
fn trailing(args: &TestArgs) -> Vec<String> {
    if args.args.is_empty() {
        return Vec::new();
    }
    let mut out = vec!["--".to_owned()];
    out.extend(args.args.iter().cloned());
    out
}

/// Run one pass, and report whether it succeeded.
///
/// Under `--json` the runner's standard output is copied to *stderr* instead of
/// being inherited, because `ui`'s first rule is that nothing but the document
/// reaches stdout in that mode. It is copied on a thread rather than buffered so
/// a twenty-minute suite still prints as it goes.
fn execute(ui: &Ui, project: &Project, pass: &Pass) -> Outcome<bool> {
    let mut command = Command::new(cargo());
    command
        .args(&pass.arguments)
        .current_dir(&project.root)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    if ui.is_json() {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::inherit());
    }

    let mut child = command.spawn().map_err(|error| {
        CliError::environment(format!("could not run cargo: {error}"))
            .with_help("install Rust from https://rustup.rs")
    })?;

    let relay = child.stdout.take().map(|stdout| {
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut stderr = std::io::stderr();
            let _ = std::io::copy(&mut reader, &mut stderr);
        })
    });

    let status = child.wait().map_err(|error| {
        CliError::environment(format!("could not wait for the test runner: {error}"))
    })?;
    if let Some(relay) = relay {
        let _ = relay.join();
    }

    Ok(status.success())
}

// ---------------------------------------------------------------------------
// The data layer
// ---------------------------------------------------------------------------

/// Which of the two connection variables are set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataLayer {
    /// `DATABASE_URL` is set and not empty.
    pub database: bool,
    /// `REDIS_URL` is set and not empty.
    pub redis: bool,
}

impl DataLayer {
    /// Read the environment.
    fn detect() -> Self {
        Self {
            database: is_set(DATA_LAYER[0]),
            redis: is_set(DATA_LAYER[1]),
        }
    }

    /// The stores whose suites will skip.
    fn skipped(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.database {
            out.push("postgres");
        }
        if !self.redis {
            out.push("redis");
        }
        out
    }

    /// The variables that are not set.
    fn missing(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.database {
            out.push(DATA_LAYER[0]);
        }
        if !self.redis {
            out.push(DATA_LAYER[1]);
        }
        out
    }
}

/// Whether a variable is set to something non-empty.
fn is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

/// Say what will run and what will not.
fn report_environment(ui: &Ui, project: &Project, environment: &DataLayer) {
    let missing = environment.missing();
    if missing.is_empty() {
        ui.status(
            Level::Ok,
            &DATA_LAYER.join(", "),
            "both set — the data-layer suites will run",
        );
        return;
    }

    let detail = if missing.len() == DATA_LAYER.len() {
        "unset — every suite that needs one skips, and a skipped test still \
         reports success"
            .to_owned()
    } else {
        // The asymmetric case is the dangerous one: half the data layer ran, the
        // whole run says "ok", and nobody looks again.
        format!(
            "unset — the {} suites skip while the rest of the run reports success",
            environment.skipped().join(" and ")
        )
    };
    ui.status(Level::Warn, &missing.join(", "), &detail);
    ui.fix(&start_hint(project));
}

/// How to get both stores running, given what this project ships.
///
/// A Moso checkout has `scripts/test-db.sh`, which starts Postgres and Redis
/// together and prints both URLs in one `eval`-able block — precisely so that
/// exporting one without the other is hard. A project that does not have it gets
/// the generic instruction instead of a path that does not exist.
fn start_hint(project: &Project) -> String {
    match find_script(&project.root) {
        Some(script) => format!(
            "{} up, then eval \"$({} env)\" — it prints both URLs together",
            script.display(),
            script.display()
        ),
        None => format!(
            "start Postgres and Redis, then export {} — export both or neither",
            DATA_LAYER.join(" and ")
        ),
    }
}

/// Look for `scripts/test-db.sh` at or above `root`.
///
/// Above as well as at, because in a Cargo workspace the package being tested is
/// a directory or two below the checkout that owns the script.
fn find_script(root: &Path) -> Option<PathBuf> {
    for directory in root.ancestors() {
        let candidate = directory.join(TEST_DB_SCRIPT);
        if candidate.is_file() {
            return Some(match candidate.strip_prefix(root) {
                Ok(relative) => Path::new("./").join(relative),
                Err(_) => candidate,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> TestArgs {
        TestArgs {
            filter: None,
            workspace: false,
            no_nextest: false,
            no_doc: false,
            all_features: false,
            features: None,
            manifest_path: None,
            args: Vec::new(),
        }
    }

    fn project(root: &str) -> Project {
        Project {
            manifest_path: PathBuf::from(root).join("Cargo.toml"),
            root: PathBuf::from(root),
            name: "shop".to_owned(),
            rust_version: None,
            uses_moso: true,
        }
    }

    fn nextest() -> Runner {
        Runner {
            name: "cargo nextest",
            version: Some("cargo-nextest 0.9.90".to_owned()),
        }
    }

    fn plain() -> Runner {
        Runner {
            name: "cargo test",
            version: None,
        }
    }

    #[test]
    fn nextest_never_runs_the_doctests_so_a_second_pass_does() {
        let passes = plan(&args(), &project("/tmp/shop"), &nextest());
        assert_eq!(passes.len(), 2);
        assert!(passes[0].command().starts_with("cargo nextest run"));
        assert_eq!(passes[1].label, "doctests");
        assert!(passes[1].command().contains("--doc"));
    }

    #[test]
    fn both_runners_cover_the_same_ground() {
        // `cargo test` would run doctests in pass one and then run them again in
        // pass two, so pass one is `--all-targets`, which is every target except
        // doctests. Same set, once each, whichever runner is installed.
        let passes = plan(&args(), &project("/tmp/shop"), &plain());
        assert_eq!(passes.len(), 2);
        assert!(passes[0].command().contains("--all-targets"));
        assert!(!passes[0].command().contains("--doc"));
        assert!(passes[1].command().contains("--doc"));
    }

    #[test]
    fn no_doc_leaves_exactly_one_pass() {
        let args = TestArgs {
            no_doc: true,
            ..args()
        };
        assert_eq!(plan(&args, &project("/tmp/shop"), &nextest()).len(), 1);
    }

    #[test]
    fn a_filter_and_trailing_arguments_reach_both_passes() {
        let args = TestArgs {
            filter: Some("users".to_owned()),
            args: vec!["--nocapture".to_owned()],
            ..args()
        };
        for pass in plan(&args, &project("/tmp/shop"), &nextest()) {
            let command = pass.command();
            assert!(command.contains(" users"), "{command}");
            assert!(command.ends_with("-- --nocapture"), "{command}");
        }
    }

    #[test]
    fn features_are_passed_one_way_or_the_other_but_never_both() {
        let all = TestArgs {
            all_features: true,
            features: Some("orm".to_owned()),
            ..args()
        };
        let rendered = common(&all, &project("/tmp/shop")).join(" ");
        assert!(rendered.contains("--all-features"), "{rendered}");
        assert!(!rendered.contains("--features"), "{rendered}");

        let some = TestArgs {
            features: Some("orm,jobs".to_owned()),
            ..args()
        };
        let rendered = common(&some, &project("/tmp/shop")).join(" ");
        assert!(rendered.contains("--features orm,jobs"), "{rendered}");
    }

    #[test]
    fn the_package_is_named_explicitly_so_a_subdirectory_tests_the_same_thing() {
        let rendered = common(&args(), &project("/tmp/shop")).join(" ");
        assert!(
            rendered.contains("--manifest-path /tmp/shop/Cargo.toml"),
            "{rendered}"
        );
        assert!(!rendered.contains("--workspace"), "{rendered}");

        let wide = TestArgs {
            workspace: true,
            ..args()
        };
        assert!(common(&wide, &project("/tmp/shop")).contains(&"--workspace".to_owned()));
    }

    #[test]
    fn no_nextest_forces_the_fallback_whatever_is_installed() {
        let forced = choose_runner(&TestArgs {
            no_nextest: true,
            ..args()
        });
        assert_eq!(forced.name, "cargo test");
        assert_eq!(forced.version, None);
        assert_eq!(forced.describe(), "cargo test");
    }

    #[test]
    fn an_empty_trailing_list_does_not_produce_a_dangling_separator() {
        assert!(trailing(&args()).is_empty());
    }

    #[test]
    fn both_variables_unset_means_both_suites_skip() {
        let none = DataLayer {
            database: false,
            redis: false,
        };
        assert_eq!(none.skipped(), vec!["postgres", "redis"]);
        assert_eq!(none.missing(), vec!["DATABASE_URL", "REDIS_URL"]);
    }

    #[test]
    fn exporting_one_of_the_two_still_leaves_a_suite_skipping() {
        // The trap: DATABASE_URL alone gives a green run in which every Redis
        // test skipped, so this case has to be reported, not treated as fine.
        let half = DataLayer {
            database: true,
            redis: false,
        };
        assert_eq!(half.skipped(), vec!["redis"]);
        assert_eq!(half.missing(), vec!["REDIS_URL"]);

        let both = DataLayer {
            database: true,
            redis: true,
        };
        assert!(both.skipped().is_empty());
        assert!(both.missing().is_empty());
    }

    #[test]
    fn the_hint_names_the_script_when_the_checkout_has_one() {
        let scratch = std::env::temp_dir().join(format!("moso-test-hint-{}", std::process::id()));
        let package = scratch.join("crates/shop");
        std::fs::create_dir_all(scratch.join("scripts")).expect("scratch");
        std::fs::create_dir_all(&package).expect("scratch");
        std::fs::write(scratch.join(TEST_DB_SCRIPT), "#!/bin/sh\n").expect("script");

        // Found from the checkout root …
        let hint = start_hint(&project(&scratch.display().to_string()));
        assert!(hint.contains("test-db.sh up"), "{hint}");
        assert!(hint.contains("env"), "{hint}");

        // … and from a package a couple of directories below it.
        let nested = Project {
            manifest_path: package.join("Cargo.toml"),
            root: package,
            name: "shop".to_owned(),
            rust_version: None,
            uses_moso: true,
        };
        assert!(start_hint(&nested).contains("test-db.sh"));

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn without_a_script_the_hint_is_the_generic_one() {
        let hint = start_hint(&project("/definitely/not/a/checkout/4f2a"));
        assert!(hint.contains("DATABASE_URL and REDIS_URL"), "{hint}");
        assert!(hint.contains("export both or neither"), "{hint}");
    }
}
