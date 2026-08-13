//! `moso run` — build the application, run it once, hand back its exit code.
//!
//! ```text
//! cargo build → spawn (cwd = project root, MOSO_PROFILE set) → wait → exit(n)
//! ```
//!
//! # Why this is not an alias for `cargo run`
//!
//! Four differences, and each of them is a mistake someone has made.
//!
//! **The working directory is the project root**, not wherever you were
//! standing, so `.env`, `config/` and every relative path resolve the way they
//! will in a deployment rather than the way they happen to from `src/`.
//!
//! **The package is found the way cargo finds it**, by
//! [`Project::discover`] — so this works from a subdirectory without
//! `--manifest-path`, and says which package it picked.
//!
//! **`--profile production` sets `MOSO_PROFILE`**, which is the variable the
//! application actually reads. `cargo run` has no idea that concept exists.
//!
//! **The build happens first**, so a compile error is not buried under a
//! startup log, and the time it took is reported.
//!
//! The build itself is [`Project::build`], the same call `moso dev`, `moso
//! routes` and `moso openapi` make, so a project that builds for one builds for
//! all of them.
//!
//! # Two profiles, and why they are separate flags
//!
//! `--release` picks **cargo's** profile: optimisation, debug assertions, how
//! long the compile takes. `--profile` picks **Moso's**: which
//! `config/<profile>.toml` is read, whether `.env` is loaded, whether `/docs` is
//! mounted. They are independent, and the pairing that catches people out is a
//! `--release` build still running under the `dev` profile with its
//! documentation UI exposed. Two flags, two names, no overloading.
//!
//! # The exit code
//!
//! Forwarded. `moso run` is a wrapper, and a wrapper that flattened every
//! application failure to 1 could not be used in the script that is the reason
//! to have a wrapper. A child killed by a signal reports `128 + signal`, the
//! convention every shell already uses, so a Ctrl-C'd server exits 130.
//!
//! # Ctrl-C
//!
//! Not handled here, and it does not need to be. A terminal delivers `SIGINT` to
//! the whole foreground process group, so the application receives it at the same
//! instant this process does and begins the graceful drain its own signal
//! handling implements — `server.grace`, 25 seconds by default. `moso run` never
//! sends a signal of its own and never kills the child; the drain completes on
//! the application's schedule.
//!
//! What follows from that, stated because it looks like a bug when you meet it:
//! this process is stopped by the same `SIGINT`, so the shell prompt returns
//! while the application is still draining, and its remaining log lines arrive
//! after it. Waiting instead would need a `SIGINT` handler, which needs either
//! `unsafe` (the workspace forbids it) or another dependency, and would buy
//! nothing the application is not already doing.

use std::process::ExitStatus;
use std::time::Instant;

use crate::cli::RunArgs;
use crate::exit::{CliError, Outcome};
use crate::project::Project;
use crate::ui::{Level, Ui};

use super::dev::Server;

/// The environment variable that names the profile an application runs under.
///
/// Restated here rather than imported: `moso-cli` depends on no Moso crate
/// (ADR-0004), so it cannot name `moso_core::config::PROFILE_ENV` and this is
/// the one place in the CLI that spells it. Everything else — `moso build`,
/// `moso deploy checklist` — reads it from here.
pub(super) const PROFILE_ENV: &str = "MOSO_PROFILE";

/// Run `moso run`.
///
/// # Errors
/// [`Fault::Environment`](crate::exit::Fault::Environment) when the project
/// cannot be found or the binary cannot be spawned,
/// [`Fault::User`](crate::exit::Fault::User) when it does not compile, and
/// [`Fault::Application`](crate::exit::Fault::Application) carrying the
/// application's own exit code when it exits non-zero.
pub fn run(ui: &Ui, args: &RunArgs) -> Outcome<()> {
    // Everything this command prints goes to stderr, because the child is about
    // to inherit stdout and a `building` line written there would land inside
    // the application's own output — `moso run -- --dump-routes | jq` would read
    // it first and fail on it.
    let ui = &ui.on_stderr();
    let project = Project::discover(args.app.manifest_path.as_deref())?;
    project.require_moso()?;

    let started = Instant::now();
    ui.status(Level::Ok, "building", &project.name);
    let executable = project.build(&args.app)?;
    let compiled = started.elapsed();

    let env = environment(args);
    ui.status(
        Level::Ok,
        "running",
        &format!(
            "{} ({}, built in {:.2}s)",
            project.name,
            describe_profiles(args),
            compiled.as_secs_f64()
        ),
    );
    if ui.is_verbose() {
        ui.line(&ui.dim(&format!("      {}", executable.display())));
    }

    // Nothing is emitted under `--json`, deliberately: the child inherits this
    // process's stdout, so the only thing on it is the application's own output
    // and a summary document inserted into that stream would not be JSON.
    // `moso dev` makes the same choice for the same reason.
    let mut server = Server::spawn(&executable, &project, &args.args, &env)?;
    let status = server.wait()?;
    report(&project.name, status)
}

