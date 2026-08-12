//! `bench-compile` — the edit loop, measured.
//!
//! `docs/04-devex/42-compile-times.md` opens with "this is the row on the
//! scorecard we are most likely to lose" and then sets nine numeric budgets. Its
//! own status note says every one of them is unverified, and its acceptance
//! criterion 1 is this command: *"`xtask bench-compile` exists, is reproducible
//! (± 5% across runs), and gates PRs."* WP-25 — compile-time optimisation — is
//! blocked on it, because optimisation without measurement is guesswork.
//!
//! # The six scenarios
//!
//! | Scenario | What a developer is doing |
//! | --- | --- |
//! | `cold-build` | first build of the day, or after `cargo clean` |
//! | `handler-body-edit` | the edit made most often, and the one the p50 budget is about |
//! | `check-handler-edit` | the same edit under `cargo check`, which is what an editor runs |
//! | `new-endpoint` | adding a route |
//! | `new-entity` | adding a model type |
//! | `cargo-toml-touch` | changing a dependency or a feature |
//! | `test-build` | `cargo test --no-run` after a handler edit |
//!
//! # Three things that make the numbers mean something
//!
//! **It never touches the checkout.** The workspace's sources are copied into
//! `target/xtask/bench-ws` and every edit happens there. Two consequences: the
//! benchmark cannot corrupt somebody's working tree, and it cannot fight another
//! `cargo` for the build lock on the shared `target/`.
//!
//! **`cold-build` cleans the Moso crates, not the world.** The budgets in the
//! document are measured "warm cargo cache", so third-party dependencies stay
//! built and the measurement is of Moso plus the application. Rebuilding
//! `icu_properties` five times would measure the registry, not the framework.
//!
//! **Scenarios that change the shape of the code build twice.** "Add an
//! endpoint" only means something if the previous build had one fewer endpoint,
//! so those scenarios build the base state (unmeasured) and then measure the
//! build after the addition. Without that, the second run measures a rename.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bail;
use crate::meta::Workspace;
use crate::util::{Cmd, Error, Result, Stats, fmt_ms, ui};

/// The default number of runs per scenario, and the number the ±5%
/// reproducibility criterion is stated over.
///
/// ```
/// assert_eq!(xtask::bench_compile::DEFAULT_RUNS, 5);
/// ```
pub const DEFAULT_RUNS: usize = 5;

/// The regression a scenario is allowed before the gate fails, as a percentage
/// of the committed baseline. `docs/04-devex/42-compile-times.md`: "a PR that
/// regresses any budget by > 5% fails CI".
///
/// ```
/// assert_eq!(xtask::bench_compile::DEFAULT_TOLERANCE_PCT, 5.0);
/// ```
pub const DEFAULT_TOLERANCE_PCT: f64 = 5.0;

/// One measurable edit.
///
/// ```
/// use xtask::bench_compile::Scenario;
///
/// assert_eq!(Scenario::parse("handler-body-edit")?, Scenario::HandlerBodyEdit);
/// assert!(Scenario::parse("nonsense").is_err());
/// assert_eq!(Scenario::ALL.len(), 7);
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    /// Every Moso crate and the application rebuilt from clean, dependencies warm.
    ColdBuild,
    /// One statement inside one handler changed.
    HandlerBodyEdit,
    /// The same edit, under `cargo check`.
    CheckHandlerEdit,
    /// One `#[endpoint]` added and registered.
    NewEndpoint,
    /// One model type added.
    NewEntity,
    /// The application's `Cargo.toml` touched.
    CargoTomlTouch,
    /// `cargo test --no-run` after a handler edit.
    TestBuild,
}

impl Scenario {
    /// Every scenario, in the order they are reported.
    ///
    /// ```
    /// use xtask::bench_compile::Scenario;
    ///
    /// assert_eq!(Scenario::ALL[0], Scenario::ColdBuild);
    /// ```
    pub const ALL: [Scenario; 7] = [
        Self::ColdBuild,
        Self::HandlerBodyEdit,
        Self::CheckHandlerEdit,
        Self::NewEndpoint,
        Self::NewEntity,
        Self::CargoTomlTouch,
        Self::TestBuild,
    ];

