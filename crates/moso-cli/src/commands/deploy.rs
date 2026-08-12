//! `moso deploy checklist` — the pre-flight, not the flight.
//!
//! ```text
//! $ moso deploy checklist
//!   ✓ profile                         production (MOSO_PROFILE=production)
//!   ✓ expose_internal_errors          off — 5xx responses carry no detail
//!   ✓ expose_docs                     off — /docs and /openapi.json are not mounted
//!   ⚠ trusted_proxies                 empty — X-Forwarded-For is not believed
//!       → set it if this runs behind a load balancer; empty is right if it does not
//!   ✗ secret_key                      still on its default (src/lib.rs)
//!       → moso config --generate-secret, then set SHOP__SECRET_KEY in the environment
//!   ✗ .env                            tracked by git
//!       → git rm --cached .env && echo .env >> .gitignore
//!   ✓ shutdown grace                  25 s, under the usual 30 s kill timeout
//!   ✓ /healthz, /readyz               mounted by App::build()
//! ```
//!
//! # It is a checklist, not a deployer
//!
//! `40-cli.md` is explicit that Moso is not a PaaS, and this command tree exists
//! only so that the one useful thing in it — a pre-production audit — has a
//! home. It writes nothing, uploads nothing and connects to nothing. The
//! `dockerfile`, `compose`, `k8s` and provider subcommands sketched in that
//! document are not implemented and are absent from the tree rather than
//! present and empty.
//!
//! # Where each answer comes from
//!
//! Two sources, and the difference is worth knowing when a finding surprises
//! you.
//!
//! **The application itself**, through `--dump-config` run with
//! `MOSO_PROFILE=production`. That is the whole reason for
//! [`Project::dump_with_env`]: the values worth auditing are the ones the
//! production profile resolves, and a checklist that reported development values
//! before a production deployment would answer a question nobody asked. From it
//! come the profile the application actually settled on and, for every secret
//! field, *where the value came from* — which is the check that catches a
//! password committed to `config/production.toml`.
//!
//! **The project on disk**, by reading `src/**/*.rs` and `config/*.toml`. This
//! is where the rest live, and they have to, because `HttpConfig` and
//! `ServerConfig` are handed to the builder in code — `App::new(cfg)
//! .http_config(..)` — rather than resolved through the configuration stack, so
//! `--dump-config` never sees them. The scan is line-based and it says so: every
//! finding names the file and line it read, so the reader confirms rather than
//! trusts. Lines that are comments are skipped, which is what keeps the doc
//! comment above a field from being reported as the field.
//!
//! A check that cannot be answered says so and is reported as informational. It
//! never guesses, because a checklist that invents a ✓ is worse than no
//! checklist: it is the thing that gets trusted at 2 a.m.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::cli::{DeployChecklistArgs, DeployCommand, Profile};
use crate::exit::{CliError, Outcome};
use crate::project::{Dump, Project};
use crate::ui::{Level, Ui};

use super::run::PROFILE_ENV;

/// The grace period an orchestrator typically allows before `SIGKILL`.
///
/// Kubernetes' `terminationGracePeriodSeconds` and Docker's `stop` both default
/// to 30 seconds. A drain that asks for longer is not a longer drain; it is a
/// process killed mid-request, which is the thing the grace existed to prevent.
const USUAL_KILL_TIMEOUT: u64 = 30;

/// Dispatch one `moso deploy` subcommand.
///
/// # Errors
/// Whatever [`checklist`] returns.
pub fn run(ui: &Ui, command: &DeployCommand) -> Outcome<()> {
    match command {
        DeployCommand::Checklist(args) => checklist(ui, args),
    }
}

// ---------------------------------------------------------------------------
// One finding
// ---------------------------------------------------------------------------

/// One thing the checklist looked at.
#[derive(Debug, Clone)]
pub struct Finding {
    /// What was checked, in the vocabulary of the thing being checked.
    pub name: String,
    /// How it went.
    pub level: Level,
    /// What was found.
    pub detail: String,
    /// The file the answer was read from, when it came from one.
    pub file: Option<String>,
    /// What to do about it.
    pub fix: Option<String>,
}