/// The environment the application is started with, on top of the inherited one.
fn environment(args: &RunArgs) -> Vec<(&'static str, String)> {
    match args.profile {
        Some(profile) => vec![(PROFILE_ENV, profile.as_str().to_owned())],
        None => Vec::new(),
    }
}

/// Both profiles, for the `running` line.
///
/// Named together because the pair is what a reader needs: "release" alone does
/// not say whether `/docs` is mounted, and "production" alone does not say
/// whether the binary is optimised.
fn describe_profiles(args: &RunArgs) -> String {
    let cargo = if args.app.release { "release" } else { "debug" };
    match args.profile {
        Some(profile) => format!("{cargo} build, {} profile", profile.as_str()),
        None => format!("{cargo} build, profile detected by the application"),
    }
}

/// Turn the child's exit status into this command's outcome.
fn report(name: &str, status: ExitStatus) -> Outcome<()> {
    if status.success() {
        return Ok(());
    }
    let code = exit_code(status);
    Err(
        CliError::application(format!("`{name}` exited with status {code}"), code).with_help(
            "the failure is your application's and its output is above; `moso run` only \
             passed the code on",
        ),
    )
}

/// The code to exit with, given how the child ended.
///
/// A process killed by a signal has no exit code of its own, so it gets the
/// `128 + signal` every shell reports for the same event: Ctrl-C is 130, a
/// `SIGKILL` from an out-of-memory killer is 137. Reporting 1 for all of them
/// would throw away the one detail that says which happened.
#[cfg(unix)]
fn exit_code(status: ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;

    if let Some(code) = status.code() {
        return u8::try_from(code).unwrap_or(1);
    }
    status
        .signal()
        .and_then(|signal| u8::try_from(128 + signal).ok())
        .unwrap_or(1)
}

/// The code to exit with, given how the child ended.
///
/// Windows has no signals, so the status is the code or nothing.
#[cfg(not(unix))]
fn exit_code(status: ExitStatus) -> u8 {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{AppArgs, Profile};

    fn args(release: bool, profile: Option<Profile>) -> RunArgs {
        RunArgs {
            profile,
            app: AppArgs {
                release,
                ..AppArgs::default()
            },
            args: Vec::new(),
        }
    }

    #[test]
    fn a_profile_reaches_the_application_as_moso_profile() {
        let environment = environment(&args(false, Some(Profile::Production)));
        assert_eq!(environment, vec![("MOSO_PROFILE", "production".to_owned())]);
    }

    #[test]
    fn without_a_profile_nothing_is_set_and_the_application_detects_it() {
        // Setting `MOSO_PROFILE=dev` by default would silently override a
        // deployment that had set it in the environment already.
        assert!(environment(&args(false, None)).is_empty());
    }

    #[test]
    fn the_running_line_names_both_profiles() {
        let both = describe_profiles(&args(true, Some(Profile::Production)));
        assert_eq!(both, "release build, production profile");
        let neither = describe_profiles(&args(false, None));
        assert_eq!(neither, "debug build, profile detected by the application");
    }

    #[test]
    fn the_three_profiles_spell_themselves_the_way_the_framework_parses_them() {
        assert_eq!(Profile::Dev.as_str(), "dev");
        assert_eq!(Profile::Test.as_str(), "test");
        assert_eq!(Profile::Production.as_str(), "production");
    }

    #[cfg(unix)]
    #[test]
    fn a_clean_exit_is_success_and_a_dirty_one_carries_its_own_code() {
        use std::os::unix::process::ExitStatusExt;

        assert!(report("shop", ExitStatus::from_raw(0)).is_ok());

        // Raw status 7 << 8 is "exited with code 7" on every Unix.
        let error = report("shop", ExitStatus::from_raw(7 << 8)).expect_err("non-zero fails");
        assert_eq!(error.fault.code(), 7);
        assert_eq!(error.fault.as_str(), "application");
        assert!(error.message.contains("status 7"), "{}", error.message);
    }

    #[cfg(unix)]
    #[test]
    fn a_child_killed_by_a_signal_reports_the_shells_own_number() {
        use std::os::unix::process::ExitStatusExt;

        // Raw status 2 is "killed by SIGINT", which every shell reports as 130.
        assert_eq!(exit_code(ExitStatus::from_raw(2)), 130);
        // SIGKILL is 9, so 137 — the number an out-of-memory kill leaves behind.
        assert_eq!(exit_code(ExitStatus::from_raw(9)), 137);
    }

    #[cfg(unix)]
    #[test]
    fn an_exit_code_is_never_reported_as_zero_after_a_failure() {
        use std::os::unix::process::ExitStatusExt;

        // 256 truncates to 0 in a byte; a wrapper must never turn a failure into
        // "everything went well", so the constructor clamps it.
        let error = CliError::application("boom", 0);
        assert_eq!(error.fault.code(), 1);
        assert!(report("shop", ExitStatus::from_raw(0)).is_ok());
    }
}