    /// The scenario's name on the command line and in the JSON.
    ///
    /// ```
    /// use xtask::bench_compile::Scenario;
    ///
    /// assert_eq!(Scenario::ColdBuild.id(), "cold-build");
    /// ```
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::ColdBuild => "cold-build",
            Self::HandlerBodyEdit => "handler-body-edit",
            Self::CheckHandlerEdit => "check-handler-edit",
            Self::NewEndpoint => "new-endpoint",
            Self::NewEntity => "new-entity",
            Self::CargoTomlTouch => "cargo-toml-touch",
            Self::TestBuild => "test-build",
        }
    }

    /// What the scenario is a proxy for.
    ///
    /// ```
    /// use xtask::bench_compile::Scenario;
    ///
    /// assert!(Scenario::HandlerBodyEdit.description().contains("handler"));
    /// ```
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::ColdBuild => "cargo build after cleaning every Moso crate (dependencies warm)",
            Self::HandlerBodyEdit => "cargo build after changing one statement in one handler",
            Self::CheckHandlerEdit => "cargo check after changing one statement in one handler",
            Self::NewEndpoint => "cargo build after adding and registering one #[endpoint]",
            Self::NewEntity => "cargo build after adding one model type",
            Self::CargoTomlTouch => "cargo build after touching the application's Cargo.toml",
            Self::TestBuild => "cargo test --no-run after changing one handler",
        }
    }

    /// Parses a scenario name, listing the alternatives when it is not one.
    ///
    /// ```
    /// use xtask::bench_compile::Scenario;
    ///
    /// let error = Scenario::parse("handler-edit").expect_err("not a scenario");
    /// assert!(error.to_string().contains("handler-body-edit"), "{error}");
    /// ```
    pub fn parse(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|scenario| scenario.id() == name)
            .ok_or_else(|| {
                Error::new(format!(
                    "`{name}` is not a scenario; the scenarios are {}",
                    Self::ALL
                        .iter()
                        .map(|scenario| scenario.id())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    /// Parses a comma-separated list, where `all` means every scenario.
    ///
    /// ```
    /// use xtask::bench_compile::Scenario;
    ///
    /// assert_eq!(Scenario::parse_list("all")?.len(), 7);
    /// assert_eq!(Scenario::parse_list("cold-build,test-build")?.len(), 2);
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn parse_list(list: &str) -> Result<Vec<Self>> {
        if list.trim() == "all" {
            return Ok(Self::ALL.to_vec());
        }
        list.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(Self::parse)
            .collect()
    }

    /// Whether the scenario needs an unmeasured build of the base state first.
    ///
    /// ```
    /// use xtask::bench_compile::Scenario;
    ///
    /// assert!(Scenario::NewEndpoint.needs_base_build());
    /// assert!(!Scenario::HandlerBodyEdit.needs_base_build());
    /// ```
    #[must_use]
    pub fn needs_base_build(self) -> bool {
        matches!(self, Self::NewEndpoint | Self::NewEntity)
    }
}

/// What one scenario measured.
///
/// ```
/// use xtask::bench_compile::ScenarioResult;
/// use xtask::util::Stats;
///
/// let result = ScenarioResult {
///     id: "handler-body-edit".into(), description: "…".into(), command: "cargo build".into(),
///     variant: None, samples_ms: vec![1000.0, 1010.0, 990.0],
///     stats: Stats::new(&[1000.0, 1010.0, 990.0]).expect("three samples"),
/// };
/// assert_eq!(result.stats.p50_ms, 1000.0);
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScenarioResult {
    /// The scenario's name.
    pub id: String,
    /// What it is a proxy for.
    pub description: String,
    /// The command that was timed, as a paste-able line.
    pub command: String,
    /// How the scenario had to be adapted to the workspace as it is today.
    pub variant: Option<String>,
    /// Every timing, in the order measured.
    pub samples_ms: Vec<f64>,
    /// The summary the gate compares.
    pub stats: Stats,
}

/// One whole benchmark run.
///
/// ```
/// use xtask::bench_compile::Bench;
///
/// let bench = Bench { generated_at: "1970-01-01".into(), rustc: "rustc 1.97.1".into(),
///     host: "aarch64-apple-darwin".into(), package: "example-crud".into(), runs: 5,
///     scenarios: Vec::new() };
/// assert!(bench.scenario("cold-build").is_none());
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Bench {
    /// The date the run was made, `YYYY-MM-DD`.
    pub generated_at: String,
    /// The compiler used.
    pub rustc: String,
    /// The host triple. A baseline from another host is not comparable.
    pub host: String,
    /// The application that was built.
    pub package: String,
    /// How many runs each scenario made.
    pub runs: usize,
    /// One entry per scenario.
    pub scenarios: Vec<ScenarioResult>,
}

impl Bench {
    /// The result for one scenario id.
    ///
    /// ```
    /// # use xtask::bench_compile::Bench;
    /// let bench = Bench { generated_at: String::new(), rustc: String::new(), host: String::new(),
    ///     package: String::new(), runs: 0, scenarios: Vec::new() };
    /// assert!(bench.scenario("cold-build").is_none());
    /// ```
    #[must_use]
    pub fn scenario(&self, id: &str) -> Option<&ScenarioResult> {
        self.scenarios.iter().find(|result| result.id == id)
    }

    /// The worst per-run deviation from the median across every scenario, which
    /// is the number the ±5% reproducibility criterion is about.
    ///
    /// ```
    /// use xtask::bench_compile::{Bench, ScenarioResult};
    /// use xtask::util::Stats;
    ///
    /// let samples = vec![100.0, 103.0, 99.0];
    /// let bench = Bench { generated_at: String::new(), rustc: String::new(), host: String::new(),
    ///     package: String::new(), runs: 3, scenarios: vec![ScenarioResult {
    ///         id: "s".into(), description: String::new(), command: String::new(), variant: None,
    ///         samples_ms: samples.clone(), stats: Stats::new(&samples).expect("three") }] };
    /// assert!((bench.worst_deviation_pct() - 3.0).abs() < 1e-9);
    /// ```
    #[must_use]
    pub fn worst_deviation_pct(&self) -> f64 {
        self.scenarios
            .iter()
            .map(|result| result.stats.deviation_pct)
            .fold(0.0_f64, f64::max)
    }
}

/// How a scenario compares with the baseline.
///
/// ```
/// use xtask::bench_compile::Comparison;
///
/// let comparison = Comparison { id: "cold-build".into(), baseline_ms: 1000.0, current_ms: 1100.0,
///     delta_pct: 10.0, within_tolerance: false };
/// assert!(!comparison.within_tolerance);
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Comparison {
    /// The scenario's name.
    pub id: String,
    /// The baseline p50, in milliseconds.
    pub baseline_ms: f64,
    /// This run's p50, in milliseconds.
    pub current_ms: f64,
    /// The change, positive meaning slower.
    pub delta_pct: f64,
    /// Whether the change is inside the tolerance.
    pub within_tolerance: bool,
}

/// Options for one run.
///
/// ```
/// let options = xtask::bench_compile::Options::default();
/// assert_eq!(options.runs, 5);
/// assert_eq!(options.package, "example-crud");
/// ```
#[derive(Clone, Debug)]
pub struct Options {
    /// Which scenarios to run.
    pub scenarios: Vec<Scenario>,
    /// How many times each.
    pub runs: usize,
    /// The application to build.
    pub package: String,
    /// Write the report here, relative to the workspace root.
    pub json: Option<PathBuf>,
    /// Compare against this baseline, relative to the workspace root.
    pub baseline: Option<PathBuf>,
    /// Overwrite the baseline with this run.
    pub update_baseline: bool,
    /// The regression allowed before the gate fails.
    pub tolerance_pct: f64,
    /// Where the sandbox copy of the workspace lives.
    pub sandbox: PathBuf,
    /// Reuse an existing sandbox instead of copying the sources again.
    pub reuse_sandbox: bool,
    /// Pass `--offline` to cargo, so a registry lookup cannot add noise.
    pub offline: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            scenarios: Scenario::ALL.to_vec(),
            runs: DEFAULT_RUNS,
            package: "example-crud".to_owned(),
            json: None,
            baseline: Some(PathBuf::from("xtask/bench/compile-baseline.json")),
            update_baseline: false,
            tolerance_pct: DEFAULT_TOLERANCE_PCT,
            sandbox: PathBuf::from("target/xtask/bench-ws"),
            reuse_sandbox: false,
            offline: true,
        }
    }
}

/// Runs the benchmark, prints the table, and compares against the baseline.
///
/// Returns `Ok(false)` when a scenario regressed past the tolerance.
///
/// ```no_run
/// let mut options = xtask::bench_compile::Options::default();
/// options.runs = 3;
/// let within = xtask::bench_compile::run(&options)?;
/// assert!(within);
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn run(options: &Options) -> Result<bool> {
    if options.runs == 0 {
        bail!("--runs must be at least 1");
    }
    let root = crate::util::workspace_root()?;
    let workspace = Workspace::load()?;
    let sandbox = Sandbox::prepare(&root, &workspace, options)?;

    ui::headline("bench-compile");
    ui::note(&format!("sandbox {}", sandbox.dir.display()));
    ui::note(&format!(
        "{} run(s) per scenario, tolerance {:.0}%",
        options.runs, options.tolerance_pct
    ));

    let mut bench = Bench {
        generated_at: crate::util::today(),
        rustc: rustc_version(),
        host: host_triple(),
        package: options.package.clone(),
        runs: options.runs,
        scenarios: Vec::new(),
    };

    for scenario in &options.scenarios {
        let result = measure(&sandbox, *scenario, options)?;
        let line = format!(
            "{:<19} p50 {:>9}  p95 {:>9}  spread {:>5.1}%",
            result.id,
            fmt_ms(result.stats.p50_ms),
            fmt_ms(result.stats.p95_ms),
            result.stats.deviation_pct
        );
        if result.stats.reproducible(options.tolerance_pct) {
            ui::ok(&line);
        } else {
            ui::warn(&format!(
                "{line}  (noisy: no run may differ from the median by more than {:.0}%)",
                options.tolerance_pct
            ));
        }
        if let Some(variant) = &result.variant {
            ui::note(variant);
        }
        bench.scenarios.push(result);
    }

    if let Some(path) = &options.json {
        let file = root.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&file, serde_json::to_string_pretty(&bench)? + "\n")?;
        ui::note(&format!("report written to {}", path.display()));
    }

    let mut within_tolerance = true;
    if let Some(path) = &options.baseline {
        let file = root.join(path);
        if options.update_baseline {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&file, serde_json::to_string_pretty(&bench)? + "\n")?;
            ui::ok(&format!("baseline updated: {}", path.display()));
        } else {
            match load_baseline(&file)? {
                None => ui::warn(&format!(
                    "no baseline at {} — run with --update-baseline to record one",
                    path.display()
                )),
                Some(baseline) => {
                    within_tolerance = compare(&baseline, &bench, options.tolerance_pct);
                }
            }
        }
    }

    println!(
        "  worst per-run deviation from the median: {:.1}% (criterion: ≤ {:.0}%)",
        bench.worst_deviation_pct(),
        options.tolerance_pct
    );

    Ok(within_tolerance)
}

