//! `moso self update` — the running version, and the command that changes it.
//!
//! ```text
//! $ moso self update --check
//!   ✓ version                         0.1.0
//!   ✓ installed                       /Users/x/.cargo/bin/moso (cargo install)
//!   ⚠ latest                          0.2.0 on the registry
//!       → cargo install moso-cli --locked --force
//! ```
//!
//! # Why it does not replace this binary
//!
//! Because it cannot do so honestly, and a self-updater that downloads over
//! itself without being able to verify what it downloaded is not a convenience —
//! it is a supply-chain hole with a friendly interface. This CLI depends on
//! `clap`, `serde`, `serde_json`, `toml` and `clap_complete` and nothing else,
//! deliberately: it has no HTTP client, no TLS stack and no signature
//! verification, and adding all three so that one subcommand can overwrite the
//! file it is executing from is the wrong trade.
//!
//! What it can do truthfully is the whole of what it does:
//!
//! - report the version that is running, from the manifest it was built from;
//! - report where that binary is, and — when it is in cargo's own `bin`
//!   directory — that cargo is what put it there;
//! - with `--check`, ask the registry which version is the latest published one;
//! - print the command that performs the update.
//!
//! The tool that installed a binary is the tool that can correctly replace it.
//! `cargo install` knows about the lockfile, a package manager knows about its
//! receipts, and an archive somebody unpacked knows about neither — so this
//! names the command rather than guessing at a mechanism.
//!
//! # No network unless asked
//!
//! `40-cli.md` allows this one command to be online, and it is the only one that
//! ever is. It is still off by default: `moso self update` on a laptop with no
//! connectivity, or inside a sealed build container, must not hang on a socket
//! before printing a version number it already knew. `--check` is the opt-in,
//! and it queries the registry cargo is configured with — the same registry
//! `cargo install` would install from — rather than a release feed invented
//! here.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::SelfUpdateArgs;
use crate::exit::{CliError, Outcome};
use crate::project::cargo;
use crate::ui::{Level, Ui};

use super::doctor::parse_version;

/// The crate this binary is published as.
///
/// `moso-cli`, taken from the manifest rather than written out, because the
/// command printed for the reader to paste has to name the package that
/// actually exists on the registry.
const PACKAGE: &str = env!("CARGO_PKG_NAME");

/// The version this binary was built from.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How this binary appears to have arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// Inside cargo's `bin` directory, which only `cargo install` writes to.
    Cargo,
    /// Anywhere else: a package manager, an unpacked archive, a build tree.
    Elsewhere,
}

impl Install {
    /// A stable machine-readable name.
    const fn as_str(self) -> &'static str {
        match self {
            Install::Cargo => "cargo",
            Install::Elsewhere => "unknown",
        }
    }

    /// The command that updates this installation, when there is one to name.
    ///
    /// `None` for anything outside cargo's `bin` directory, and it stays `None`
    /// rather than becoming a guess at `brew upgrade` or `apt upgrade`: nothing
    /// in this repository publishes to either, so naming one would be advice
    /// that goes nowhere.
    fn command(self) -> Option<String> {
        match self {
            Install::Cargo => Some(format!("cargo install {PACKAGE} --locked --force")),
            Install::Elsewhere => None,
        }
    }

    /// The line printed under the version, command or not.
    fn advice(self) -> String {
        self.command().unwrap_or_else(|| {
            format!(
                "update it the way you installed it; `cargo install {PACKAGE} --locked` \
                 is the documented way to install it"
            )
        })
    }
}

