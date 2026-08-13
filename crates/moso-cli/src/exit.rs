//! Failure, and the exit code it produces.
//!
//! `40-cli.md` fixes four exit codes and they are a contract, not a detail: a
//! CI job distinguishes "your spec is stale" (1) from "this machine has no
//! linker" (3) by the number, not by scraping the message.
//!
//! | code | meaning                                                        |
//! | ---- | -------------------------------------------------------------- |
//! | 0    | the command did what it said                                    |
//! | 1    | a user error: the request was well-formed but could not be done |
//! | 2    | a usage error: the command line itself was wrong                |
//! | 3    | an environment problem: the machine or project is not ready     |
//! | *n*  | `moso run` only: the code the *application* exited with          |
//!
//! The last row is the one exception, and it exists because `moso run` is a
//! transparent wrapper. `cargo run` forwards its child's exit code and so does
//! this; a wrapper that flattened every failure to 1 would be unusable in the
//! script that is the reason to have a wrapper at all. No other command may
//! construct it.

use std::fmt;
use std::process::ExitCode;

/// Which of the four documented outcomes this failure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The request was understood and could not be satisfied. Exit code 1.
    User,
    /// The command line was wrong. Exit code 2.
    ///
    /// Clap produces most of these itself; this variant is for the ones it
    /// cannot see, such as `--yes` being required because stdin is not a
    /// terminal.
    Usage,
    /// The machine or the project is not in a state where this can work.
    /// Exit code 3.
    Environment,
    /// The *application* failed, and its own exit code is being forwarded.
    ///
    /// Constructed only by `moso run`; see the module header for why it is the
    /// one fault that does not map onto a fixed code.
    Application(u8),
}

impl Fault {
    /// The process exit code for this fault.
    pub const fn code(self) -> u8 {
        match self {
            Fault::User => 1,
            Fault::Usage => 2,
            Fault::Environment => 3,
            Fault::Application(code) => code,
        }
    }

    /// A stable machine-readable name, used by `--json`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Fault::User => "user",
            Fault::Usage => "usage",
            Fault::Environment => "environment",
            Fault::Application(_) => "application",
        }
    }
}

/// Something the CLI could not do, and what the user should do about it.
///
/// The `help` line is not optional in spirit: `41-diagnostics.md` asks every
/// diagnostic Moso emits to end with something the reader can paste. It is
/// `Option` only because a handful of failures genuinely have no next step.
#[derive(Debug, Clone)]
pub struct CliError {
    /// Which exit code this produces.
    pub fault: Fault,
    /// One line, lower case, no trailing period — rustc's house style.
    pub message: String,
    /// A concrete next step, usually a command to run.
    pub help: Option<String>,
}

impl CliError {
    /// A well-formed request that could not be satisfied. Exit code 1.
    pub fn user(message: impl Into<String>) -> Self {
        Self {
            fault: Fault::User,
            message: message.into(),
            help: None,
        }
    }

    /// A malformed command line clap could not catch. Exit code 2.
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            fault: Fault::Usage,
            message: message.into(),
            help: None,
        }
    }

    /// A machine or project that is not ready. Exit code 3.
    pub fn environment(message: impl Into<String>) -> Self {
        Self {
            fault: Fault::Environment,
            message: message.into(),
            help: None,
        }
    }

    /// The application exited non-zero, and `moso run` is passing its code on.
    ///
    /// `code` is clamped away from 0: a failure that exited 0 would print an
    /// error and then tell the shell everything went well, which is the one
    /// outcome a wrapper must never produce.
    pub fn application(message: impl Into<String>, code: u8) -> Self {
        Self {
            fault: Fault::Application(if code == 0 { 1 } else { code }),
            message: message.into(),
            help: None,
        }
    }

    /// Attach the line telling the reader what to do next.
    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// The exit code this failure produces.
    pub fn code(&self) -> ExitCode {
        ExitCode::from(self.fault.code())
    }

    /// The `--json` rendering.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ok": false,
            "error": {
                "kind": self.fault.as_str(),
                "code": self.fault.code(),
                "message": self.message,
                "help": self.help,
            }
        })
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// The result type every command returns.
pub type Outcome<T = ()> = Result<T, CliError>;

/// Turn an I/O failure on a known path into an environment error.
///
/// I/O failures are environment problems, not user errors: the user asked for
/// something legitimate and the filesystem said no.
pub fn io(context: &str, path: &std::path::Path, error: &std::io::Error) -> CliError {
    let error = CliError::environment(format!("{context} `{}`: {error}", path.display()));
    match error_hint(error.message.as_str(), path) {
        Some(help) => error.with_help(help),
        None => error,
    }
}

/// A next step for the I/O failures that have an obvious one.
fn error_hint(message: &str, path: &std::path::Path) -> Option<String> {
    if message.contains("Permission denied") || message.contains("permission denied") {
        return Some(format!(
            "check the permissions on `{}`, or run the command somewhere you can write",
            path.display()
        ));
    }
    if message.contains("No space left") {
        return Some("free some disk space; `cargo clean` usually reclaims the most".to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_exit_codes_are_the_documented_ones() {
        assert_eq!(Fault::User.code(), 1);
        assert_eq!(Fault::Usage.code(), 2);
        assert_eq!(Fault::Environment.code(), 3);
    }

    #[test]
    fn json_carries_the_code_and_the_help() {
        let error = CliError::user("the committed spec is stale").with_help("moso openapi export");
        let json = error.to_json();
        assert_eq!(json["ok"], serde_json::json!(false));
        assert_eq!(json["error"]["code"], serde_json::json!(1));
        assert_eq!(json["error"]["kind"], serde_json::json!("user"));
        assert_eq!(
            json["error"]["help"],
            serde_json::json!("moso openapi export")
        );
    }

    #[test]
    fn a_permission_failure_suggests_the_permissions() {
        let error = io(
            "could not write",
            std::path::Path::new("/etc/shop"),
            &std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied"),
        );
        assert_eq!(error.fault, Fault::Environment);
        assert!(error.help.is_some_and(|help| help.contains("permissions")));
    }
}