fn load_baseline(path: &Path) -> Result<Option<Bench>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text).map_err(|error| {
            Error::from(error).with_context(path.display().to_string())
        })?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::from(error).with_context(path.display().to_string())),
    }
}

/// Compares a run with a baseline and prints the difference.
///
/// A baseline from another host triple is reported and *not* enforced: comparing
/// an arm64 laptop with the x86_64 reference machine would fail every PR made on
/// the wrong machine, and a gate that cries wolf gets disabled.
///
/// ```
/// use xtask::bench_compile::{Bench, ScenarioResult, compare};
/// use xtask::util::Stats;
///
/// let bench = |ms: f64| {
///     let samples = vec![ms];
///     Bench { generated_at: String::new(), rustc: String::new(),
///         host: "aarch64-apple-darwin".into(), package: String::new(), runs: 1,
///         scenarios: vec![ScenarioResult { id: "cold-build".into(),
///             description: String::new(), command: String::new(), variant: None,
///             samples_ms: samples.clone(), stats: Stats::new(&samples).expect("one") }] }
/// };
/// assert!(compare(&bench(1000.0), &bench(1040.0), 5.0), "4% is inside 5%");
/// assert!(!compare(&bench(1000.0), &bench(1060.0), 5.0), "6% is not");
/// assert!(compare(&bench(1000.0), &bench(500.0), 5.0), "faster is never a failure");
/// ```
#[must_use]
pub fn compare(baseline: &Bench, current: &Bench, tolerance_pct: f64) -> bool {
    if baseline.host != current.host {
        ui::warn(&format!(
            "the baseline was recorded on {} and this is {} — reporting the difference but not \
             enforcing it",
            baseline.host, current.host
        ));
    }
    let enforce = baseline.host == current.host;
    let mut ok = true;
    for result in &current.scenarios {
        let Some(before) = baseline.scenario(&result.id) else {
            ui::warn(&format!("{}: no baseline entry", result.id));
            continue;
        };
        let baseline_ms = before.stats.p50_ms;
        let current_ms = result.stats.p50_ms;
        let delta_pct = if baseline_ms > 0.0 {
            (current_ms - baseline_ms) / baseline_ms * 100.0
        } else {
            0.0
        };
        let comparison = Comparison {
            id: result.id.clone(),
            baseline_ms,
            current_ms,
            delta_pct,
            within_tolerance: delta_pct <= tolerance_pct,
        };
        let line = format!(
            "{:<19} {} -> {} ({:+.1}% vs baseline)",
            comparison.id,
            fmt_ms(baseline_ms),
            fmt_ms(current_ms),
            delta_pct
        );
        if comparison.within_tolerance {
            ui::ok(&line);
        } else if enforce {
            ui::fail(&line);
            ok = false;
        } else {
            ui::warn(&line);
        }
    }
    ok
}

