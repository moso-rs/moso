//! `moso config --check` — the configuration mistakes that are silent.
//!
//! # What "silent" means, and why this command exists
//!
//! A configuration mistake that stops the boot needs no tool: the application
//! already prints the key, its type, every environment spelling that would have
//! supplied it and the line to write. This command is about the other kind —
//! the mistakes that let the process start and then behave as if you had never
//! configured anything.
//!
//! | Finding | Why it is silent |
//! | --- | --- |
//! | `SHOP__GRETING=hei` | Nothing reads it, nothing warns, the default wins |
//! | `.env.example` has drifted | The next person configures the wrong keys |
//! | a secret came out of a committed file | The secret is in the repository, and in every clone |
//! | a key in `config/*.toml` no field reads | The same typo, in the file people trust most |
//!
//! # It resolves the configuration by asking the application
//!
//! Every input comes from the same two dumps `moso config` already uses:
//! `--dump-config` for the resolved keys and the origin of each, and
//! `--dump-env-example` for what the committed example *should* say. Nothing
//! here parses Rust and nothing here re-implements the loader, so a check
//! cannot disagree with the application about which value won.
//!
//! That has one consequence worth stating plainly. `src/main.rs` builds the
//! application before it answers a dump, so **a key with no value and no
//! default, or a value that fails its type, stops the dump itself**. The check
//! reports that as a failure and lets the application's own boot report — which
//! is better than anything this command could reconstruct — stand as the
//! explanation. It is not a case this file can reach with a document in hand:
//! a null origin on a *successful* boot means the field is `Option<T>`, and
//! reporting that as missing would be inventing a problem.
//!
//! # Exit code
//!
//! 1 when anything failed, 0 when only warnings were printed. The split is by
//! whether a human had to decide: a key nothing reads, a drifted example and a
//! secret in a tracked file are facts, and each is a failure. "This looks like
//! a file you would normally commit" is a judgement, and warns.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::cli::ConfigArgs;
use crate::exit::{CliError, Fault, Outcome};
use crate::project::{Dump, Project};
use crate::ui::{Level, Ui};

/// The environment variables that steer the loader rather than the application.
///
/// They carry no application prefix, so they never look like a mistyped key —
/// but they are listed here so that a reader of this file can see the set was
/// considered rather than forgotten.
const LOADER_VARIABLES: &[&str] = &["MOSO_PROFILE", "MOSO_CONFIG_PREFIX", "MOSO_CONFIG_DIR"];

/// Where the committed TOML layers live, unless `MOSO_CONFIG_DIR` moves them.
const CONFIG_DIR: &str = "config";

/// The file `--env-example` regenerates.
const ENV_EXAMPLE: &str = ".env.example";

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

/// One thing the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// A stable slug, so a script can branch on the kind of problem.
    pub check: &'static str,
    /// Whether this fails the command or only warns.
    pub level: Level,
    /// The key, variable or file the finding is about.
    pub subject: String,
    /// One sentence, lower case, no trailing period.
    pub message: String,
    /// What to do about it.
    pub fix: Option<String>,
}

impl Finding {
    /// A failure: something is definitely wrong.
    fn fail(check: &'static str, subject: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            check,
            level: Level::Fail,
            subject: subject.into(),
            message: message.into(),
            fix: None,
        }
    }

    /// A warning: something is worth a human's attention.
    fn warn(check: &'static str, subject: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            check,
            level: Level::Warn,
            subject: subject.into(),
            message: message.into(),
            fix: None,
        }
    }

    /// Attach the next step.
    #[must_use]
    fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    /// The `--json` rendering.
    fn to_json(&self) -> Value {
        serde_json::json!({
            "check": self.check,
            "level": self.level.as_str(),
            "subject": self.subject,
            "message": self.message,
            "fix": self.fix,
        })
    }
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