/// Run `moso self update`.
///
/// # Errors
/// [`Fault::Environment`](crate::exit::Fault::Environment) when `--check` was
/// given and the registry could not be reached, which is the only thing here
/// that can fail. Being out of date is not a failure — it is the answer.
pub fn run(ui: &Ui, args: &SelfUpdateArgs) -> Outcome<()> {
    let executable = std::env::current_exe().ok();
    let install = classify(executable.as_deref());
    let latest = if args.check {
        Some(latest_published()?)
    } else {
        None
    };
    let behind = latest
        .as_ref()
        .and_then(|latest| latest.as_deref())
        .map(|latest| is_newer(latest, VERSION));

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": true,
            "package": PACKAGE,
            "version": VERSION,
            "executable": executable.as_ref().map(|path| path.display().to_string()),
            "installed_by": install.as_str(),
            "update_command": install.command(),
            "advice": install.advice(),
            "checked": args.check,
            "latest": latest.flatten(),
            "update_available": behind,
        }));
        return Ok(());
    }

    ui.blank();
    ui.status(Level::Ok, "version", VERSION);
    ui.status(
        Level::Ok,
        "installed",
        &match &executable {
            Some(path) if install == Install::Cargo => {
                format!("{} (cargo install)", path.display())
            }
            Some(path) => path.display().to_string(),
            None => "this process cannot see its own path".to_owned(),
        },
    );

    match (args.check, latest.flatten(), behind) {
        (false, _, _) => {
            ui.status(
                Level::Info,
                "latest",
                "not checked — `moso self update --check` asks the registry",
            );
            ui.fix(&install.advice());
        }
        (true, None, _) => ui.status(
            Level::Info,
            "latest",
            format!("no version of `{PACKAGE}` is published on the registry yet").as_str(),
        ),
        (true, Some(latest), Some(true)) => {
            ui.status(Level::Warn, "latest", &format!("{latest} on the registry"));
            ui.fix(&install.advice());
        }
        (true, Some(latest), _) => ui.status(
            Level::Ok,
            "latest",
            &format!("{latest} on the registry — this is up to date"),
        ),
    }
    ui.blank();

    Ok(())
}

/// Where this binary lives, and what that implies.
///
/// Only one inference is drawn, and only because it is safe: cargo's `bin`
/// directory is written to by `cargo install` and by nothing else, so a binary
/// there was installed with `cargo install`. Everywhere else is reported as a
/// path and left alone — a `/usr/local/bin/moso` could have come from a package
/// manager, an installer script or a `cp`, and picking one would be a guess
/// dressed as a fact.
fn classify(executable: Option<&Path>) -> Install {
    let Some(executable) = executable else {
        return Install::Elsewhere;
    };
    let Some(parent) = executable.parent() else {
        return Install::Elsewhere;
    };

    for home in cargo_homes() {
        if parent == home.join("bin") {
            return Install::Cargo;
        }
    }
    Install::Elsewhere
}

/// The directories cargo might call home, most explicit first.
fn cargo_homes() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        homes.push(PathBuf::from(cargo_home));
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        homes.push(PathBuf::from(home).join(".cargo"));
    }
    homes
}

/// Ask the registry for the latest published version of this package.
///
/// `cargo search` rather than an HTTP request of our own: it goes to whichever
/// registry this cargo is configured with, which is by construction the registry
/// `cargo install` would install from, and it needs no HTTP client in this
/// binary. `Ok(None)` means the query succeeded and the package is not published
/// — a real answer, and one this build is likely to give.
///
/// # Errors
/// [`Fault::Environment`](crate::exit::Fault::Environment) when cargo cannot be
/// run or the registry cannot be reached.
fn latest_published() -> Outcome<Option<String>> {
    let output = Command::new(cargo())
        .args(["search", PACKAGE, "--limit", "1"])
        .output()
        .map_err(|error| {
            CliError::environment(format!("could not run cargo: {error}"))
                .with_help("install Rust from https://rustup.rs")
        })?;

    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr);
        let reason = reason.lines().next().unwrap_or("the registry said no");
        return Err(
            CliError::environment(format!("could not reach the registry: {reason}")).with_help(
                "this is the only command that needs the network; drop --check to \
                 report the running version offline",
            ),
        );
    }

    Ok(published_version(
        &String::from_utf8_lossy(&output.stdout),
        PACKAGE,
    ))
}