fn rustc_version() -> String {
    Cmd::new("rustc")
        .arg("--version")
        .capture()
        .map(|output| output.stdout.trim().to_owned())
        .unwrap_or_else(|_| "unknown".to_owned())
}

fn host_triple() -> String {
    Cmd::new("rustc")
        .arg("-vV")
        .capture()
        .ok()
        .and_then(|output| {
            output
                .stdout
                .lines()
                .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The copy of the workspace the benchmark edits.
///
/// ```no_run
/// use xtask::bench_compile::{Options, Sandbox};
/// use xtask::meta::Workspace;
///
/// let root = xtask::util::workspace_root()?;
/// let workspace = Workspace::load()?;
/// let sandbox = Sandbox::prepare(&root, &workspace, &Options::default())?;
/// assert!(sandbox.dir.join("Cargo.toml").is_file());
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Sandbox {
    /// The sandbox's root directory.
    pub dir: PathBuf,
    /// The package being built.
    pub package: String,
    /// The generated module the scenarios rewrite, relative to the sandbox.
    pub bench_file: PathBuf,
    /// The application's manifest, relative to the sandbox.
    pub manifest: PathBuf,
    /// Every workspace member, for `cargo clean -p`.
    pub members: Vec<String>,
    /// Whether the workspace has an ORM, which decides what `new-entity` writes.
    pub has_orm: bool,
    /// Whether cargo is run with `--offline`.
    pub offline: bool,
}

impl Sandbox {
    /// Copies the workspace, injects the benchmark module, and builds once so
    /// that every measured build starts from a warm dependency cache.
    ///
    /// ```no_run
    /// # use xtask::bench_compile::{Options, Sandbox};
    /// # use xtask::meta::Workspace;
    /// # let root = xtask::util::workspace_root()?;
    /// # let workspace = Workspace::load()?;
    /// let sandbox = Sandbox::prepare(&root, &workspace, &Options::default())?;
    /// assert!(sandbox.members.iter().any(|m| m == "moso-core"));
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn prepare(root: &Path, workspace: &Workspace, options: &Options) -> Result<Self> {
        let Some(package) = workspace.package(&options.package) else {
            bail!(
                "{} is not a workspace member; bench-compile needs an application to build \
                 (try --package example-crud)",
                options.package
            );
        };
        let dir = root.join(&options.sandbox);
        if dir.exists() && !options.reuse_sandbox {
            std::fs::remove_dir_all(&dir)
                .map_err(|error| Error::from(error).with_context("clearing the sandbox"))?;
        }
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
            for entry in ["Cargo.toml", "Cargo.lock", "rustfmt.toml"] {
                let from = root.join(entry);
                if from.is_file() {
                    std::fs::copy(&from, dir.join(entry))?;
                }
            }
            for entry in ["crates", "examples", "xtask"] {
                let from = root.join(entry);
                if from.is_dir() {
                    copy_tree(&from, &dir.join(entry))?;
                }
            }
        }

        let relative_manifest = package
            .manifest_path
            .strip_prefix(root)
            .map(Path::to_path_buf)
            .map_err(|_| {
                Error::new(format!(
                    "{} is outside the workspace root, which bench-compile cannot copy",
                    package.manifest_path.display()
                ))
            })?;
        let package_dir = relative_manifest
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let routes_mod = package_dir.join("src/routes/mod.rs");
        let (mod_file, bench_file) = if dir.join(&routes_mod).is_file() {
            (routes_mod, package_dir.join("src/routes/xtask_bench.rs"))
        } else {
            (
                package_dir.join("src/lib.rs"),
                package_dir.join("src/xtask_bench.rs"),
            )
        };
        if !dir.join(&mod_file).is_file() {
            bail!(
                "cannot find {} in the sandbox; bench-compile needs a module to add its \
                 generated code to",
                mod_file.display()
            );
        }

        let sandbox = Self {
            dir,
            package: options.package.clone(),
            bench_file,
            manifest: relative_manifest,
            members: workspace
                .packages
                .iter()
                .map(|member| member.name.clone())
                .collect(),
            has_orm: workspace.has("moso-orm"),
            offline: options.offline,
        };

        let declaration = "\n// Added by `xtask bench-compile`; see xtask/src/bench_compile.rs.\npub mod xtask_bench;\n";
        let mod_path = sandbox.dir.join(&mod_file);
        let existing = std::fs::read_to_string(&mod_path)?;
        if !existing.contains("pub mod xtask_bench;") {
            std::fs::write(&mod_path, existing + declaration)?;
        }
        sandbox.write_bench_module(0, 0, 0)?;

        // The unmeasured build that makes every later measurement warm.
        sandbox.cargo(&["build"]).run().map_err(|error| {
            error.with_context("the sandbox does not build, so nothing can be measured")
        })?;
        Ok(sandbox)
    }

    /// A cargo invocation inside the sandbox, with the flags every measurement
    /// wants: the package, the lockfile frozen, and no colour.
    ///
    /// ```no_run
    /// # use xtask::bench_compile::{Options, Sandbox};
    /// # use xtask::meta::Workspace;
    /// # let root = xtask::util::workspace_root()?;
    /// # let workspace = Workspace::load()?;
    /// # let sandbox = Sandbox::prepare(&root, &workspace, &Options::default())?;
    /// assert!(sandbox.cargo(&["build"]).rendered().contains("--locked"));
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn cargo(&self, args: &[&str]) -> Cmd {
        let mut cmd = Cmd::cargo()
            .cwd(&self.dir)
            .args(args.iter().copied())
            .args(["--package", &self.package, "--locked"]);
        if self.offline {
            cmd = cmd.arg("--offline");
        }
        cmd
    }

    /// Writes the generated module with `marker` in the handler body, plus
    /// `extra_endpoints` further endpoints and `extra_models` further model
    /// types.
    ///
    /// ```no_run
    /// # use xtask::bench_compile::{Options, Sandbox};
    /// # use xtask::meta::Workspace;
    /// # let root = xtask::util::workspace_root()?;
    /// # let workspace = Workspace::load()?;
    /// # let sandbox = Sandbox::prepare(&root, &workspace, &Options::default())?;
    /// sandbox.write_bench_module(3, 1, 0)?;
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn write_bench_module(
        &self,
        marker: u64,
        extra_endpoints: usize,
        extra_models: usize,
    ) -> Result<()> {
        let source = bench_module(marker, extra_endpoints, extra_models, self.has_orm);
        std::fs::write(self.dir.join(&self.bench_file), source)?;
        Ok(())
    }

    /// Makes the smallest change to the application's manifest that cargo
    /// actually acts on: declaring an unused feature.
    ///
    /// A comment, or a bare `touch`, provably rebuilds *nothing* — cargo's
    /// dirty-checking reads the dep-info rustc wrote, and `Cargo.toml` is not in
    /// it. Measured on this machine: appending a comment and rebuilding takes
    /// 0.07 s and compiles no crate, while renaming a declared feature takes
    /// 0.65 s and recompiles the package. The second is the edit a developer
    /// makes when they touch a manifest, so it is the one the scenario makes.
    ///
    /// ```no_run
    /// # use xtask::bench_compile::{Options, Sandbox};
    /// # use xtask::meta::Workspace;
    /// # let root = xtask::util::workspace_root()?;
    /// # let workspace = Workspace::load()?;
    /// # let sandbox = Sandbox::prepare(&root, &workspace, &Options::default())?;
    /// sandbox.touch_manifest(1)?;
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn touch_manifest(&self, marker: u64) -> Result<()> {
        let path = self.dir.join(&self.manifest);
        let text = std::fs::read_to_string(&path)?;
        std::fs::write(&path, manifest_with_marker(&text, marker))?;
        Ok(())
    }

    /// Removes the build artefacts of every workspace member, leaving the
    /// third-party dependencies built.
    ///
    /// ```no_run
    /// # use xtask::bench_compile::{Options, Sandbox};
    /// # use xtask::meta::Workspace;
    /// # let root = xtask::util::workspace_root()?;
    /// # let workspace = Workspace::load()?;
    /// # let sandbox = Sandbox::prepare(&root, &workspace, &Options::default())?;
    /// sandbox.clean_members()?;
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn clean_members(&self) -> Result<()> {
        let mut cmd = Cmd::cargo().cwd(&self.dir).arg("clean");
        for member in &self.members {
            cmd = cmd.args(["--package", member]);
        }
        cmd.run().map(|_| ())
    }
}

/// The prefix of the feature the `cargo-toml-touch` scenario declares.
///
/// ```
/// assert_eq!(xtask::bench_compile::MARKER_FEATURE_PREFIX, "xtask-bench-marker-");
/// ```
pub const MARKER_FEATURE_PREFIX: &str = "xtask-bench-marker-";

/// Rewrites a manifest so it declares exactly one marker feature.
///
/// ```
/// use xtask::bench_compile::manifest_with_marker;
///
/// let manifest = "[package]\nname = \"app\"\n";
/// let first = manifest_with_marker(manifest, 1);
/// assert!(first.contains("[features]"));
/// assert!(first.contains("xtask-bench-marker-1 = []"));
///
/// // The second call replaces the first marker rather than accumulating, and
/// // reuses the `[features]` table it already created.
/// let second = manifest_with_marker(&first, 2);
/// assert!(second.contains("xtask-bench-marker-2 = []"));
/// assert!(!second.contains("xtask-bench-marker-1"));
/// assert_eq!(second.matches("[features]").count(), 1);
///
/// // An existing `[features]` table is used instead of a new one.
/// let existing = manifest_with_marker("[features]\ndefault = []\n", 3);
/// assert_eq!(existing.matches("[features]").count(), 1);
/// assert!(existing.contains("default = []"));
/// ```
#[must_use]
pub fn manifest_with_marker(text: &str, marker: u64) -> String {
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| !line.trim_start().starts_with(MARKER_FEATURE_PREFIX))
        .map(str::to_owned)
        .collect();
    let marker_line = format!("{MARKER_FEATURE_PREFIX}{marker} = []");
    match lines.iter().position(|line| line.trim() == "[features]") {
        Some(index) => lines.insert(index + 1, marker_line),
        None => {
            lines.push(String::new());
            lines.push(
                "# Added by `xtask bench-compile`: a declared feature is the smallest manifest"
                    .to_owned(),
            );
            lines.push("# change cargo treats as significant.".to_owned());
            lines.push("[features]".to_owned());
            lines.push(marker_line);
        }
    }
    lines.join("\n") + "\n"
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    const SKIP: [&str; 4] = ["target", ".git", "node_modules", ".DS_Store"];
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if SKIP.contains(&name_str.as_ref()) {
            continue;
        }
        let source = entry.path();
        let destination = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_tree(&source, &destination)?;
        } else {
            std::fs::copy(&source, &destination)?;
        }
    }
    Ok(())
}