/// Run `moso config --check`.
///
/// # Errors
/// [`Fault::User`] when the application will not
/// resolve its configuration at all, and again when any check failed —
/// `--check` is meant to gate CI, so a finding is an exit code and not a
/// paragraph.
pub fn run(ui: &Ui, project: &Project, args: &ConfigArgs) -> Outcome<()> {
    let resolved = project
        .dump(&args.app, Dump::Config)
        .map_err(did_not_resolve)?;
    let document: Value = serde_json::from_str(&resolved).map_err(|error| {
        CliError::user(format!(
            "the application's `--dump-config` output is not JSON: {error}"
        ))
        .with_help("everything except the document must go to stderr")
    })?;
    let entries = entries(&document)?;
    let profile = document
        .get("profile")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();

    let generated = project.dump(&args.app, Dump::EnvExample)?;
    let example = parse_env_example(&generated);

    let mut findings = Vec::new();
    findings.extend(drifted_example(project, &generated));
    findings.extend(unread_environment(
        &entries,
        &example,
        &read_environment(project),
    ));
    findings.extend(unread_file_keys(
        &entries,
        &read_toml_layers(project, &profile),
    ));
    findings.extend(exposed_secrets(project, &entries));

    report(ui, &profile, &findings)
}

/// Keep the application's own explanation and say what it covers.
///
/// The message is left exactly as [`Project::dump`] wrote it — it names the
/// package and the flag — and only the help line is replaced, because the two
/// conditions that produce it are precisely the two this command would
/// otherwise have to guess at.
fn did_not_resolve(error: CliError) -> CliError {
    if error.fault != Fault::User {
        return error;
    }
    CliError::user(error.message).with_help(
        "the application printed the reason above: a key with no value and no default, or a \
         value that fails its type, stops the boot before any of it can be checked",
    )
}

/// Print the findings and turn them into an exit code.
fn report(ui: &Ui, profile: &str, findings: &[Finding]) -> Outcome<()> {
    let failures = findings
        .iter()
        .filter(|finding| finding.level == Level::Fail)
        .count();
    let warnings = findings.len() - failures;

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": failures == 0,
            "profile": profile,
            "failures": failures,
            "warnings": warnings,
            "findings": findings.iter().map(Finding::to_json).collect::<Vec<_>>(),
        }));
    } else {
        ui.blank();
        ui.heading(&format!("  profile: {profile}"));
        ui.blank();
        if findings.is_empty() {
            ui.status(Level::Ok, "configuration", "(nothing to report)");
        }
        for finding in findings {
            ui.status(finding.level, &finding.subject, &finding.message);
            if let Some(fix) = &finding.fix {
                ui.fix(fix);
            }
        }
        ui.blank();
    }

    if failures == 0 {
        return Ok(());
    }
    let one = failures == 1;
    Err(CliError::user(format!(
        "{failures} configuration {} above",
        if one { "problem" } else { "problems" }
    ))
    .with_help(format!(
        "fix {}, or run `moso config` to see every key and where it came from",
        if one { "it" } else { "them" }
    )))
}

// ---------------------------------------------------------------------------
// The resolved document
// ---------------------------------------------------------------------------

/// One resolved key, as `--dump-config` described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The dotted key: `database.url`.
    pub key: String,
    /// The canonical prefixed environment spelling: `SHOP__DATABASE__URL`.
    pub env: String,
    /// Whether the field is a secret. Its value is already redacted.
    pub secret: bool,
    /// The rendered origin, or `None` when no source supplied a value.
    pub origin: Option<String>,
}

/// Read the `entries` array.
fn entries(document: &Value) -> Outcome<Vec<Entry>> {
    let array = document
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CliError::user("the application's `--dump-config` output has no `entries` array")
                .with_help("compare src/dump.rs with the one `moso new` writes")
        })?;

    Ok(array
        .iter()
        .map(|entry| Entry {
            key: string(entry, "key"),
            env: string(entry, "env"),
            secret: entry
                .get("secret")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            origin: entry
                .get("origin")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
        .collect())
}

/// A string field, or the empty string.
fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Where a value came from, as the seven `Origin` renderings spell it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// No source supplied a value.
    Unset,
    /// A file: `config/production.toml:8`, or `.env`.
    File(String),
    /// Anything else — the environment, the command line, code, a default.
    Elsewhere,
}