impl Finding {
    /// A check that passed.
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::at(name, Level::Ok, detail)
    }

    /// Something to think about, that does not block a deployment.
    fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::at(name, Level::Warn, detail)
    }

    /// Something that must be fixed before this is deployed.
    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::at(name, Level::Fail, detail)
    }

    /// A question this build cannot answer, reported rather than guessed at.
    fn info(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::at(name, Level::Info, detail)
    }

    /// The shared constructor.
    fn at(name: impl Into<String>, level: Level, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level,
            detail: detail.into(),
            file: None,
            fix: None,
        }
    }

    /// Name the file the finding was read from.
    #[must_use]
    fn in_file(mut self, file: impl Into<String>) -> Self {
        self.file = Some(file.into());
        self
    }

    /// Attach the fix.
    #[must_use]
    fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    /// The `--json` rendering.
    fn to_json(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "level": self.level.as_str(),
            "detail": self.detail,
            "file": self.file,
            "fix": self.fix,
        })
    }

    /// The right column, file included when there is one.
    fn detail_line(&self) -> String {
        match &self.file {
            Some(file) => format!("{} ({file})", self.detail),
            None => self.detail.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

/// Run `moso deploy checklist`.
///
/// # Errors
/// Anything the dump protocol can fail with, plus
/// [`Fault::User`](crate::exit::Fault::User) when a check failed — which is what
/// lets this gate a deploy.
pub fn checklist(ui: &Ui, args: &DeployChecklistArgs) -> Outcome<()> {
    let project = Project::discover(args.app.manifest_path.as_deref())?;
    project.require_moso()?;

    let answer = project.dump_with_env(
        &args.app,
        Dump::Config,
        &[(PROFILE_ENV, args.profile.as_str())],
    )?;
    let document: Value = serde_json::from_str(&answer).map_err(|error| {
        CliError::user(format!(
            "the application's `--dump-config` output is not JSON: {error}"
        ))
        .with_help("everything except the document must go to stderr")
    })?;

    let source = Source::read(&project.root);
    let mut findings = vec![profile_check(&document, args)];
    findings.push(source.toggle_is_off(
        "expose_internal_errors",
        "5xx responses carry no detail, source chain or backtrace",
        "remove it: a 500 that names the row it failed on is a disclosure, and \
         the developer error page belongs to the dev profile",
    ));
    findings.push(source.toggle_is_off(
        "expose_docs",
        "/docs and /openapi.json are not mounted in the production profile",
        "remove it, or put the documentation UI behind authentication; the \
         default is already off in production",
    ));
    findings.push(source.trusted_proxies());
    findings.extend(source.cors());
    findings.extend(secrets(&document));
    findings.push(dotenv_is_untracked(&project));
    findings.push(source.shutdown_grace());
    findings.push(source.probes());

    let failed = findings
        .iter()
        .filter(|finding| finding.level == Level::Fail)
        .count();
    let warned = findings
        .iter()
        .filter(|finding| finding.level == Level::Warn)
        .count();

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": failed == 0 && !(args.strict && warned > 0),
            "profile": args.profile.as_str(),
            "checks": findings.iter().map(Finding::to_json).collect::<Vec<_>>(),
            "failed": failed,
            "warned": warned,
        }));
    } else {
        ui.blank();
        for finding in &findings {
            ui.status(finding.level, &finding.name, &finding.detail_line());
            if let Some(fix) = &finding.fix {
                ui.fix(fix);
            }
        }
        ui.blank();
    }

    verdict(failed, warned, args.strict)
}