/// The command a scenario times.
///
/// ```no_run
/// # use xtask::bench_compile::{Options, Sandbox, Scenario, scenario_command};
/// # use xtask::meta::Workspace;
/// # let root = xtask::util::workspace_root()?;
/// # let workspace = Workspace::load()?;
/// # let sandbox = Sandbox::prepare(&root, &workspace, &Options::default())?;
/// assert!(scenario_command(&sandbox, Scenario::TestBuild).rendered().contains("--no-run"));
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[must_use]
pub fn scenario_command(sandbox: &Sandbox, scenario: Scenario) -> Cmd {
    match scenario {
        Scenario::CheckHandlerEdit => sandbox.cargo(&["check"]),
        Scenario::TestBuild => sandbox.cargo(&["test", "--no-run"]),
        _ => sandbox.cargo(&["build"]),
    }
}

/// Puts the sandbox into the state one run of `scenario` measures a build from.
///
/// `marker` distinguishes runs, so that consecutive runs are consecutive edits
/// rather than the same edit twice — cargo would do nothing the second time.
fn prepare_run(sandbox: &Sandbox, scenario: Scenario, marker: u64) -> Result<()> {
    match scenario {
        Scenario::ColdBuild => sandbox.clean_members(),
        Scenario::HandlerBodyEdit | Scenario::CheckHandlerEdit | Scenario::TestBuild => {
            sandbox.write_bench_module(marker, 0, 0)
        }
        Scenario::NewEndpoint | Scenario::NewEntity => {
            // The base state has to be *built*, not only written: "one endpoint
            // was added" is only a meaningful measurement if the last build had
            // one fewer.
            sandbox.write_bench_module(0, 0, 0)?;
            scenario_command(sandbox, scenario).run()?;
            if scenario == Scenario::NewEndpoint {
                sandbox.write_bench_module(0, 1, 0)
            } else {
                sandbox.write_bench_module(0, 0, 1)
            }
        }
        Scenario::CargoTomlTouch => sandbox.touch_manifest(marker),
    }
}