/// Classify one rendered origin.
///
/// The renderings are fixed by `Origin`'s `Display`, and only the two file
/// shapes matter here: a value that lives in a file is a value that can be
/// committed, and that is the whole point of the secret check.
#[must_use]
pub fn source_of(origin: Option<&str>) -> Source {
    let Some(origin) = origin else {
        return Source::Unset;
    };
    if let Some(name) = origin.strip_prefix(".env ") {
        let _ = name;
        return Source::File(".env".to_owned());
    }
    if origin == "code"
        || origin == "default"
        || origin == "profile default"
        || origin.starts_with("env ")
        || origin.starts_with("cli ")
    {
        return Source::Elsewhere;
    }
    // What is left is `Origin::File`, whose rendering is the path, optionally
    // followed by `:<line>`.
    let path = origin
        .rsplit_once(':')
        .filter(|(_, line)| line.chars().all(|digit| digit.is_ascii_digit()))
        .map_or(origin, |(path, _)| path);
    Source::File(path.to_owned())
}

// ---------------------------------------------------------------------------
// `.env.example` drift
// ---------------------------------------------------------------------------

/// One key of a rendered `.env.example`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExampleKey {
    /// The variable name, which is the alias when the field declares one.
    pub name: String,
    /// The default, or the empty string.
    pub value: String,
}

/// Read the keys out of a rendered `.env.example`.
///
/// The format is the renderer's: comment lines, then `NAME=value`, blocks
/// separated by a blank line. Only the assignments matter here.
#[must_use]
pub fn parse_env_example(text: &str) -> Vec<ExampleKey> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| ExampleKey {
            name: name.trim().to_owned(),
            value: value.to_owned(),
        })
        .filter(|key| !key.name.is_empty())
        .collect()
}

/// The one line that repairs a drifted or missing `.env.example`.
pub const ENV_EXAMPLE_FIX: &str = "moso config --env-example --out .env.example";

/// Compare the committed `.env.example` with the one the type generates.
fn drifted_example(project: &Project, generated: &str) -> Vec<Finding> {
    let path = project.root.join(ENV_EXAMPLE);
    let Ok(committed) = std::fs::read_to_string(&path) else {
        return vec![
            Finding::warn(
                "env_example_missing",
                ENV_EXAMPLE,
                "there is no committed example to compare the Config type against",
            )
            .with_fix(ENV_EXAMPLE_FIX),
        ];
    };

    match example_drift(&committed, generated) {
        None => Vec::new(),
        Some(detail) => {
            vec![Finding::fail("env_example_drift", ENV_EXAMPLE, detail).with_fix(ENV_EXAMPLE_FIX)]
        }
    }
}

/// Whether a committed `.env.example` still says what the `Config` type does.
///
/// `None` when they agree; `Some(detail)` naming the difference when they do
/// not. **This is the single home of the `env_example_drift` rule.** It is
/// reported by two commands — as a lint by [`check`](super::check) and as a
/// finding by `moso config --check` — and they both call this rather than each
/// holding a comparison of its own. Two commands that report the same named
/// problem and disagree about whether it is present is worse than either of
/// them not having the check at all.
///
/// The comparison goes through [`normalise`], so trailing whitespace and a
/// missing final newline are not drift: an editor that strips them on save must
/// not turn a green CI job red.
///
/// # Examples
///
/// ```ignore
/// // Private to the `moso` binary; shown here rather than run.
/// assert!(example_drift("A=1\n", "A=1").is_none());
/// assert!(example_drift("A=1\n", "A=1\nB=2\n").is_some());
/// ```
#[must_use]
pub fn example_drift(committed: &str, generated: &str) -> Option<String> {
    if normalise(committed) == normalise(generated) {
        return None;
    }

    let mine: BTreeSet<String> = parse_env_example(committed)
        .into_iter()
        .map(|key| key.name)
        .collect();
    let theirs: BTreeSet<String> = parse_env_example(generated)
        .into_iter()
        .map(|key| key.name)
        .collect();

    let missing: Vec<&String> = theirs.difference(&mine).collect();
    let extra: Vec<&String> = mine.difference(&theirs).collect();

    Some(match (missing.is_empty(), extra.is_empty()) {
        // The key sets agree, so what moved was a default or a comment. Still
        // drift, because the comments are the documentation an operator reads.
        (true, true) => "the committed file has drifted from the Config type".to_owned(),
        (false, true) => format!("{} missing: {}", counted(missing.len()), listed(&missing)),
        (true, false) => format!(
            "{} no longer declared: {}",
            counted(extra.len()),
            listed(&extra)
        ),
        (false, false) => format!(
            "{} missing and {} no longer declared: {}, {}",
            counted(missing.len()),
            counted(extra.len()),
            listed(&missing),
            listed(&extra)
        ),
    })
}

