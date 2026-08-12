#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! The `xtask` binary: argument parsing and an exit code. Every subcommand's
//! behaviour lives in the library, so that it can be tested without a process.
//!
//! ```text
//! cargo xtask bench-compile --runs 5 --json target/compile-bench.json
//! cargo xtask expand-size
//! cargo xtask check-crates
//! cargo xtask check-sealed --self-test
//! cargo xtask check-deps
//! cargo xtask check-diagnostics
//! cargo xtask release plan --version 0.2.0
//! cargo xtask ci --fast
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use xtask::util::{Result, ui};
use xtask::{
    GATE_FAILED, HARNESS_FAILED, bench_compile, ci, crates, deps, diagnostics, expand, release,
    sealed,
};

/// Moso's measurement and gate harness.
#[derive(Debug, Parser)]
#[command(
    name = "xtask",
    about = "Moso's measurement and gate harness",
    long_about = "Measures what the design documents promise: compile times, macro expansion \
                  sizes, the sealed SQL facade, the dependency rules and diagnostic coverage. \
                  Exit code 1 means a gate failed; 2 means the harness could not run.",
    version
)]
struct Cli {
    /// Which task to run.
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Measure the edit loop and compare it with the committed baseline.
    BenchCompile(BenchArgs),
    /// Measure macro expansion against the budgets in 62-macro-reference.md.
    ExpandSize(ExpandArgs),
    /// Check the structural properties every crate must have (G5).
    CheckCrates(CratesArgs),
    /// Fail if a foreign type is reachable from a sealed crate's public API.
    CheckSealed(SealedArgs),
    /// Check the six dependency rules and the crate-count budget.
    CheckDeps(DepsArgs),
    /// Fail if a public trait has no hand-written diagnostic.
    CheckDiagnostics(DiagnosticsArgs),
    /// Version bump, publish dry-run and tag.
    Release(ReleaseArgs),
    /// Run the whole local gate set, in the order CI runs it.
    Ci(CiArgs),
}

#[derive(Debug, Args)]
struct BenchArgs {
    /// Scenarios to run: `all`, or a comma-separated list.
    #[arg(long, default_value = "all")]
    scenarios: String,
    /// How many times to run each scenario.
    #[arg(long, default_value_t = bench_compile::DEFAULT_RUNS)]
    runs: usize,
    /// The application to build.
    #[arg(long, default_value = "example-crud")]
    package: String,
    /// Write the report here, relative to the workspace root.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Compare against this baseline. Pass `--baseline ""` to skip comparing.
    #[arg(long, default_value = "xtask/bench/compile-baseline.json")]
    baseline: String,
    /// Overwrite the baseline with this run.
    #[arg(long)]
    update_baseline: bool,
    /// The regression allowed before the gate fails, as a percentage.
    #[arg(long, default_value_t = bench_compile::DEFAULT_TOLERANCE_PCT)]
    tolerance: f64,
    /// Where the sandbox copy of the workspace lives.
    #[arg(long, default_value = "target/xtask/bench-ws")]
    sandbox: PathBuf,
    /// Reuse an existing sandbox instead of copying the sources again.
    #[arg(long)]
    reuse_sandbox: bool,
    /// Let cargo reach the network. Off by default, so a registry lookup cannot
    /// add noise to a timing.
    #[arg(long)]
    online: bool,
}