fn measure(sandbox: &Sandbox, scenario: Scenario, options: &Options) -> Result<ScenarioResult> {
    let mut samples = Vec::with_capacity(options.runs);
    let mut command = String::new();
    let mut variant = None;

    // One unmeasured run first, doing exactly what a measured run does.
    // `cargo check` and `cargo test` keep different artefacts from
    // `cargo build`, and `cold-build` deletes all of them, so without this the
    // first measured run of a scenario pays for whatever the previous scenario
    // threw away: measured, that was a 6.9 s first run against a 0.22 s median
    // for `check-handler-edit`, and a 5.06 s outlier against 4.68 s for
    // `cold-build`.
    sandbox.write_bench_module(0, 0, 0)?;
    prepare_run(sandbox, scenario, 0)?;
    scenario_command(sandbox, scenario)
        .run()
        .map_err(|error| error.with_context(format!("priming the {} scenario", scenario.id())))?;

    for run in 0..options.runs {
        let marker = (run + 1) as u64;
        prepare_run(sandbox, scenario, marker)?;
        if scenario == Scenario::NewEntity && !sandbox.has_orm {
            variant = Some(
                "variant: #[derive(Schema)] — moso-orm is not in the workspace, so there is no \
                 #[derive(Entity)] to measure yet"
                    .to_owned(),
            );
        }

        let cmd = scenario_command(sandbox, scenario);
        command = cmd.rendered();
        let (millis, output) = cmd.timed()?;
        if !output.ok() {
            bail!(
                "the {} scenario failed to build on run {}\n{}",
                scenario.id(),
                run + 1,
                crate::util::indent(&output.stderr_tail(25))
            );
        }
        samples.push(millis);
    }

    // Leave the sandbox in its base state so the next scenario starts clean.
    sandbox.write_bench_module(0, 0, 0)?;

    let stats = Stats::new(&samples).ok_or_else(|| Error::new("no samples"))?;
    Ok(ScenarioResult {
        id: scenario.id().to_owned(),
        description: scenario.description().to_owned(),
        command,
        variant,
        samples_ms: samples,
        stats,
    })
}