/// Ignore trailing whitespace and a missing final newline.
fn normalise(text: &str) -> String {
    let mut out: String = text
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// `1 key` or `3 keys`.
fn counted(keys: usize) -> String {
    format!("{keys} key{}", if keys == 1 { "" } else { "s" })
}

/// At most four names, then a count.
fn listed(names: &[&String]) -> String {
    const SHOWN: usize = 4;
    let head: Vec<&str> = names.iter().take(SHOWN).map(|name| name.as_str()).collect();
    if names.len() <= SHOWN {
        head.join(", ")
    } else {
        format!("{} and {} more", head.join(", "), names.len() - SHOWN)
    }
}

// ---------------------------------------------------------------------------
// Environment keys nothing reads
// ---------------------------------------------------------------------------

/// One environment variable, and where it was set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    /// The name.
    pub name: String,
    /// `environment`, or the path of the `.env` it was read from.
    pub whence: String,
}

/// The prefix every environment spelling of this application's keys carries.
///
/// Derived rather than configured: `--dump-config` gives both the dotted key
/// and its canonical environment spelling for every field, and the prefix is
/// what is left when the one is stripped from the other. An application with no
/// prefix reads unprefixed names, and then every variable in the environment is
/// a candidate — which is not a check, so it is skipped.
#[must_use]
pub fn prefix_of(entries: &[Entry]) -> Option<String> {
    let mut agreed: Option<String> = None;
    for entry in entries {
        let suffix = entry.key.to_uppercase().replace('.', "__");
        let prefix = entry.env.strip_suffix(&suffix)?;
        if prefix.is_empty() {
            return None;
        }
        match &agreed {
            Some(known) if known != prefix => return None,
            Some(_) => {}
            None => agreed = Some(prefix.to_owned()),
        }
    }
    agreed
}

/// Every prefixed variable that no field reads.
fn unread_environment(
    entries: &[Entry],
    example: &[ExampleKey],
    variables: &[Variable],
) -> Vec<Finding> {
    let Some(prefix) = prefix_of(entries) else {
        return Vec::new();
    };

    // Both spellings a field answers to: the canonical prefixed name, and the
    // `#[config(env = ..)]` alias, which is the name the rendered example uses
    // in place of it.
    let mut known: BTreeSet<&str> = entries.iter().map(|entry| entry.env.as_str()).collect();
    known.extend(example.iter().map(|key| key.name.as_str()));

    variables
        .iter()
        .filter(|variable| variable.name.starts_with(&prefix))
        .filter(|variable| !known.contains(variable.name.as_str()))
        // `${KEY}_FILE` is the mounted-secret convention: a real spelling of a
        // real key, and not a typo.
        .filter(|variable| {
            variable
                .name
                .strip_suffix("_FILE")
                .is_none_or(|stem| !known.contains(stem))
        })
        .map(|variable| {
            let finding = Finding::fail(
                "unread_environment_key",
                variable.name.clone(),
                format!(
                    "set in {}, but no field of the Config type reads it",
                    variable.whence
                ),
            );
            match nearest(&variable.name, &known) {
                Some(near) => finding.with_fix(format!("did you mean {near}?")),
                None => finding.with_fix(
                    "remove it, or add the field to the Config type it was meant for",
                ),
            }
        })
        .collect()
}