#[derive(Debug, Args)]
struct ExpandArgs {
    /// The package whose library target is expanded.
    #[arg(long, default_value = "example-crud")]
    package: String,
    /// Write the report here.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Write the expanded source here, for reading.
    #[arg(long)]
    save: Option<PathBuf>,
    /// Build artefacts directory.
    #[arg(long)]
    target_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct CratesArgs {
    /// The exemption list, relative to the workspace root.
    #[arg(long, default_value = "xtask/allow/crates.toml")]
    allow: PathBuf,
    /// Write the report here.
    #[arg(long)]
    json: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SealedArgs {
    /// Crates to check. Defaults to moso-sql and moso-orm.
    #[arg(long)]
    crates: Vec<String>,
    /// The allowlist, relative to the workspace root.
    #[arg(long, default_value = "xtask/allow/sealed.toml")]
    allow: PathBuf,
    /// Also prove the check works, against the fixtures in xtask/fixtures.
    #[arg(long)]
    self_test: bool,
    /// Write the report here.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Build artefacts directory.
    #[arg(long)]
    target_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct DepsArgs {
    /// The battery topology, relative to the workspace root.
    #[arg(long, default_value = "xtask/allow/dep-edges.toml")]
    edges: PathBuf,
    /// The default-features crate budget.
    #[arg(long, default_value_t = deps::DEFAULT_BUDGET)]
    default_budget: usize,
    /// The `full`-features crate budget.
    #[arg(long, default_value_t = deps::FULL_BUDGET)]
    full_budget: usize,
    /// Write the report here.
    #[arg(long)]
    json: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct DiagnosticsArgs {
    /// Crates to inspect. Defaults to every Moso crate with a library target.
    #[arg(long)]
    crates: Vec<String>,
    /// The allowlist, relative to the workspace root.
    #[arg(long, default_value = "xtask/allow/diagnostics.toml")]
    allow: PathBuf,
    /// Exit zero even when a recorded gap is still open. CI must not pass this.
    #[arg(long)]
    tolerate_known_gaps: bool,
    /// Do not require `#[diagnostic::do_not_recommend]` on every blanket impl.
    /// The requirement is on by default; `docs/04-devex/41-diagnostics.md` makes
    /// it Tool 4 of five.
    #[arg(long)]
    no_blanket_impls: bool,
    /// Write the report here.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Build artefacts directory.
    #[arg(long)]
    target_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ReleaseArgs {
    /// Which step to run.
    #[command(subcommand)]
    step: ReleaseStep,
}

#[derive(Debug, Subcommand)]
enum ReleaseStep {
    /// Print everything a release would do, and do nothing.
    Plan(ReleaseCommon),
    /// Rewrite the workspace version and every intra-workspace pin.
    Bump(ReleaseCommon),
    /// `cargo publish --dry-run` for every crate, in dependency order.
    Publish(ReleaseCommon),
    /// Create the annotated git tag.
    Tag(ReleaseCommon),
}

#[derive(Debug, Args)]
struct ReleaseCommon {
    /// The version being released, for example 0.2.0.
    #[arg(long)]
    version: String,
    /// Actually change files and create the tag.
    #[arg(long)]
    write: bool,
}

#[derive(Debug, Args)]
struct CiArgs {
    /// Run only these gates, comma-separated.
    #[arg(long, default_value = "")]
    only: String,
    /// Skip these gates, comma-separated.
    #[arg(long, default_value = "")]
    skip: String,
    /// Skip the slow gates: test, doc and clippy.
    #[arg(long)]
    fast: bool,
    /// Also run bench-compile, which takes minutes.
    #[arg(long)]
    bench: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(&cli.command) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("\nxtask: a gate failed. The findings are above.");
            ExitCode::from(GATE_FAILED as u8)
        }
        Err(error) => {
            eprintln!("\nxtask: {error}");
            ExitCode::from(HARNESS_FAILED as u8)
        }
    }
}

fn dispatch(command: &Command) -> Result<bool> {
    match command {
        Command::BenchCompile(args) => {
            let baseline = if args.baseline.trim().is_empty() {
                None
            } else {
                Some(PathBuf::from(&args.baseline))
            };
            bench_compile::run(&bench_compile::Options {
                scenarios: bench_compile::Scenario::parse_list(&args.scenarios)?,
                runs: args.runs,
                package: args.package.clone(),
                json: args.json.clone(),
                baseline,
                update_baseline: args.update_baseline,
                tolerance_pct: args.tolerance,
                sandbox: args.sandbox.clone(),
                reuse_sandbox: args.reuse_sandbox,
                offline: !args.online,
            })
        }
        Command::ExpandSize(args) => expand::run(&expand::Options {
            package: args.package.clone(),
            json: args.json.clone(),
            save: args.save.clone(),
            target_dir: args.target_dir.clone(),
        }),
        Command::CheckCrates(args) => crates::run(&crates::Options {
            allow_file: args.allow.clone(),
            json: args.json.clone(),
        }),
        Command::CheckSealed(args) => {
            let crates = if args.crates.is_empty() {
                sealed::SEALED_CRATES
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect()
            } else {
                args.crates.clone()
            };
            sealed::run(&sealed::Options {
                crates,
                allow_file: args.allow.clone(),
                self_test: args.self_test,
                json: args.json.clone(),
                target_dir: args.target_dir.clone(),
            })
        }
        Command::CheckDeps(args) => deps::run(&deps::Options {
            edges_file: args.edges.clone(),
            default_budget: args.default_budget,
            full_budget: args.full_budget,
            json: args.json.clone(),
        }),
        Command::CheckDiagnostics(args) => diagnostics::run(&diagnostics::Options {
            crates: args.crates.clone(),
            allow_file: args.allow.clone(),
            tolerate_known_gaps: args.tolerate_known_gaps,
            check_blanket_impls: !args.no_blanket_impls,
            json: args.json.clone(),
            target_dir: args.target_dir.clone(),
        }),
        Command::Release(args) => {
            let (step, common) = match &args.step {
                ReleaseStep::Plan(common) => (None, common),
                ReleaseStep::Bump(common) => (Some(release::Step::Bump), common),
                ReleaseStep::Publish(common) => (Some(release::Step::Publish), common),
                ReleaseStep::Tag(common) => (Some(release::Step::Tag), common),
            };
            let options = release::Options {
                version: common.version.clone(),
                write: common.write,
            };
            match step {
                None => release::plan(&options).map(|()| true),
                Some(step) => release::run(step, &options),
            }
        }
        Command::Ci(args) => ci::run(&ci::Options {
            only: ci::Gate::parse_list(&args.only)?,
            skip: ci::Gate::parse_list(&args.skip)?,
            fast: args.fast,
            bench: args.bench,
        }),
    }
    .inspect(|passed| {
        if !passed {
            ui::note("");
        }
    })
}