/// The source of the module the scenarios rewrite.
///
/// It is deliberately small and deliberately generated: the benchmark must not
/// depend on the shape of somebody's example application, and the edit it makes
/// must be the same edit every time.
///
/// ```
/// use xtask::bench_compile::bench_module;
///
/// let source = bench_module(7, 1, 1, false);
/// assert!(source.contains("let marker: u64 = 7;"));
/// assert!(source.contains("xtask_bench_extra_0"));
/// assert!(source.contains("XtaskBenchModel0"));
/// assert!(!source.contains("derive(Entity"), "no ORM in the workspace");
/// assert!(bench_module(0, 0, 1, true).contains("derive(Entity"));
/// ```
#[must_use]
pub fn bench_module(
    marker: u64,
    extra_endpoints: usize,
    extra_models: usize,
    has_orm: bool,
) -> String {
    let mut source = String::new();
    source.push_str(
        "//! Written by `xtask bench-compile` into a copy of the workspace under\n\
         //! `target/`, never into the checkout. See `xtask/src/bench_compile.rs`.\n\
         //!\n\
         //! The scenarios edit this file and nothing else, so that \"a handler body\n\
         //! changed\" means the same thing on every run and in every release.\n\
         \n\
         use moso::prelude::*;\n\
         \n\
         /// What the benchmark endpoint returns.\n\
         #[derive(Schema, Debug, Clone)]\n\
         pub struct XtaskBenchOut {\n\
         \x20   /// The value the `handler-body-edit` scenario changes.\n\
         \x20   pub marker: u64,\n\
         }\n\
         \n\
         /// The handler whose body the `handler-body-edit` scenario rewrites.\n\
         #[endpoint]\n\
         async fn xtask_bench_root() -> Result<Json<XtaskBenchOut>> {\n",
    );
    source.push_str(&format!("    let marker: u64 = {marker};\n"));
    source.push_str(
        "    Ok(Json(XtaskBenchOut { marker }))\n\
         }\n\
         \n\
         /// The routes this module registers.\n\
         pub fn router() -> Router {\n\
         \x20   moso::routes! {\n\
         \x20       GET \"/xtask-bench\" => xtask_bench_root,\n\
         \x20   }\n\
         }\n",
    );

    for index in 0..extra_endpoints {
        source.push_str(&format!(
            "\n/// An endpoint added by the `new-endpoint` scenario.\n\
             #[endpoint]\n\
             async fn xtask_bench_extra_{index}() -> Result<Json<XtaskBenchOut>> {{\n\
             \x20   Ok(Json(XtaskBenchOut {{ marker: {index} }}))\n\
             }}\n\
             \n\
             /// The route for the endpoint above.\n\
             pub fn extra_router_{index}() -> Router {{\n\
             \x20   moso::routes! {{\n\
             \x20       GET \"/xtask-bench/extra-{index}\" => xtask_bench_extra_{index},\n\
             \x20   }}\n\
             }}\n"
        ));
    }

    for index in 0..extra_models {
        if has_orm {
            source.push_str(&format!(
                "\n/// A model added by the `new-entity` scenario.\n\
                 #[derive(Entity, Debug, Clone)]\n\
                 #[entity(table = \"xtask_bench_model_{index}\")]\n\
                 pub struct XtaskBenchModel{index} {{\n\
                 \x20   /// Primary key.\n\
                 \x20   #[entity(pk)]\n\
                 \x20   pub id: Id<XtaskBenchModel{index}>,\n\
                 \x20   /// An indexed, unique column.\n\
                 \x20   #[entity(unique, index)]\n\
                 \x20   pub slug: String,\n\
                 \x20   /// A plain column.\n\
                 \x20   pub title: String,\n\
                 \x20   /// Another plain column.\n\
                 \x20   pub body: String,\n\
                 \x20   /// A nullable column.\n\
                 \x20   pub subtitle: Option<String>,\n\
                 \x20   /// A numeric column.\n\
                 \x20   pub reading_time: u32,\n\
                 \x20   /// A boolean column.\n\
                 \x20   pub published: bool,\n\
                 \x20   /// A list column.\n\
                 \x20   pub tags: Vec<String>,\n\
                 }}\n"
            ));
        } else {
            source.push_str(&format!(
                "\n/// A model added by the `new-entity` scenario.\n\
                 ///\n\
                 /// A schema derive rather than an entity derive, because this workspace has\n\
                 /// no `moso-orm` yet. The scenario switches over on its own once it does.\n\
                 #[derive(Schema, Debug, Clone)]\n\
                 pub struct XtaskBenchModel{index} {{\n\
                 \x20   /// An identifier.\n\
                 \x20   pub id: u64,\n\
                 \x20   /// A constrained string.\n\
                 \x20   #[schema(len = 1..=120)]\n\
                 \x20   pub slug: String,\n\
                 \x20   /// A plain column.\n\
                 \x20   pub title: String,\n\
                 \x20   /// Another plain column.\n\
                 \x20   pub body: String,\n\
                 \x20   /// A nullable column.\n\
                 \x20   pub subtitle: Option<String>,\n\
                 \x20   /// A numeric column with a range.\n\
                 \x20   #[schema(range = 0..=600)]\n\
                 \x20   pub reading_time: u32,\n\
                 \x20   /// A boolean column.\n\
                 \x20   pub published: bool,\n\
                 \x20   /// A list column.\n\
                 \x20   pub tags: Vec<String>,\n\
                 }}\n"
            ));
        }
    }

    source
}