/// Collect the candidate variables: the process environment, then `.env`.
///
/// The process environment is exactly what the application inherited when the
/// CLI ran it, so a variable this sees is a variable that was in scope for the
/// resolution being checked.
fn read_environment(project: &Project) -> Vec<Variable> {
    let mut found: Vec<Variable> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| !LOADER_VARIABLES.contains(&name.as_str()))
        .map(|name| Variable {
            name,
            whence: "the environment".to_owned(),
        })
        .collect();

    if let Some(path) = dotenv_path(&project.root)
        && let Ok(text) = std::fs::read_to_string(&path)
    {
        let whence = path
            .strip_prefix(&project.root)
            .unwrap_or(&path)
            .display()
            .to_string();
        for name in dotenv_names(&text) {
            found.push(Variable {
                name,
                whence: whence.clone(),
            });
        }
    }

    found.sort_by(|left, right| left.name.cmp(&right.name));
    found.dedup_by(|left, right| left.name == right.name);
    found
}

/// The `.env` the loader would find: the first one at or above the package.
fn dotenv_path(root: &Path) -> Option<PathBuf> {
    root.ancestors()
        .map(|directory| directory.join(".env"))
        .find(|candidate| candidate.is_file())
}

/// The names assigned in a `.env`, ignoring comments and blank lines.
#[must_use]
pub fn dotenv_names(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.strip_prefix("export ").unwrap_or(line))
        .filter_map(|line| line.split_once('='))
        .map(|(name, _)| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect()
}

/// The closest known spelling, when one is close enough to be a typo.
///
/// The threshold is deliberately tight: a suggestion that is wrong is worse
/// than no suggestion, because it sends the reader to edit the wrong line.
fn nearest<'a>(name: &str, known: &BTreeSet<&'a str>) -> Option<&'a str> {
    let budget = (name.len() / 4).clamp(1, 3);
    known
        .iter()
        .map(|candidate| (distance(name, candidate), *candidate))
        .filter(|(distance, _)| *distance <= budget)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// Levenshtein distance, two rows at a time.
fn distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0_usize; right.len() + 1];

    for (row, from) in left.iter().enumerate() {
        current[0] = row + 1;
        for (column, to) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(from != to);
            let insertion = current[column] + 1;
            let deletion = previous[column + 1] + 1;
            current[column + 1] = substitution.min(insertion).min(deletion);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

// ---------------------------------------------------------------------------
// TOML keys nothing reads
// ---------------------------------------------------------------------------

/// One committed TOML layer, flattened to dotted keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    /// The path, relative to the project root.
    pub path: String,
    /// Every leaf key it sets, dotted.
    pub keys: Vec<String>,
}

/// Read `config/default.toml` and the profile's own file.
fn read_toml_layers(project: &Project, profile: &str) -> Vec<Layer> {
    let directory = std::env::var_os("MOSO_CONFIG_DIR")
        .map_or_else(|| PathBuf::from(CONFIG_DIR), PathBuf::from);

    [format!("{profile}.toml"), "default.toml".to_owned()]
        .into_iter()
        .filter_map(|name| {
            let relative = directory.join(&name);
            let text = std::fs::read_to_string(project.root.join(&relative)).ok()?;
            let value: toml::Value = toml::from_str(&text).ok()?;
            let mut keys = Vec::new();
            flatten(&value, String::new(), &mut keys);
            Some(Layer {
                path: relative.display().to_string(),
                keys,
            })
        })
        .collect()
}

/// Flatten a TOML document to dotted leaf keys.
fn flatten(value: &toml::Value, prefix: String, out: &mut Vec<String>) {
    match value {
        toml::Value::Table(table) => {
            for (name, child) in table {
                let key = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                flatten(child, key, out);
            }
        }
        // An array is a `Vec<T>` field and a scalar is a scalar: both are
        // leaves, and neither has keys of its own that a field could name.
        _ if !prefix.is_empty() => out.push(prefix),
        _ => {}
    }
}