/// Pull the version out of `cargo search` output.
///
/// One line per crate, in the shape `name = "1.2.3"    # description`. The name
/// is matched exactly, because `cargo search moso-cli` also returns `moso-cli-x`
/// and reporting a different crate's version as this one's would be worse than
/// reporting nothing.
fn published_version(output: &str, package: &str) -> Option<String> {
    for line in output.lines() {
        // `continue`, never `?`: `cargo search` ends with a "… and N crates
        // more" note that has no `=` in it, and bailing on the first such line
        // would report "not published" for a crate that is.
        let Some((name, rest)) = line.split_once('=') else {
            continue;
        };
        if name.trim() != package {
            continue;
        }
        let version = rest.split('"').nth(1)?;
        if version.is_empty() {
            return None;
        }
        return Some(version.to_owned());
    }
    None
}

/// Whether `candidate` is a later release than `running`.
///
/// Compared as `(major, minor, patch)` by [`parse_version`], which is the
/// comparison the whole CLI already uses for the toolchain. Pre-release
/// identifiers are stripped rather than ordered, so `1.0.0-rc.1` and `1.0.0`
/// compare equal — acceptable because `cargo search` reports the latest *stable*
/// release, and a wrong answer here would be a spurious "up to date", never a
/// spurious "update available".
fn is_newer(candidate: &str, running: &str) -> bool {
    match (parse_version(candidate), parse_version(running)) {
        (Some(candidate), Some(running)) => candidate > running,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_reported_is_the_one_this_binary_was_built_from() {
        // Not a tautology: it asserts the constant is wired to the manifest
        // rather than typed out, which is what stops it going stale at a release.
        assert!(parse_version(VERSION).is_some(), "{VERSION}");
        assert_eq!(PACKAGE, "moso-cli");
    }

    #[test]
    fn a_binary_in_cargos_bin_directory_was_installed_by_cargo() {
        let home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
        let Some(home) = home else {
            return;
        };
        assert_eq!(classify(Some(&home.join("bin/moso"))), Install::Cargo);
    }

    #[test]
    fn a_binary_anywhere_else_is_left_alone_rather_than_guessed_at() {
        // `/usr/local/bin` could be brew, a `.deb`, an installer script or a
        // `cp`. Naming one of them would be a fact this cannot know.
        assert_eq!(
            classify(Some(Path::new("/usr/local/bin/moso"))),
            Install::Elsewhere
        );
        assert_eq!(
            classify(Some(Path::new("/tmp/target/debug/moso"))),
            Install::Elsewhere
        );
        assert_eq!(classify(None), Install::Elsewhere);
    }

    #[test]
    fn the_command_named_is_the_one_that_can_actually_replace_the_binary() {
        assert_eq!(
            Install::Cargo.command().as_deref(),
            Some("cargo install moso-cli --locked --force")
        );
    }

    #[test]
    fn an_installation_this_cannot_name_gets_no_command_rather_than_a_guess() {
        // No `brew upgrade`, no `apt upgrade`: nothing in this repository
        // publishes to either, and advice that goes nowhere is worse than none.
        assert_eq!(Install::Elsewhere.command(), None);

        let advice = Install::Elsewhere.advice();
        assert!(advice.contains("the way you installed it"), "{advice}");
        assert!(!advice.contains("brew"), "{advice}");
    }

    #[test]
    fn a_search_result_is_read_by_exact_name() {
        let output = "moso-cli-extras = \"9.9.9\"    # not this crate\n\
                      moso-cli = \"0.4.1\"    # The `moso` command line interface\n";
        assert_eq!(
            published_version(output, "moso-cli").as_deref(),
            Some("0.4.1")
        );
    }

    #[test]
    fn a_package_nobody_has_published_reports_nothing_rather_than_a_number() {
        assert_eq!(published_version("", "moso-cli"), None);
        assert_eq!(
            published_version("moso-cli-extras = \"9.9.9\"\n", "moso-cli"),
            None
        );
    }

    #[test]
    fn only_a_later_version_counts_as_an_update() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn an_unreadable_version_never_claims_an_update_is_available() {
        // The failure that matters: telling someone to reinstall because a
        // string did not parse.
        assert!(!is_newer("not a version", "0.1.0"));
        assert!(!is_newer("0.2.0", "not a version"));
    }
}