/// The scenarios' p50 values, keyed by id — the shape a report or a baseline
/// reduces to when only the gate's number matters.
///
/// ```
/// use xtask::bench_compile::{Bench, ScenarioResult, p50_by_scenario};
/// use xtask::util::Stats;
///
/// let samples = vec![1_500.0];
/// let bench = Bench { generated_at: String::new(), rustc: String::new(), host: String::new(),
///     package: String::new(), runs: 1, scenarios: vec![ScenarioResult {
///         id: "cold-build".into(), description: String::new(), command: String::new(),
///         variant: None, samples_ms: samples.clone(),
///         stats: Stats::new(&samples).expect("one") }] };
/// assert_eq!(p50_by_scenario(&bench).get("cold-build"), Some(&1_500.0));
/// ```
#[must_use]
pub fn p50_by_scenario(bench: &Bench) -> BTreeMap<String, f64> {
    bench
        .scenarios
        .iter()
        .map(|result| (result.id.clone(), result.stats.p50_ms))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_has_a_unique_id_and_a_description() {
        let mut ids: Vec<&str> = Scenario::ALL.iter().map(|s| s.id()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(
            ids.len(),
            count,
            "the ids are the JSON keys and must be unique"
        );
        for scenario in Scenario::ALL {
            assert!(!scenario.description().is_empty(), "{}", scenario.id());
            assert_eq!(
                Scenario::parse(scenario.id()).expect("round trip"),
                scenario
            );
        }
    }

    #[test]
    fn the_generated_module_is_valid_looking_rust_for_every_shape() {
        for (marker, endpoints, models, orm) in [
            (0, 0, 0, false),
            (5, 0, 0, false),
            (0, 1, 0, false),
            (0, 0, 1, false),
            (0, 0, 1, true),
            (9, 3, 2, false),
        ] {
            let source = bench_module(marker, endpoints, models, orm);
            assert_eq!(
                source.matches('{').count(),
                source.matches('}').count(),
                "braces balance for ({marker}, {endpoints}, {models}, {orm})"
            );
            assert!(source.starts_with("//!"));
            assert!(source.contains("use moso::prelude::*;"));
            assert_eq!(
                source.matches("#[endpoint]").count(),
                1 + endpoints,
                "one endpoint per registration"
            );
            assert_eq!(source.matches("moso::routes!").count(), 1 + endpoints);
        }
    }

    #[test]
    fn the_marker_is_the_only_difference_between_two_body_edits() {
        let first = bench_module(1, 0, 0, false);
        let second = bench_module(2, 0, 0, false);
        let differing: Vec<(&str, &str)> = first
            .lines()
            .zip(second.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(differing.len(), 1, "{differing:?}");
        assert!(differing[0].0.contains("let marker"));
    }

    #[test]
    fn adding_an_endpoint_only_appends() {
        let base = bench_module(0, 0, 0, false);
        let plus_one = bench_module(0, 1, 0, false);
        assert!(
            plus_one.starts_with(&base),
            "the new-endpoint scenario must be an append, or it measures a rewrite"
        );
    }

    #[test]
    fn the_baseline_is_committed_and_parses() {
        let root = crate::util::workspace_root().expect("a workspace");
        let path = root.join("xtask/bench/compile-baseline.json");
        let text = std::fs::read_to_string(&path).expect("the committed baseline");
        let bench: Bench = serde_json::from_str(&text).expect("valid baseline JSON");
        assert!(!bench.scenarios.is_empty());
        for result in &bench.scenarios {
            Scenario::parse(&result.id).expect("a known scenario");
            assert!(result.stats.p50_ms > 0.0, "{} has no timing", result.id);
            assert_eq!(
                result.samples_ms.len(),
                result.stats.runs,
                "{} records fewer samples than runs",
                result.id
            );
        }
    }

    #[test]
    fn a_faster_run_never_fails_the_gate_and_a_slower_one_does() {
        let bench = |ms: f64, host: &str| {
            let samples = vec![ms];
            Bench {
                generated_at: String::new(),
                rustc: String::new(),
                host: host.to_owned(),
                package: String::new(),
                runs: 1,
                scenarios: vec![ScenarioResult {
                    id: "cold-build".to_owned(),
                    description: String::new(),
                    command: String::new(),
                    variant: None,
                    samples_ms: samples.clone(),
                    stats: Stats::new(&samples).expect("one sample"),
                }],
            }
        };
        assert!(compare(&bench(1000.0, "x"), &bench(200.0, "x"), 5.0));
        assert!(!compare(&bench(1000.0, "x"), &bench(2000.0, "x"), 5.0));
        assert!(
            compare(&bench(1000.0, "linux"), &bench(2000.0, "darwin"), 5.0),
            "a cross-host comparison is reported, not enforced"
        );
    }

    #[test]
    fn a_missing_baseline_entry_is_a_warning_not_a_failure() {
        let empty = Bench {
            generated_at: String::new(),
            rustc: String::new(),
            host: "x".to_owned(),
            package: String::new(),
            runs: 0,
            scenarios: Vec::new(),
        };
        let samples = vec![10.0];
        let current = Bench {
            scenarios: vec![ScenarioResult {
                id: "new-scenario".to_owned(),
                description: String::new(),
                command: String::new(),
                variant: None,
                samples_ms: samples.clone(),
                stats: Stats::new(&samples).expect("one sample"),
            }],
            ..empty.clone()
        };
        assert!(compare(&empty, &current, 5.0));
    }
}