/// Every committed TOML key that no field reads.
fn unread_file_keys(entries: &[Entry], layers: &[Layer]) -> Vec<Finding> {
    let known: BTreeSet<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
    layers
        .iter()
        .flat_map(|layer| {
            layer
                .keys
                .iter()
                .filter(|key| !known.contains(key.as_str()))
                .map(|key| {
                    let finding = Finding::fail(
                        "unread_file_key",
                        key.clone(),
                        format!(
                            "set in {}, but no field of the Config type reads it",
                            layer.path
                        ),
                    );
                    match nearest(key, &known) {
                        Some(near) => finding.with_fix(format!("did you mean {near}?")),
                        None => finding.with_fix(format!(
                            "remove it from {}, or declare the field",
                            layer.path
                        )),
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Secrets in files
// ---------------------------------------------------------------------------

/// Whether a file is in the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracked {
    /// Git tracks it: whatever is in it is in every clone.
    Yes,
    /// Git knows the repository and does not track this file.
    No,
    /// There is no repository here, or no git to ask.
    Unknown,
}

/// Every secret whose value came out of a file.
fn exposed_secrets(project: &Project, entries: &[Entry]) -> Vec<Finding> {
    entries
        .iter()
        .filter(|entry| entry.secret)
        .filter_map(|entry| {
            let Source::File(path) = source_of(entry.origin.as_deref()) else {
                return None;
            };
            Some(secret_finding(
                &entry.key,
                &entry.env,
                &path,
                tracked_by_git(&project.root, &path),
            ))
        })
        .collect()
}

/// The finding for one secret read out of `path`.
fn secret_finding(key: &str, env: &str, path: &str, tracked: Tracked) -> Finding {
    let fix = format!("unset it in {path} and export {env}, or {env}_FILE=/run/secrets/…");
    match tracked {
        Tracked::Yes => Finding::fail(
            "secret_in_tracked_file",
            key,
            format!("the value comes from `{path}`, which this repository tracks"),
        )
        .with_fix(fix),
        Tracked::No => Finding::warn(
            "secret_in_file",
            key,
            format!("the value comes from `{path}`, which git does not track — keep it that way"),
        )
        .with_fix(fix),
        Tracked::Unknown => Finding::warn(
            "secret_in_file",
            key,
            format!(
                "the value comes from `{path}`, and there is no repository here to prove it \
                 is not committed"
            ),
        )
        .with_fix(fix),
    }
}

/// Ask git whether it tracks `relative`.
///
/// Two questions rather than one: `ls-files --error-unmatch` exits non-zero
/// both for a file git does not track and for a directory that is not a
/// repository, and those are a failure and a shrug respectively.
fn tracked_by_git(root: &Path, relative: &str) -> Tracked {
    if !git(root, &["rev-parse", "--is-inside-work-tree"]) {
        return Tracked::Unknown;
    }
    if git(root, &["ls-files", "--error-unmatch", "--", relative]) {
        Tracked::Yes
    } else {
        Tracked::No
    }
}

/// Run one git subcommand silently, reporting whether it succeeded.
fn git(directory: &Path, arguments: &[&str]) -> bool {
    Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, env: &str, secret: bool, origin: Option<&str>) -> Entry {
        Entry {
            key: key.to_owned(),
            env: env.to_owned(),
            secret,
            origin: origin.map(str::to_owned),
        }
    }

    fn shop() -> Vec<Entry> {
        vec![
            entry("greeting", "SHOP__GREETING", false, Some("default")),
            entry("bind", "SHOP__BIND", false, Some("env SHOP__BIND")),
            entry(
                "database.url",
                "SHOP__DATABASE__URL",
                true,
                Some("config/dev.toml:4"),
            ),
        ]
    }

    // ── origins ───────────────────────────────────────────────────────────

    #[test]
    fn every_origin_rendering_is_classified() {
        assert_eq!(source_of(None), Source::Unset);
        assert_eq!(source_of(Some("code")), Source::Elsewhere);
        assert_eq!(source_of(Some("cli --bind")), Source::Elsewhere);
        assert_eq!(source_of(Some("env SHOP__BIND")), Source::Elsewhere);
        assert_eq!(source_of(Some("default")), Source::Elsewhere);
        assert_eq!(source_of(Some("profile default")), Source::Elsewhere);
        assert_eq!(
            source_of(Some(".env DATABASE_URL")),
            Source::File(".env".to_owned())
        );
        assert_eq!(
            source_of(Some("config/production.toml:8")),
            Source::File("config/production.toml".to_owned())
        );
        assert_eq!(
            source_of(Some("config/production.toml")),
            Source::File("config/production.toml".to_owned())
        );
    }

    /// A Windows path carries a colon that is not a line number, and truncating
    /// at it would report a secret as living in `C`.
    #[test]
    fn a_colon_that_is_not_a_line_number_stays_in_the_path() {
        assert_eq!(
            source_of(Some(r"C:\app\config\production.toml")),
            Source::File(r"C:\app\config\production.toml".to_owned())
        );
    }

    // ── the prefix ────────────────────────────────────────────────────────

    #[test]
    fn the_prefix_is_derived_from_the_two_spellings_of_every_key() {
        assert_eq!(prefix_of(&shop()), Some("SHOP__".to_owned()));
    }

    #[test]
    fn an_application_with_no_prefix_is_not_checked() {
        let entries = vec![entry("greeting", "GREETING", false, None)];
        assert_eq!(prefix_of(&entries), None);
    }

    // ── environment keys ──────────────────────────────────────────────────

    fn variable(name: &str) -> Variable {
        Variable {
            name: name.to_owned(),
            whence: ".env".to_owned(),
        }
    }

    #[test]
    fn a_mistyped_key_is_reported_with_the_key_it_was_probably_meant_to_be() {
        let findings = unread_environment(&shop(), &[], &[variable("SHOP__GRETING")]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, "unread_environment_key");
        assert_eq!(findings[0].level, Level::Fail);
        assert_eq!(findings[0].subject, "SHOP__GRETING");
        assert_eq!(
            findings[0].fix.as_deref(),
            Some("did you mean SHOP__GREETING?")
        );
    }

    #[test]
    fn a_key_that_is_read_and_a_variable_that_is_not_ours_are_both_left_alone() {
        let findings = unread_environment(
            &shop(),
            &[],
            &[
                variable("SHOP__GREETING"),
                variable("PATH"),
                variable("DATABASE_URL"),
            ],
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// `#[config(env = "…")]` replaces the prefixed spelling in the rendered
    /// example, and that alias is a name the loader really does read.
    #[test]
    fn an_explicit_alias_is_a_known_spelling() {
        let example = vec![ExampleKey {
            name: "SHOP__LOG_LEVEL".to_owned(),
            value: String::new(),
        }];
        let findings = unread_environment(&shop(), &example, &[variable("SHOP__LOG_LEVEL")]);
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// The mounted-secret convention: `${KEY}_FILE` is read for every secret
    /// field, and reporting it as a typo would be telling the user to delete
    /// the thing Kubernetes set.
    #[test]
    fn the_key_file_spelling_of_a_secret_is_not_a_typo() {
        let findings = unread_environment(&shop(), &[], &[variable("SHOP__DATABASE__URL_FILE")]);
        assert!(findings.is_empty(), "{findings:?}");
        let findings = unread_environment(&shop(), &[], &[variable("SHOP__NOTHING_FILE")]);
        assert_eq!(findings.len(), 1, "an unknown stem is still a typo");
    }

    #[test]
    fn a_wildly_different_name_gets_advice_rather_than_a_wrong_suggestion() {
        let findings = unread_environment(&shop(), &[], &[variable("SHOP__ZZZZZZZZZZZZ")]);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .fix
                .as_deref()
                .is_some_and(|fix| !fix.starts_with("did you mean")),
            "{:?}",
            findings[0].fix
        );
    }

    // ── .env parsing ──────────────────────────────────────────────────────

    #[test]
    fn a_dotenv_yields_only_the_names_it_assigns() {
        let names = dotenv_names(
            "# a comment\n\nSHOP__GREETING=hei\nexport SHOP__BIND=0.0.0.0:80\n#SHOP__X=1\nbare\n",
        );
        assert_eq!(names, vec!["SHOP__GREETING", "SHOP__BIND"]);
    }

    // ── .env.example ──────────────────────────────────────────────────────

    #[test]
    fn the_example_parser_reads_names_and_defaults_and_ignores_comments() {
        let keys = parse_env_example(
            "# The greeting.\nSHOP__GREETING=hello\n\n# Required.  [required]\nSHOP__URL=\n",
        );
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].name, "SHOP__GREETING");
        assert_eq!(keys[0].value, "hello");
        assert_eq!(keys[1].name, "SHOP__URL");
        assert!(keys[1].value.is_empty());
    }

    #[test]
    fn trailing_whitespace_is_not_drift() {
        assert_eq!(normalise("A=1\nB=2\n\n"), normalise("A=1   \nB=2"));
    }

    #[test]
    fn one_key_is_not_one_keys() {
        assert_eq!(counted(1), "1 key");
        assert_eq!(counted(3), "3 keys");
    }

    #[test]
    fn a_long_list_of_drifted_keys_is_summarised_rather_than_dumped() {
        let names: Vec<String> = (0..6).map(|index| format!("SHOP__K{index}")).collect();
        let borrowed: Vec<&String> = names.iter().collect();
        assert_eq!(
            listed(&borrowed),
            "SHOP__K0, SHOP__K1, SHOP__K2, SHOP__K3 and 2 more"
        );
    }

    // ── TOML layers ───────────────────────────────────────────────────────

    #[test]
    fn a_toml_document_flattens_to_the_dotted_keys_a_field_would_name() {
        let value: toml::Value =
            toml::from_str("name = \"shop\"\n[database]\nurl = \"x\"\nmax = 10\n")
                .expect("valid TOML");
        let mut keys = Vec::new();
        flatten(&value, String::new(), &mut keys);
        keys.sort();
        assert_eq!(keys, vec!["database.max", "database.url", "name"]);
    }

    #[test]
    fn a_committed_toml_key_nothing_reads_is_a_failure() {
        let layers = vec![Layer {
            path: "config/dev.toml".to_owned(),
            keys: vec!["greeting".to_owned(), "database.urls".to_owned()],
        }];
        let findings = unread_file_keys(&shop(), &layers);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].subject, "database.urls");
        assert_eq!(
            findings[0].fix.as_deref(),
            Some("did you mean database.url?")
        );
    }

    // ── secrets ───────────────────────────────────────────────────────────

    #[test]
    fn a_secret_in_a_tracked_file_fails_and_names_the_environment_instead() {
        let finding = secret_finding(
            "database.url",
            "SHOP__DATABASE__URL",
            "config/dev.toml",
            Tracked::Yes,
        );
        assert_eq!(finding.level, Level::Fail);
        assert_eq!(finding.check, "secret_in_tracked_file");
        assert!(
            finding
                .fix
                .as_deref()
                .is_some_and(|fix| fix.contains("SHOP__DATABASE__URL_FILE")),
            "{:?}",
            finding.fix
        );
    }

    #[test]
    fn a_secret_in_an_untracked_file_only_warns() {
        for tracked in [Tracked::No, Tracked::Unknown] {
            let finding = secret_finding("database.url", "SHOP__DATABASE__URL", ".env", tracked);
            assert_eq!(finding.level, Level::Warn, "{tracked:?}");
        }
    }

    #[test]
    fn only_secret_fields_are_examined() {
        let entries = vec![entry(
            "greeting",
            "SHOP__GREETING",
            false,
            Some("config/dev.toml:2"),
        )];
        let project = Project {
            manifest_path: PathBuf::from("/nowhere/Cargo.toml"),
            root: PathBuf::from("/nowhere"),
            name: "shop".to_owned(),
            rust_version: None,
            uses_moso: true,
        };
        assert!(exposed_secrets(&project, &entries).is_empty());
    }

    // ── the exit code ─────────────────────────────────────────────────────

    #[test]
    fn warnings_alone_do_not_fail_the_command() {
        let warning = Finding::warn("secret_in_file", "database.url", "in .env");
        assert!(report(&Ui::silent(), "dev", std::slice::from_ref(&warning)).is_ok());

        let failure = Finding::fail("env_example_drift", ".env.example", "drifted");
        let error = report(&Ui::silent(), "dev", &[warning, failure]).expect_err("fails");
        assert_eq!(error.fault, Fault::User);
        assert!(error.message.starts_with("1 configuration problem"));
    }

    #[test]
    fn a_clean_configuration_exits_zero() {
        assert!(report(&Ui::silent(), "dev", &[]).is_ok());
    }
}