/// Turn the counts into an exit code.
fn verdict(failed: usize, warned: usize, strict: bool) -> Outcome<()> {
    if failed > 0 {
        return Err(
            CliError::user(format!("{failed} checks would be a problem in production"))
                .with_help("each one names the file and the fix above"),
        );
    }
    if strict && warned > 0 {
        return Err(
            CliError::user(format!("{warned} warnings, and --strict was given"))
                .with_help("fix them, or drop --strict to let them through"),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The checks that come from the application
// ---------------------------------------------------------------------------

/// Did the application resolve the profile it was asked for?
///
/// It is asked through the environment, and an application is free to override
/// that in code with `App::new(cfg).profile(..)`. When it does, every other
/// value in the dump is the wrong one, so this is the first check and it is the
/// one that invalidates the rest.
fn profile_check(document: &Value, args: &DeployChecklistArgs) -> Finding {
    let resolved = document
        .get("profile")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let wanted = args.profile.as_str();

    if resolved == wanted {
        let finding = Finding::ok("profile", format!("{resolved} ({PROFILE_ENV}={wanted})"));
        if args.profile == Profile::Production {
            return finding;
        }
        return finding.with_fix(format!(
            "this audited the `{wanted}` profile; a production deployment \
             resolves different values. Drop --profile to check the one you \
             will deploy"
        ));
    }

    Finding::fail(
        "profile",
        format!("resolved to `{resolved}` even with {PROFILE_ENV}={wanted}"),
    )
    .with_fix(
        "the application pins its profile in code; remove the `.profile(..)` \
         call so the environment decides, or every value below is the wrong one",
    )
}

/// One finding per secret field, on where its value came from.
///
/// The interesting column of `--dump-config` is not the value — secrets are
/// redacted by the application before the CLI ever sees them — but the origin. A
/// secret whose origin is a file is a secret in the repository; a secret whose
/// origin is a default is the development key everybody has.
fn secrets(document: &Value) -> Vec<Finding> {
    let entries = document
        .get("entries")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    let secrets: Vec<&Value> = entries
        .iter()
        .filter(|entry| entry.get("secret").and_then(Value::as_bool) == Some(true))
        .collect();

    if secrets.is_empty() {
        return vec![Finding::info(
            "secrets",
            "this application declares no `#[config(secret)]` field",
        )];
    }

    secrets
        .into_iter()
        .map(|entry| {
            let key = entry
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("(unnamed)")
                .to_owned();
            let variable = entry
                .get("env")
                .and_then(Value::as_str)
                .unwrap_or("its environment variable")
                .to_owned();
            let origin = entry.get("origin").and_then(Value::as_str);
            secret_finding(&key, &variable, origin)
        })
        .collect()
}

/// Judge one secret by where its value came from.
fn secret_finding(key: &str, variable: &str, origin: Option<&str>) -> Finding {
    let Some(origin) = origin else {
        return Finding::fail(key, "no source supplies it")
            .with_fix(format!("set {variable} in the deployment's environment"));
    };

    // The spellings are `Origin`'s own `Display`: `env NAME`, `.env NAME`,
    // `code`, `cli --flag`, `default`, `profile default`, and otherwise a path.
    if origin.starts_with("env ") {
        return Finding::ok(key, format!("from the environment ({origin})"));
    }
    if origin.starts_with(".env ") {
        return Finding::warn(key, format!("from a .env file ({origin})")).with_fix(
            "a deployed process does not load .env — the production profile \
             skips it — so set it in the real environment or a secret store",
        );
    }
    if origin == "default" || origin == "profile default" {
        return Finding::fail(key, "still on the default compiled into the binary").with_fix(
            format!("moso config --generate-secret, then set {variable} where this runs"),
        );
    }
    if origin == "code" {
        return Finding::fail(key, "a literal in the source")
            .with_fix(format!("read it from {variable} instead"));
    }
    if origin.starts_with("cli ") {
        return Finding::warn(key, format!("from a command-line flag ({origin})")).with_fix(
            "a flag is visible in `ps` to every user on the host; prefer the \
             environment or a secret store",
        );
    }

    // Anything else is `Origin::File`, rendered as `path` or `path:line`.
    Finding::fail(key, "read from a committed configuration file")
        .in_file(origin.to_owned())
        .with_fix(format!(
            "delete the key from that file and set {variable} in the environment; \
             a secret in a TOML file is a secret in your git history"
        ))
}

/// Is `.env` tracked by git?
///
/// A `.env` that is committed is every secret in it published to everyone with
/// read access to the repository, and it is the single most common way one
/// leaves. `git ls-files` is asked rather than `git status`, because the
/// question is "is this file *in* the index", not "has it changed".
fn dotenv_is_untracked(project: &Project) -> Finding {
    if !project.root.join(".env").exists() {
        return Finding::ok(".env", "no .env in this project");
    }

    let Some(output) = Command::new("git")
        .args(["ls-files", "--", ".env"])
        .current_dir(&project.root)
        .output()
        .ok()
        .filter(|output| output.status.success())
    else {
        return Finding::info(".env", "not a git repository, or git is not installed")
            .with_fix("check by hand that .env is not shipped with the source");
    };

    if String::from_utf8_lossy(&output.stdout).trim().is_empty() {
        return Finding::ok(".env", "present and not tracked by git");
    }
    Finding::fail(".env", "tracked by git")
        .with_fix("git rm --cached .env && echo .env >> .gitignore, then rotate every secret in it")
}

// ---------------------------------------------------------------------------
// The checks that come from the project on disk
// ---------------------------------------------------------------------------

/// One line of the project that a scan matched.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hit {
    /// Where it was found, relative to the project root.
    file: String,
    /// The one-based line number.
    line: usize,
    /// The line, trimmed.
    text: String,
}

impl Hit {
    /// `src/lib.rs:82`, which is what an editor jumps to.
    fn location(&self) -> String {
        format!("{}:{}", self.file, self.line)
    }

    /// Whether the line switches the thing it names on.
    ///
    /// Both spellings the framework accepts — `expose_docs: true` in a struct
    /// literal and `.expose_docs(true)` on a builder — put `true` after the key,
    /// and neither `false` nor an absent value does.
    fn is_enabled(&self) -> bool {
        self.text.contains("true")
    }
}

/// The project's own source, read once.
///
/// Read once and searched many times: eight checks over the same handful of
/// files, and re-reading them per check would be eight walks of `src/` to answer
/// eight questions about the same bytes.
#[derive(Debug, Default)]
struct Source {
    /// Every non-comment line of `src/**/*.rs` and `config/*.toml`.
    lines: Vec<Hit>,
}

impl Source {
    /// Read the files a deployment decision can be hiding in.
    ///
    /// `src/` and `config/` only. `target/` is build output, `tests/` does not
    /// ship, and walking either would turn a fast check into a slow one for
    /// findings that could not reach production.
    fn read(root: &Path) -> Self {
        let mut lines = Vec::new();
        collect(root, &root.join("src"), "rs", "//", &mut lines);
        collect(root, &root.join("config"), "toml", "#", &mut lines);
        Self { lines }
    }

    /// Every line mentioning `needle`.
    fn find(&self, needle: &str) -> Vec<&Hit> {
        self.lines
            .iter()
            .filter(|hit| hit.text.contains(needle))
            .collect()
    }

    /// A security toggle that must be off, and is off unless the project says
    /// otherwise.
    ///
    /// `expose_internal_errors` is `false` in every profile and `expose_docs` is
    /// `false` in production, both as framework defaults, so the only way either
    /// is on is a line in this project that turns it on. Finding no such line is
    /// therefore a real answer rather than an absence of one.
    fn toggle_is_off(&self, key: &str, when_off: &str, fix: &str) -> Finding {
        match self.find(key).into_iter().find(|hit| hit.is_enabled()) {
            Some(hit) => Finding::fail(key, "switched on by this project")
                .in_file(hit.location())
                .with_fix(fix.to_owned()),
            None => Finding::ok(key, when_off),
        }
    }

    /// Whether the deployment believes `X-Forwarded-For`.
    ///
    /// Warned rather than failed in both directions, because only the person
    /// deploying knows which is right: an empty list behind a load balancer
    /// means every client IP in the logs is the balancer's, and a populated one
    /// on a directly-exposed instance means any client can claim any address.
    fn trusted_proxies(&self) -> Finding {
        match self.find("trusted_proxies").first() {
            Some(hit) => {
                Finding::ok("trusted_proxies", "configured by this project").in_file(hit.location())
            }
            None => Finding::warn(
                "trusted_proxies",
                "empty — X-Forwarded-For is not believed, so the peer address is \
                 what rate limits and audit logs record",
            )
            .with_fix(
                "set it to your load balancer's CIDR if there is one in front of \
                 this; leaving it empty is correct for a directly-exposed instance",
            ),
        }
    }

    /// CORS, and the one combination that is never right.
    fn cors(&self) -> Vec<Finding> {
        let any = self.find("any_origin");
        if any.is_empty() {
            return vec![Finding::ok(
                "cors",
                "no any-origin policy; off by default, and origins are listed \
                 when it is on",
            )];
        }

        // `CorsConfig::any_origin().allow_credentials(true)` is refused by
        // `CorsConfig::validate` at boot, so this cannot reach production — but
        // it can waste an afternoon at 3 a.m., and saying so before the deploy
        // is cheaper than reading a boot error after it.
        let credentials = self
            .find("allow_credentials")
            .into_iter()
            .find(|hit| hit.is_enabled());
        let Some(credentials) = credentials else {
            return vec![
                Finding::warn("cors", "any origin is allowed")
                    .in_file(any[0].location())
                    .with_fix(
                        "list the origins that need it: \
                         CorsConfig::allow_origins([\"https://app.example\"])",
                    ),
            ];
        };

        vec![
            Finding::fail("cors", "any origin, with credentials")
                .in_file(format!("{}, {}", any[0].location(), credentials.location()))
                .with_fix(
                    "list the origins: a wildcard with credentials is a boot \
                     error, so this application will refuse to start",
                ),
        ]
    }

    /// The drain window, against the deadline the orchestrator enforces.
    fn shutdown_grace(&self) -> Finding {
        let Some(hit) = self.find("shutdown_grace").first().copied() else {
            return Finding::ok(
                "shutdown grace",
                format!("25 s by default, under the usual {USUAL_KILL_TIMEOUT} s kill timeout"),
            );
        };

        match seconds(&hit.text) {
            Some(seconds) if seconds >= USUAL_KILL_TIMEOUT => Finding::fail(
                "shutdown grace",
                format!("{seconds} s, at or over the usual {USUAL_KILL_TIMEOUT} s kill timeout"),
            )
            .in_file(hit.location())
            .with_fix(
                "set it under your platform's termination grace period, or the \
                 process is killed mid-drain and the grace buys nothing",
            ),
            Some(seconds) => Finding::ok(
                "shutdown grace",
                format!("{seconds} s, under the usual {USUAL_KILL_TIMEOUT} s kill timeout"),
            )
            .in_file(hit.location()),
            None => Finding::info("shutdown grace", "set to something this scan cannot read")
                .in_file(hit.location())
                .with_fix(format!(
                    "check by hand that it is under {USUAL_KILL_TIMEOUT} s"
                )),
        }
    }

    /// The liveness and readiness probes.
    ///
    /// `App::build()` mounts both unconditionally on the outer router, outside
    /// the middleware stack, so they are there unless this project moved them —
    /// and they are mounted outside the application router, which is why they do
    /// not appear in `--dump-routes` and this is read from source instead.
    fn probes(&self) -> Finding {
        let moved: Vec<&Hit> = self
            .find("health_path")
            .into_iter()
            .chain(self.find("ready_path"))
            .collect();

        match moved.first() {
            None => Finding::ok(
                "/healthz, /readyz",
                "mounted by App::build(), outside the middleware stack",
            ),
            Some(hit) => Finding::warn("/healthz, /readyz", "mounted at a path this project sets")
                .in_file(hit.location())
                .with_fix(
                    "point the deployment's liveness and readiness probes at the \
                     configured paths, and make sure both start with `/`",
                ),
        }
    }
}

/// Walk `directory`, collecting every non-comment line of every `extension`
/// file.
///
/// Comments are dropped because the density of doc comments in this codebase's
/// house style guarantees that the paragraph above a field mentions the field,
/// and a checklist that reported the explanation of a setting as the setting
/// would be wrong on nearly every project. `comment` is the prefix for the
/// language being read — `//` in Rust, `#` in TOML — because `#` in a `.rs`
/// file starts an attribute rather than a comment.
///
/// It is a line-based rule and not a parser: a `/* */` block still counts,
/// which is why every finding names its file and line rather than asking to be
/// believed.
fn collect(root: &Path, directory: &Path, extension: &str, comment: &str, out: &mut Vec<Hit>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();

    for path in paths {
        if path.is_dir() {
            collect(root, &path, extension, comment, out);
            continue;
        }
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some(extension) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let file = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with(comment) {
                continue;
            }
            out.push(Hit {
                file: file.clone(),
                line: index + 1,
                text: trimmed.to_owned(),
            });
        }
    }
}

/// The seconds in a `Duration::from_secs(N)`, or a bare `N` after an `=`.
///
/// Covers `shutdown_grace: Duration::from_secs(45)` and `shutdown_grace = 45`,
/// which are the two ways it is written. Anything else returns `None` and is
/// reported as unreadable rather than as a number that was guessed.
fn seconds(text: &str) -> Option<u64> {
    if let Some(rest) = text.split("from_secs(").nth(1) {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        return digits.parse().ok();
    }
    let rest = text.split('=').nth(1)?.trim();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::AppArgs;

    fn source(lines: &[(&str, usize, &str)]) -> Source {
        Source {
            lines: lines
                .iter()
                .map(|(file, line, text)| Hit {
                    file: (*file).to_owned(),
                    line: *line,
                    text: (*text).to_owned(),
                })
                .collect(),
        }
    }

    fn args(profile: Profile) -> DeployChecklistArgs {
        DeployChecklistArgs {
            profile,
            strict: false,
            app: AppArgs::default(),
        }
    }

    // ── the application's answers ───────────────────────────────────────────

    #[test]
    fn a_profile_the_application_overrode_in_code_is_the_first_failure() {
        let document = serde_json::json!({"profile": "dev", "entries": []});
        let finding = profile_check(&document, &args(Profile::Production));
        assert_eq!(finding.level, Level::Fail);
        assert!(finding.detail.contains("dev"), "{}", finding.detail);
        assert!(finding.fix.is_some_and(|fix| fix.contains(".profile(")));
    }

    #[test]
    fn the_profile_that_was_asked_for_passes() {
        let document = serde_json::json!({"profile": "production", "entries": []});
        let finding = profile_check(&document, &args(Profile::Production));
        assert_eq!(finding.level, Level::Ok);
        assert!(finding.fix.is_none());
    }

    #[test]
    fn auditing_a_non_production_profile_says_it_audited_the_wrong_one() {
        let document = serde_json::json!({"profile": "dev", "entries": []});
        let finding = profile_check(&document, &args(Profile::Dev));
        assert_eq!(finding.level, Level::Ok);
        assert!(finding.fix.is_some_and(|fix| fix.contains("dev")));
    }

    #[test]
    fn a_secret_is_judged_by_where_its_value_came_from() {
        let cases = [
            (Some("env SHOP__SECRET_KEY"), Level::Ok),
            (Some(".env SHOP__SECRET_KEY"), Level::Warn),
            (Some("cli --secret-key"), Level::Warn),
            (Some("default"), Level::Fail),
            (Some("profile default"), Level::Fail),
            (Some("code"), Level::Fail),
            (Some("config/production.toml:14"), Level::Fail),
            (None, Level::Fail),
        ];
        for (origin, expected) in cases {
            let finding = secret_finding("secret_key", "SHOP__SECRET_KEY", origin);
            assert_eq!(finding.level, expected, "origin {origin:?}");
            assert!(
                finding.level == Level::Ok || finding.fix.is_some(),
                "origin {origin:?} has no fix"
            );
        }
    }

    #[test]
    fn a_secret_from_a_committed_file_names_the_file() {
        let finding = secret_finding(
            "secret_key",
            "SHOP__SECRET_KEY",
            Some("config/prod.toml:14"),
        );
        assert_eq!(finding.file.as_deref(), Some("config/prod.toml:14"));
        assert!(finding.detail_line().contains("config/prod.toml:14"));
    }

    #[test]
    fn an_application_with_no_secret_fields_is_reported_rather_than_passed() {
        // Silence would read as "checked, fine". It was not checked: there was
        // nothing to check, and those are different answers.
        let findings = secrets(&serde_json::json!({"entries": [
            {"key": "greeting", "env": "SHOP__GREETING", "origin": "default", "secret": false}
        ]}));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].level, Level::Info);
    }

    #[test]
    fn every_secret_gets_its_own_finding() {
        let findings = secrets(&serde_json::json!({"entries": [
            {"key": "secret_key", "env": "S__K", "origin": "env S__K", "secret": true},
            {"key": "api_token", "env": "S__T", "origin": "default", "secret": true},
        ]}));
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].level, Level::Ok);
        assert_eq!(findings[1].level, Level::Fail);
    }

    // ── the project's answers ───────────────────────────────────────────────

    #[test]
    fn a_toggle_nobody_switched_on_is_off() {
        let finding = source(&[]).toggle_is_off("expose_docs", "off", "remove it");
        assert_eq!(finding.level, Level::Ok);
        assert_eq!(finding.detail, "off");
    }

    #[test]
    fn a_toggle_switched_on_fails_and_names_the_line() {
        let finding = source(&[("src/lib.rs", 82, "expose_internal_errors: true,")]).toggle_is_off(
            "expose_internal_errors",
            "off",
            "remove it",
        );
        assert_eq!(finding.level, Level::Fail);
        assert_eq!(finding.file.as_deref(), Some("src/lib.rs:82"));
    }

    #[test]
    fn a_toggle_explicitly_set_to_false_is_not_a_finding() {
        let finding = source(&[("src/lib.rs", 82, "expose_docs: false,")]).toggle_is_off(
            "expose_docs",
            "off",
            "remove it",
        );
        assert_eq!(finding.level, Level::Ok);
    }

    #[test]
    fn both_spellings_of_switching_something_on_are_recognised() {
        for line in ["expose_docs: true,", ".expose_docs(true)"] {
            let finding =
                source(&[("src/lib.rs", 1, line)]).toggle_is_off("expose_docs", "off", "fix");
            assert_eq!(finding.level, Level::Fail, "{line}");
        }
    }

    #[test]
    fn an_unset_trusted_proxies_is_a_warning_that_argues_both_ways() {
        let finding = source(&[]).trusted_proxies();
        assert_eq!(finding.level, Level::Warn);
        let fix = finding.fix.expect("a fix");
        assert!(fix.contains("load balancer"), "{fix}");
        assert!(fix.contains("directly-exposed"), "{fix}");
    }

    #[test]
    fn any_origin_with_credentials_is_the_one_cors_failure() {
        let findings = source(&[
            ("src/lib.rs", 10, "CorsConfig::any_origin()"),
            ("src/lib.rs", 11, ".allow_credentials(true)"),
        ])
        .cors();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].level, Level::Fail);
        assert!(
            findings[0]
                .fix
                .as_ref()
                .is_some_and(|fix| fix.contains("boot"))
        );
    }

    #[test]
    fn any_origin_without_credentials_is_only_a_warning() {
        let findings = source(&[("src/lib.rs", 10, "CorsConfig::any_origin()")]).cors();
        assert_eq!(findings[0].level, Level::Warn);
    }

    #[test]
    fn no_cors_at_all_is_the_safe_default() {
        let findings = source(&[]).cors();
        assert_eq!(findings[0].level, Level::Ok);
    }

    #[test]
    fn a_grace_at_or_over_the_kill_timeout_fails() {
        for (line, level) in [
            ("shutdown_grace: Duration::from_secs(45),", Level::Fail),
            ("shutdown_grace: Duration::from_secs(30),", Level::Fail),
            ("shutdown_grace: Duration::from_secs(20),", Level::Ok),
            ("shutdown_grace = 45", Level::Fail),
            ("shutdown_grace = 20", Level::Ok),
        ] {
            let finding = source(&[("src/lib.rs", 5, line)]).shutdown_grace();
            assert_eq!(finding.level, level, "{line}");
        }
    }

    #[test]
    fn an_unset_grace_is_the_frameworks_own_twenty_five_seconds() {
        let finding = source(&[]).shutdown_grace();
        assert_eq!(finding.level, Level::Ok);
        assert!(finding.detail.contains("25 s"), "{}", finding.detail);
    }

    #[test]
    fn a_grace_this_scan_cannot_read_is_admitted_rather_than_guessed() {
        let finding =
            source(&[("src/lib.rs", 5, "shutdown_grace: grace_from(config),")]).shutdown_grace();
        assert_eq!(finding.level, Level::Info);
        assert!(finding.fix.is_some());
    }

    #[test]
    fn the_probes_are_the_frameworks_until_the_project_moves_them() {
        assert_eq!(source(&[]).probes().level, Level::Ok);
        let moved = source(&[("src/lib.rs", 9, "health_path: \"/_live\".to_owned(),")]).probes();
        assert_eq!(moved.level, Level::Warn);
        assert_eq!(moved.file.as_deref(), Some("src/lib.rs:9"));
    }

    // ── the scanner ─────────────────────────────────────────────────────────

    #[test]
    fn comments_are_not_configuration() {
        let scratch = std::env::temp_dir().join(format!("moso-deploy-{}", std::process::id()));
        let src = scratch.join("src");
        std::fs::create_dir_all(&src).expect("scratch");
        std::fs::write(
            src.join("lib.rs"),
            "/// Whether expose_docs: true is a good idea.\n\
             //! expose_docs: true\n\
             // expose_docs: true\n\
             pub const X: bool = false;\n",
        )
        .expect("write");

        let source = Source::read(&scratch);
        assert!(
            source.find("expose_docs").is_empty(),
            "a doc comment about a setting is not the setting"
        );
        assert_eq!(source.lines.len(), 1);
        assert_eq!(source.lines[0].file, "src/lib.rs");
        assert_eq!(source.lines[0].line, 4);

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn nested_modules_and_config_files_are_both_read() {
        let scratch = std::env::temp_dir().join(format!("moso-deploy-n-{}", std::process::id()));
        std::fs::create_dir_all(scratch.join("src/routes")).expect("scratch");
        std::fs::create_dir_all(scratch.join("config")).expect("scratch");
        std::fs::write(scratch.join("src/routes/users.rs"), "expose_docs: true\n").expect("write");
        std::fs::write(
            scratch.join("config/production.toml"),
            "# expose_docs = true\nexpose_docs = true\n",
        )
        .expect("write");
        // Not read: it is build output, and it cannot reach production.
        std::fs::create_dir_all(scratch.join("target")).expect("scratch");
        std::fs::write(scratch.join("target/x.rs"), "expose_docs: true\n").expect("write");

        let source = Source::read(&scratch);
        let files: Vec<&str> = source
            .find("expose_docs")
            .iter()
            .map(|h| h.file.as_str())
            .collect();
        assert_eq!(files.len(), 2, "{files:?}");
        assert!(
            files.iter().any(|file| file.contains("users.rs")),
            "{files:?}"
        );
        assert!(
            files.iter().any(|file| file.contains("production.toml")),
            "{files:?}"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_project_with_no_src_directory_scans_to_nothing_rather_than_failing() {
        assert!(
            Source::read(Path::new("/definitely/not/a/project/4f2a"))
                .lines
                .is_empty()
        );
    }

    #[test]
    fn seconds_are_read_from_both_spellings_and_nothing_else() {
        assert_eq!(
            seconds("shutdown_grace: Duration::from_secs(45),"),
            Some(45)
        );
        assert_eq!(seconds("shutdown_grace = 45"), Some(45));
        assert_eq!(seconds("shutdown_grace = \"45s\""), None);
        assert_eq!(seconds("shutdown_grace: computed()"), None);
    }

    // ── the verdict ─────────────────────────────────────────────────────────

    #[test]
    fn any_failure_exits_non_zero_so_this_can_gate_a_deploy() {
        assert!(verdict(0, 0, false).is_ok());
        assert!(verdict(0, 3, false).is_ok());
        assert_eq!(verdict(1, 0, false).expect_err("fails").fault.code(), 1);
    }

    #[test]
    fn strict_promotes_warnings_and_says_which_flag_did_it() {
        let error = verdict(0, 2, true).expect_err("strict fails on a warning");
        assert!(error.message.contains("2 warnings"), "{}", error.message);
        assert!(error.help.is_some_and(|help| help.contains("--strict")));
    }
}
