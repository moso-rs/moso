//! `moso check` — the mistakes rustc cannot see.
//!
//! ```text
//! warning[stale_layer]: `.layer()` is the last call in `router`
//!   --> src/routes.rs:41:9
//!    = note: a layer applies to the routes registered before it
//!    = help: move the `.layer(..)` call above the routes it should cover
//! ```
//!
//! # Why this command exists before it is finished
//!
//! Several shipped diagnostics end by telling the reader to run `moso check`,
//! and until now that advice went nowhere — which is worse than no advice,
//! because the reader spends their next minute finding out the command is not
//! real. Every lint below is one a design document promised. The ones that are
//! *not* below are listed in `40-cli.md` with what they need, and this command
//! says nothing about them rather than passing them silently.
//!
//! # Two sources, and the difference matters
//!
//! | Source | Lints | Confidence |
//! | --- | --- | --- |
//! | The application's own answer (`--dump-*`) | `undocumented_endpoint`, `route_not_in_document`, `env_example_drift`, `missing_authz`, `unknown_permission` | exact — it is the assembled router and document |
//! | A lexical scan of `src/**.rs` | `stale_layer`, `n_plus_one`, `blocking_in_async`, `layering`, `unhandled_error_variant` | textual — it reads tokens, not types |
//!
//! The scan is deliberately not a parse. `40-cli.md` describes a `syn`-based
//! pass; the CLI depends on no Moso crate and on four third-party crates, and
//! adding a parser to it is a decision with an ADR attached rather than a detail
//! of this command. What is here strips comments and string literals first —
//! so a lint cannot fire on a doc comment that mentions the thing it hunts —
//! tracks braces to know which function and which loop a line is inside, and
//! then matches tokens. It finds the shapes it claims to find and it will miss
//! a mistake spelled unusually. Every lexical lint is therefore reported with
//! its exact line, so a false positive costs a glance rather than an
//! investigation, and every one of them can be turned off by name.
//!
//! # Levels, `moso.toml`, and the exit code
//!
//! Each lint has a default level from `40-cli.md`'s table. `[lints]` in
//! `moso.toml` overrides any of them by name with `allow`, `warn` or `deny` —
//! the key `31-authorization.md` documents and that, until now, nothing read.
//! The process exits 1 when a finding is at `deny`, so a CI job gates on the
//! lints a team has decided to enforce rather than on every opinion this command
//! holds. `--strict` promotes every warning for a job that wants all of them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cli::CheckArgs;
use crate::exit::{CliError, Outcome};
use crate::project::{Battery, Dump, Project};
use crate::ui::{Level as Glyph, Ui};

// ---------------------------------------------------------------------------
// The catalogue
// ---------------------------------------------------------------------------

/// How loudly a lint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Not reported at all.
    Allow,
    /// Reported; the command still exits 0.
    Warn,
    /// Reported; the command exits 1.
    Deny,
}

impl Level {
    /// The spelling used in `moso.toml` and in `--json`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Allow => "allow",
            Level::Warn => "warn",
            Level::Deny => "deny",
        }
    }

    /// Read one `moso.toml` value.
    fn parse(text: &str) -> Option<Self> {
        match text {
            "allow" => Some(Level::Allow),
            "warn" => Some(Level::Warn),
            "deny" => Some(Level::Deny),
            _ => None,
        }
    }

    /// The word rustc would print in front of the message.
    const fn heading(self) -> &'static str {
        match self {
            Level::Deny => "error",
            _ => "warning",
        }
    }
}

/// What one lint needs before it can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Needs {
    /// A lexical scan of `src/**.rs`.
    Source,
    /// `--dump-routes`.
    Routes,
    /// `--dump-routes` and `--dump-openapi`.
    Document,
    /// `--dump-env-example`.
    EnvExample,
    /// `--dump-authz`, and therefore `--authz` on the command line.
    Authz,
}

/// One lint: its name, its default level, and what it catches.
#[derive(Debug, Clone, Copy)]
struct Lint {
    /// The name used by `--lint`, by `[lints]` in `moso.toml` and in `--json`.
    name: &'static str,
    /// The level it reports at unless `moso.toml` says otherwise.
    default: Level,
    /// What it needs to run.
    needs: Needs,
    /// One line for `--list`.
    catches: &'static str,
}

/// Every lint this build implements.
///
/// The names are `40-cli.md`'s. A lint that document lists and this array does
/// not is one that needs a crate or a snapshot this build has no access to;
/// leaving it out is the honest form, because a lint that silently passes is
/// indistinguishable from a codebase that is clean.
const LINTS: &[Lint] = &[
    Lint {
        name: "layering",
        default: Level::Deny,
        needs: Needs::Source,
        catches: "routes/ importing SQL, services/ importing http, models/ importing either",
    },
    Lint {
        name: "blocking_in_async",
        default: Level::Deny,
        needs: Needs::Source,
        catches: "std::fs, std::thread::sleep or reqwest::blocking inside an async fn",
    },
    Lint {
        name: "n_plus_one",
        default: Level::Warn,
        needs: Needs::Source,
        catches: ".load( or .fetch_ inside a loop",
    },
    Lint {
        name: "stale_layer",
        default: Level::Warn,
        needs: Needs::Source,
        catches: ".layer() or .guard() as the last call in a router fn",
    },
    Lint {
        name: "unhandled_error_variant",
        default: Level::Warn,
        needs: Needs::Document,
        catches: "a handler constructing a 4xx it does not declare",
    },
    Lint {
        name: "undocumented_endpoint",
        default: Level::Warn,
        needs: Needs::Routes,
        catches: "a route registered without #[endpoint]",
    },
    Lint {
        name: "route_not_in_document",
        default: Level::Warn,
        needs: Needs::Document,
        catches: "a visible route with no operation in the OpenAPI document",
    },
    Lint {
        name: "env_example_drift",
        default: Level::Warn,
        needs: Needs::EnvExample,
        catches: "a committed .env.example the Config type no longer generates",
    },
    Lint {
        name: "missing_authz",
        default: Level::Warn,
        needs: Needs::Authz,
        catches: "an operation with no #[requires], Authorized<..> or #[public]",
    },
    Lint {
        name: "unknown_permission",
        default: Level::Deny,
        needs: Needs::Authz,
        catches: "a permission named by a string the registry does not declare",
    },
];

/// Look one up by name.
fn lint(name: &str) -> Option<&'static Lint> {
    LINTS.iter().find(|lint| lint.name == name)
}

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

/// One thing worth telling the reader about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Which lint produced it.
    lint: &'static str,
    /// The level it is being reported at, after `moso.toml` and `--strict`.
    level: Level,
    /// The one-line statement of what is wrong.
    message: String,
    /// Where, as `file:line:col` or as a route, whichever the source knows.
    location: String,
    /// The file, when there is one, so `--json` can carry it separately.
    file: Option<String>,
    /// The 1-based line, when there is one.
    line: Option<usize>,
    /// Why the rule exists.
    note: String,
    /// What to do, as something pasteable.
    help: String,
}

impl Finding {
    /// The `--json` rendering.
    fn to_json(&self) -> Value {
        serde_json::json!({
            "lint": self.lint,
            "level": self.level.as_str(),
            "message": self.message,
            "location": self.location,
            "file": self.file,
            "line": self.line,
            "note": self.note,
            "help": self.help,
        })
    }
}

/// Collects findings and knows which lints are switched on.
struct Report {
    /// The effective level of every lint, after `moso.toml` and `--strict`.
    levels: BTreeMap<&'static str, Level>,
    /// What was found, in discovery order.
    findings: Vec<Finding>,
}

impl Report {
    /// Whether a lint will be reported at all.
    fn enabled(&self, name: &str) -> bool {
        self.levels.get(name).copied().unwrap_or(Level::Allow) != Level::Allow
    }

    /// Record one finding, unless its lint is switched off.
    fn push(
        &mut self,
        name: &'static str,
        location: (Option<&str>, Option<usize>, String),
        message: String,
        note: &str,
        help: String,
    ) {
        let Some(&level) = self.levels.get(name) else {
            return;
        };
        if level == Level::Allow {
            return;
        }
        let (file, line, rendered) = location;
        self.findings.push(Finding {
            lint: name,
            level,
            message,
            location: rendered,
            file: file.map(str::to_owned),
            line,
            note: note.to_owned(),
            help,
        });
    }
}

/// A `file:line:col` location, in the three forms a finding carries it.
fn at(file: &str, line: usize, column: usize) -> (Option<&str>, Option<usize>, String) {
    (Some(file), Some(line), format!("{file}:{line}:{column}"))
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

/// Run `moso check`.
///
/// # Errors
/// [`Fault::User`](crate::exit::Fault::User) when a lint fires at `deny`, so a
/// CI job can gate on it; a `--lint` naming something that does not exist, which
/// is a usage error listing what does; and anything the dump protocol can fail
/// with.
pub fn run(ui: &Ui, args: &CheckArgs) -> Outcome<()> {
    if args.list {
        return list(ui);
    }

    let project = Project::discover(args.app.manifest_path.as_deref())?;
    project.require_moso()?;

    let (configured, unknown) = configured_levels(&project.root)?;
    for key in &unknown {
        ui.warn(&format!(
            "moso.toml: `lints.{key}` is not a lint this build knows"
        ));
    }
    let levels = resolve(&configured, &args.lint, args.strict, args.authz)?;
    let mut report = Report {
        levels,
        findings: Vec::new(),
    };

    if report.levels.values().all(|level| *level == Level::Allow) {
        return Err(CliError::user("every lint is switched off").with_help(
            "run `moso check --list` to see the lints, or relax `[lints]` in moso.toml",
        ));
    }

    scan_sources(&project.root, &mut report)?;

    let routes = if needed(&report, &[Needs::Routes, Needs::Document]) {
        parse_routes(&project.dump(&args.app, Dump::Routes)?)?
    } else {
        Vec::new()
    };
    let document = if needed(&report, &[Needs::Document]) {
        Some(parse_json(
            &project.dump(&args.app, Dump::OpenApi)?,
            "--dump-openapi",
        )?)
    } else {
        None
    };

    check_routes(&routes, document.as_ref(), &mut report);

    if needed(&report, &[Needs::EnvExample]) {
        check_env_example(
            &project.root,
            &project.dump(&args.app, Dump::EnvExample)?,
            &mut report,
        );
    }

    if args.authz && needed(&report, &[Needs::Authz]) {
        let answer = project.battery(&args.app, &Battery::Authz(authz_request()))?;
        check_authz(&parse_json(&answer, "--dump-authz")?, &mut report)?;
    }

    emit(ui, &report, args.authz)
}

/// `moso check --list`.
fn list(ui: &Ui) -> Outcome<()> {
    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": true,
            "lints": LINTS.iter().map(|lint| serde_json::json!({
                "name": lint.name,
                "default": lint.default.as_str(),
                "needs_authz": lint.needs == Needs::Authz,
                "catches": lint.catches,
            })).collect::<Vec<_>>(),
        }));
        return Ok(());
    }

    let rows: Vec<Vec<String>> = LINTS
        .iter()
        .map(|lint| {
            vec![
                lint.name.to_owned(),
                lint.default.as_str().to_owned(),
                lint.catches.to_owned(),
            ]
        })
        .collect();
    ui.blank();
    ui.table(&["LINT", "DEFAULT", "CATCHES"], &rows);
    ui.blank();
    ui.line(&ui.dim("  set any of them in moso.toml:  [lints]  n_plus_one = \"deny\""));
    ui.blank();
    Ok(())
}

/// Whether any enabled lint needs one of these sources.
fn needed(report: &Report, needs: &[Needs]) -> bool {
    LINTS
        .iter()
        .filter(|lint| needs.contains(&lint.needs))
        .any(|lint| report.enabled(lint.name))
}

/// The request `moso check --authz` sends.
fn authz_request() -> String {
    serde_json::json!({ "view": "check" }).to_string()
}

/// Work out the effective level of every lint.
///
/// Precedence, weakest first: the default from the table, `[lints]` in
/// `moso.toml`, `--strict`, then `--lint` which switches everything else off.
fn resolve(
    configured: &BTreeMap<String, Level>,
    only: &[String],
    strict: bool,
    authz: bool,
) -> Outcome<BTreeMap<&'static str, Level>> {
    for name in only {
        if lint(name).is_none() {
            let known: Vec<&str> = LINTS.iter().map(|lint| lint.name).collect();
            return Err(CliError::usage(format!("`{name}` is not a lint"))
                .with_help(format!("the lints are: {}", known.join(", "))));
        }
    }

    let mut levels = BTreeMap::new();
    for lint in LINTS {
        let mut level = configured.get(lint.name).copied().unwrap_or(lint.default);
        if strict && level == Level::Warn {
            level = Level::Deny;
        }
        if !only.is_empty() && !only.iter().any(|name| name == lint.name) {
            level = Level::Allow;
        }
        // The authorization lints ask the application a question only a project
        // using `moso-authz` can answer. Running them unasked would turn "you
        // do not use this battery" into a failed check.
        if lint.needs == Needs::Authz && !authz {
            level = Level::Allow;
        }
        levels.insert(lint.name, level);
    }
    Ok(levels)
}

/// Read `[lints]` out of `moso.toml`, if there is one.
///
/// Returns the levels it understood and the keys it did not. The second half
/// matters: a `[lints]` key that nothing reads is exactly the defect this
/// command exists to remove, and a typo would silently reproduce it.
///
/// # Errors
/// [`Fault::User`](crate::exit::Fault::User) when the file exists and does not
/// parse, or when a level is not one of the three words.
fn configured_levels(root: &Path) -> Outcome<(BTreeMap<String, Level>, Vec<String>)> {
    let path = root.join("moso.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok((BTreeMap::new(), Vec::new()));
    };
    let parsed: toml::Value = toml::from_str(&text).map_err(|error| {
        CliError::user(format!("`{}` is not valid TOML: {error}", path.display()))
            .with_help("fix the file, or delete it — every lint has a default without it")
    })?;

    let mut levels = BTreeMap::new();
    let mut unknown = Vec::new();
    let Some(table) = parsed.get("lints").and_then(toml::Value::as_table) else {
        return Ok((levels, unknown));
    };

    for (key, value) in table {
        let Some(text) = value.as_str() else {
            return Err(
                CliError::user(format!("moso.toml: `lints.{key}` must be a string"))
                    .with_help("one of \"allow\", \"warn\" or \"deny\""),
            );
        };
        let Some(level) = Level::parse(text) else {
            return Err(CliError::user(format!(
                "moso.toml: `lints.{key} = {text:?}` is not a level"
            ))
            .with_help("one of \"allow\", \"warn\" or \"deny\""));
        };
        if lint(key).is_none() {
            unknown.push(key.clone());
            continue;
        }
        levels.insert(key.clone(), level);
    }
    Ok((levels, unknown))
}

// ---------------------------------------------------------------------------
// The lexical scan
// ---------------------------------------------------------------------------

/// One function, as the scan understands it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Function {
    /// Its name.
    name: String,
    /// The 0-based index of the line the signature starts on.
    start: usize,
    /// The 0-based index of the line its body closes on.
    end: usize,
    /// Whether it is `async`.
    is_async: bool,
    /// Whether its return type mentions `Router`.
    returns_router: bool,
    /// Whether `#[endpoint]` sits above it.
    is_endpoint: bool,
}

/// Walk `src/` and run every lexical lint over each file.
fn scan_sources(root: &Path, report: &mut Report) -> Outcome<()> {
    if !needed(report, &[Needs::Source, Needs::Document]) {
        return Ok(());
    }
    let mut files = Vec::new();
    collect(&root.join("src"), &mut files);
    files.sort();

    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string()
            .replace('\\', "/");
        let raw: Vec<&str> = text.lines().collect();
        let clean = strip(&text);
        let functions = functions(&raw, &clean);

        check_layering(&relative, &clean, report);
        check_blocking(&relative, &clean, &functions, report);
        check_loops(&relative, &clean, report);
        check_stale_layer(&relative, &clean, &functions, report);
        record_handlers(&relative, &clean, &functions, report);
    }
    Ok(())
}

/// Every `.rs` file under `directory`, recursively.
fn collect(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

/// Blank out comments, string literals and char literals, keeping every line
/// and every column exactly where it was.
///
/// This is what stops `n_plus_one` firing on the paragraph in `relations.md`
/// that explains it, and `blocking_in_async` firing on a doc comment that says
/// "never call `std::fs` here". A lint that cannot be written about is a lint
/// nobody documents.
fn strip(source: &str) -> Vec<String> {
    /// Where the scanner is between lines.
    enum Mode {
        /// Ordinary code.
        Code,
        /// Inside `/* */`, which nests, carrying the depth.
        Block(usize),
        /// Inside a string: `Some(n)` for a raw string with `n` hashes.
        Text(Option<usize>),
    }

    let mut mode = Mode::Code;
    let mut out = Vec::new();

    for line in source.lines() {
        let characters: Vec<char> = line.chars().collect();
        let mut cleaned: Vec<char> = vec![' '; characters.len()];
        let mut index = 0;

        while index < characters.len() {
            match mode {
                Mode::Block(depth) => {
                    if characters[index] == '/' && characters.get(index + 1) == Some(&'*') {
                        mode = Mode::Block(depth + 1);
                        index += 2;
                    } else if characters[index] == '*' && characters.get(index + 1) == Some(&'/') {
                        mode = if depth <= 1 {
                            Mode::Code
                        } else {
                            Mode::Block(depth - 1)
                        };
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
                Mode::Text(hashes) => {
                    if let Some(hashes) = hashes {
                        if characters[index] == '"' && closes_raw(&characters, index + 1, hashes) {
                            mode = Mode::Code;
                            index += 1 + hashes;
                        } else {
                            index += 1;
                        }
                    } else if characters[index] == '\\' {
                        index += 2;
                    } else if characters[index] == '"' {
                        mode = Mode::Code;
                        index += 1;
                    } else {
                        index += 1;
                    }
                }
                Mode::Code => {
                    let character = characters[index];
                    if character == '/' && characters.get(index + 1) == Some(&'/') {
                        break;
                    }
                    if character == '/' && characters.get(index + 1) == Some(&'*') {
                        mode = Mode::Block(1);
                        index += 2;
                        continue;
                    }
                    if character == '"' {
                        mode = Mode::Text(None);
                        index += 1;
                        continue;
                    }
                    if let Some(hashes) = opens_raw(&characters, index) {
                        mode = Mode::Text(Some(hashes));
                        index += hashes + 2;
                        continue;
                    }
                    if character == '\'' && is_char_literal(&characters, index) {
                        index += 1;
                        while index < characters.len() {
                            if characters[index] == '\\' {
                                index += 2;
                                continue;
                            }
                            if characters[index] == '\'' {
                                index += 1;
                                break;
                            }
                            index += 1;
                        }
                        continue;
                    }
                    cleaned[index] = character;
                    index += 1;
                }
            }
        }
        out.push(cleaned.into_iter().collect());
    }
    out
}

/// Whether a raw-string opener starts here, and how many hashes it has.
fn opens_raw(characters: &[char], index: usize) -> Option<usize> {
    let mut cursor = index;
    if characters.get(cursor) == Some(&'b') {
        cursor += 1;
    }
    if characters.get(cursor) != Some(&'r') {
        return None;
    }
    cursor += 1;
    let start = cursor;
    while characters.get(cursor) == Some(&'#') {
        cursor += 1;
    }
    if characters.get(cursor) == Some(&'"') {
        Some(cursor - start)
    } else {
        None
    }
}

/// Whether `hashes` hashes follow, which is what closes a raw string.
fn closes_raw(characters: &[char], from: usize, hashes: usize) -> bool {
    (0..hashes).all(|offset| characters.get(from + offset) == Some(&'#'))
}

/// Whether a `'` opens a char literal rather than a lifetime.
///
/// `'a'` and `'\n'` are literals; `'static` is a lifetime. The test is the third
/// character: a literal closes there, a lifetime carries on.
fn is_char_literal(characters: &[char], index: usize) -> bool {
    match characters.get(index + 1) {
        Some(next) if next.is_alphanumeric() || *next == '_' => {
            characters.get(index + 2) == Some(&'\'')
        }
        Some(_) => true,
        None => false,
    }
}

/// Find every function, its body, and the three things the lints ask about it.
fn functions(raw: &[&str], clean: &[String]) -> Vec<Function> {
    let mut found = Vec::new();
    for (index, line) in clean.iter().enumerate() {
        let Some(column) = keyword(line, "fn") else {
            continue;
        };
        let before = &line[..column];
        if !is_definition(before) {
            continue;
        }
        let Some(name) = identifier_after(line, column + 2) else {
            continue;
        };

        // The signature runs from here to the `{` that opens the body, which
        // rustfmt may have put several lines down for a long parameter list.
        let mut signature = String::new();
        let mut opening = None;
        for (offset, text) in clean.iter().enumerate().skip(index).take(40) {
            match text.find('{') {
                Some(at) => {
                    signature.push_str(&text[..at]);
                    opening = Some(offset);
                    break;
                }
                None => {
                    signature.push_str(text);
                    signature.push(' ');
                }
            }
        }
        let Some(opening) = opening else { continue };

        found.push(Function {
            name,
            start: index,
            end: body_end(clean, opening),
            is_async: keyword(before, "async").is_some(),
            returns_router: signature.contains("->") && signature.contains("Router"),
            is_endpoint: has_endpoint_attribute(raw, index),
        });
    }
    found
}

/// The line the body opened on `opening` closes on.
fn body_end(clean: &[String], opening: usize) -> usize {
    let mut depth = 0_i32;
    for (index, line) in clean.iter().enumerate().skip(opening) {
        for character in line.chars() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return index;
                    }
                }
                _ => {}
            }
        }
    }
    clean.len().saturating_sub(1)
}

/// Whether what precedes `fn` on its line is only the words a definition may
/// carry — so `Fn(u8) -> u8` and `fn` inside an expression are both skipped.
fn is_definition(before: &str) -> bool {
    before.split_whitespace().all(|word| {
        matches!(
            word,
            "pub"
                | "pub(crate)"
                | "pub(super)"
                | "pub(self)"
                | "async"
                | "const"
                | "unsafe"
                | "extern"
                | "default"
                | "\"C\""
        )
    })
}

/// The identifier starting at `from`, skipping the space after the keyword.
fn identifier_after(line: &str, from: usize) -> Option<String> {
    let rest = line.get(from..)?.trim_start();
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

/// The column of `word` as a standalone token, if it is on this line.
fn keyword(line: &str, word: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(offset) = line[from..].find(word) {
        let at = from + offset;
        let before_ok = at == 0
            || !line[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let after_ok = line[at + word.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        if before_ok && after_ok {
            return Some(at);
        }
        from = at + word.len();
    }
    None
}

/// Whether `#[endpoint]` sits in the attribute block above line `index`.
///
/// Walked over the *raw* lines: the cleaned ones have had doc comments blanked
/// out, and a doc comment between the attribute and the `fn` is the normal
/// shape, not a reason to stop looking.
fn has_endpoint_attribute(raw: &[&str], index: usize) -> bool {
    for line in raw[..index].iter().rev() {
        let trimmed = line.trim();
        if trimmed.starts_with("#[endpoint") {
            return true;
        }
        if trimmed.is_empty()
            || trimmed.starts_with("#[")
            || trimmed.starts_with("///")
            || trimmed.starts_with("//")
            || trimmed.starts_with(')')
            || trimmed.starts_with(']')
        {
            continue;
        }
        return false;
    }
    false
}

/// The lines of one function's body, each with its 0-based index.
///
/// Clamped to the file: a body whose closing brace is missing — a file that does
/// not compile, which is a state a lint has to survive — would otherwise index
/// past the end.
fn body_of<'a>(
    clean: &'a [String],
    function: &Function,
) -> impl Iterator<Item = (usize, &'a String)> {
    let end = function.end.min(clean.len().saturating_sub(1));
    clean.iter().enumerate().take(end + 1).skip(function.start)
}

// ---------------------------------------------------------------------------
// The lexical lints
// ---------------------------------------------------------------------------

/// The import each layer may not have, and why.
const LAYERS: &[(&str, &[&str], &str)] = &[
    (
        "routes",
        &["moso::sql", "moso_sql", "sqlx::", "use sqlx"],
        "a handler that builds SQL cannot be reused by a job or a command, and the query \
         stops being reviewable in one place",
    ),
    (
        "services",
        &["moso::extract", "use axum", "use http::", "moso::http"],
        "a service that knows about HTTP cannot be called from a job, a test or a CLI task",
    ),
    (
        "models",
        &[
            "crate::services",
            "crate::routes",
            "super::services",
            "super::routes",
        ],
        "a model that reaches upward makes the dependency graph a cycle and the entity \
         impossible to test on its own",
    ),
    (
        "jobs",
        &["crate::routes", "super::routes", "moso::extract"],
        "a job runs without a request, so anything it borrows from the routing layer is \
         unreachable at the moment it needs it",
    ),
];

/// `layering`.
fn check_layering(file: &str, clean: &[String], report: &mut Report) {
    if !report.enabled("layering") {
        return;
    }
    let Some((layer, forbidden, why)) = LAYERS
        .iter()
        .find(|(layer, _, _)| in_layer(file, layer))
        .copied()
    else {
        return;
    };

    for (index, line) in clean.iter().enumerate() {
        if !line.trim_start().starts_with("use ") {
            continue;
        }
        for needle in forbidden {
            if let Some(column) = line.find(needle) {
                report.push(
                    "layering",
                    at(file, index + 1, column + 1),
                    format!("`{layer}/` imports `{}`", needle.trim_start_matches("use ")),
                    why,
                    format!(
                        "move the call into the layer below and give `{layer}/` a function to \
                         call, or set `layering = \"allow\"` in moso.toml"
                    ),
                );
                break;
            }
        }
    }
}

/// Whether a path is inside one of the layered directories.
///
/// `src/routes.rs` counts as `routes/`: a project small enough to keep the layer
/// in one file is still that layer, and it is the shape `moso new` writes.
fn in_layer(file: &str, layer: &str) -> bool {
    file.contains(&format!("/{layer}/")) || file.ends_with(&format!("/{layer}.rs"))
}

/// What blocks a runtime thread, and what to reach for instead.
const BLOCKING: &[(&str, &str)] = &[
    ("std::fs::", "moso::task::blocking(|| std::fs::..).await"),
    ("std::thread::sleep", "tokio::time::sleep(duration).await"),
    (
        "reqwest::blocking",
        "the async `reqwest::Client`, which is the default one",
    ),
    (
        "std::process::Command",
        "tokio::process::Command, which does not park the thread",
    ),
];

/// `blocking_in_async`.
fn check_blocking(file: &str, clean: &[String], functions: &[Function], report: &mut Report) {
    if !report.enabled("blocking_in_async") {
        return;
    }
    for function in functions.iter().filter(|function| function.is_async) {
        for (index, line) in body_of(clean, function) {
            for (needle, instead) in BLOCKING {
                if let Some(column) = line.find(needle) {
                    report.push(
                        "blocking_in_async",
                        at(file, index + 1, column + 1),
                        format!(
                            "`{needle}` blocks the runtime thread in `{}`",
                            function.name
                        ),
                        "an async task that blocks stops every other task sharing its worker, \
                         and a runtime with all workers parked serves nothing",
                        format!("use {instead}"),
                    );
                }
            }
        }
    }
}

/// What is an N+1 when it happens once per iteration.
const PER_ROW: &[&str] = &[".load(", ".fetch_one(", ".fetch_all(", ".fetch_optional("];

/// `n_plus_one`.
///
/// The loop is found by tracking braces: a `{` opened while a loop keyword was
/// the first token of its line pushes a loop frame, and everything nested inside
/// it — including a closure — is inside the loop.
fn check_loops(file: &str, clean: &[String], report: &mut Report) {
    if !report.enabled("n_plus_one") {
        return;
    }

    let mut stack: Vec<bool> = Vec::new();
    let mut pending = false;

    for (index, line) in clean.iter().enumerate() {
        let trimmed = line.trim_start();
        let statement = trimmed.strip_prefix('\'').map_or(trimmed, |rest| {
            rest.split_once(':')
                .map_or(rest, |(_, tail)| tail.trim_start())
        });
        if ["for ", "while ", "loop "]
            .iter()
            .any(|word| statement.starts_with(word))
            || statement == "loop"
        {
            pending = true;
        }

        // Walked in segments between braces rather than line by line, so that
        // `for row in rows { row.load(..); }` — the whole loop on one line — is
        // inside the frame its own `{` opened.
        let mut rest = line.as_str();
        let mut offset = 0;
        loop {
            let brace = rest.find(['{', '}']);
            let segment = brace.map_or(rest, |at| &rest[..at]);

            if stack.iter().any(|frame| *frame)
                && let Some((column, call)) = per_row(segment)
            {
                report.push(
                    "n_plus_one",
                    at(file, index + 1, offset + column + 1),
                    format!("`{call}` runs once per iteration"),
                    "one statement per row is the N+1: a hundred rows is a hundred round \
                     trips, and it looks fast on a development database with ten",
                    "collect the rows first and call `load_many(&mut rows, Rel, &db)` once, \
                     or `.with(Rel)` on the query that produced them"
                        .to_owned(),
                );
            }

            let Some(brace) = brace else { break };
            if rest.as_bytes()[brace] == b'{' {
                stack.push(pending);
                pending = false;
            } else {
                stack.pop();
            }
            offset += brace + 1;
            rest = &rest[brace + 1..];
        }
    }
}

/// The first per-row call in one segment, with its column and its name.
fn per_row(segment: &str) -> Option<(usize, &'static str)> {
    PER_ROW
        .iter()
        .filter_map(|needle| {
            segment
                .find(needle)
                .map(|column| (column, needle.trim_matches(|c| c == '.' || c == '(')))
        })
        .min_by_key(|(column, _)| *column)
}

/// What registers a route, for the purposes of "was anything registered after
/// this `.layer()`".
const REGISTRATIONS: &[&str] = &[
    ".get(",
    ".post(",
    ".put(",
    ".patch(",
    ".delete(",
    ".head(",
    ".options(",
    ".trace(",
    ".route(",
    ".nest(",
    ".merge(",
    ".mount(",
    ".static_files(",
    ".fallback(",
];

/// `stale_layer`.
fn check_stale_layer(file: &str, clean: &[String], functions: &[Function], report: &mut Report) {
    if !report.enabled("stale_layer") {
        return;
    }
    for function in functions.iter().filter(|function| function.returns_router) {
        let mut last_registration = None;
        let mut last_scoping: Option<(usize, usize, &str)> = None;

        for (index, line) in body_of(clean, function) {
            if REGISTRATIONS.iter().any(|call| line.contains(call)) {
                last_registration = Some(index);
            }
            for call in [".layer(", ".guard("] {
                if let Some(column) = line.find(call) {
                    last_scoping =
                        Some((index, column, call.trim_matches(|c| c == '.' || c == '(')));
                }
            }
        }

        let Some((index, column, call)) = last_scoping else {
            continue;
        };
        if last_registration.is_some_and(|registration| registration > index) {
            continue;
        }

        report.push(
            "stale_layer",
            at(file, index + 1, column + 1),
            format!("`.{call}()` is the last call in `{}`", function.name),
            "`.layer()` and `.guard()` apply to the routes registered before them, so nothing \
             chained onto this router afterwards — by a caller, or by the next person to add a \
             route — is covered",
            format!(
                "if `{}` is only ever mounted as it stands, this is fine and \
                 `stale_layer = \"allow\"` in moso.toml says so; otherwise move the `.{call}(..)` \
                 call to the position that scopes it",
                function.name
            ),
        );
    }
}

/// The 4xx each `Error` constructor produces.
///
/// 5xx is deliberately absent. `Error::internal` is reachable from any handler
/// that calls anything, so linting it would fire on nearly every function and
/// say nothing; `errors = ..` exists to declare the *contract*, and the contract
/// is the 4xx a client is expected to handle.
const CONSTRUCTED: &[(&str, u16)] = &[
    ("Error::bad_request(", 400),
    ("Error::unauthenticated(", 401),
    ("Error::forbidden(", 403),
    ("Error::not_found(", 404),
    ("Error::method_not_allowed(", 405),
    ("Error::conflict(", 409),
    ("Error::payload_too_large(", 413),
    ("Error::uri_too_long(", 414),
    ("Error::unsupported_media(", 415),
    ("Error::validation(", 422),
    ("Error::too_many(", 429),
];

/// One handler, and the statuses its body constructs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Constructs {
    /// The file it is written in.
    file: String,
    /// The handler's name, which is what a route row carries.
    name: String,
    /// The status, the line, and the constructor that produced it.
    statuses: Vec<(u16, usize, &'static str)>,
}

/// Every handler in this file that constructs a 4xx, remembered for the pass
/// that has the OpenAPI document in hand.
fn record_handlers(file: &str, clean: &[String], functions: &[Function], report: &mut Report) {
    if !report.enabled("unhandled_error_variant") {
        return;
    }
    for function in functions.iter().filter(|function| function.is_endpoint) {
        let mut statuses = Vec::new();
        for (index, line) in body_of(clean, function) {
            for (needle, status) in CONSTRUCTED {
                if line.contains(needle) {
                    statuses.push((*status, index + 1, *needle));
                }
            }
        }
        if !statuses.is_empty() {
            HANDLERS.with_borrow_mut(|handlers| {
                handlers.push(Constructs {
                    file: file.to_owned(),
                    name: function.name.clone(),
                    statuses,
                });
            });
        }
    }
}

thread_local! {
    /// The handlers the source scan found, waiting for the document.
    ///
    /// The scan runs before the application is built, because a build takes
    /// seconds and a scan takes milliseconds — so a project that fails a lexical
    /// lint hears about it immediately. This holds the half of
    /// `unhandled_error_variant` that cannot be decided until the document
    /// arrives. Thread-local rather than threaded through six signatures, and
    /// cleared by the pass that drains it.
    static HANDLERS: std::cell::RefCell<Vec<Constructs>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

// ---------------------------------------------------------------------------
// The lints that read the application's own answer
// ---------------------------------------------------------------------------

/// One route, read for what the lints ask about it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Route {
    /// The HTTP method, lower case, as the OpenAPI document keys it.
    method: String,
    /// The full path.
    path: String,
    /// The handler's name.
    handler: String,
    /// Whether it carries an `#[endpoint]` description.
    documented: bool,
    /// Whether it is deliberately absent from the document.
    hidden: bool,
    /// Where `#[endpoint]` was written, when the handler carries a location.
    source: Option<String>,
}

impl Route {
    /// How this route reads as a location.
    fn location(&self) -> (Option<&str>, Option<usize>, String) {
        match &self.source {
            Some(source) => (None, None, source.clone()),
            None => (
                None,
                None,
                format!("{} {}", self.method.to_uppercase(), self.path),
            ),
        }
    }
}

/// `undocumented_endpoint`, `route_not_in_document` and the second half of
/// `unhandled_error_variant`.
fn check_routes(routes: &[Route], document: Option<&Value>, report: &mut Report) {
    for route in routes.iter().filter(|route| !route.hidden) {
        if !route.documented {
            report.push(
                "undocumented_endpoint",
                route.location(),
                format!(
                    "`{} {}` is registered without `#[endpoint]`",
                    route.method.to_uppercase(),
                    route.path
                ),
                "an undocumented operation is marked `x-moso-undocumented` and contributes \
                 nothing to a generated client, so every consumer hand-writes it",
                "put `#[endpoint]` on the handler and register it with `routes!` or `ep!`"
                    .to_owned(),
            );
        }

        if let Some(document) = document
            && operation(document, &route.path, &route.method).is_none()
        {
            report.push(
                "route_not_in_document",
                route.location(),
                format!(
                    "`{} {}` is served but has no operation in the OpenAPI document",
                    route.method.to_uppercase(),
                    route.path
                ),
                "a route absent from the document is absent from every generated client and \
                 from `openapi check`, so it can change without anything noticing",
                "register it through `routes!` rather than `mount_axum`, or mark it `.hidden()` \
                 so its absence is a decision"
                    .to_owned(),
            );
        }
    }

    let Some(document) = document else {
        HANDLERS.with_borrow_mut(Vec::clear);
        return;
    };

    HANDLERS.with_borrow_mut(|handlers| {
        for handler in handlers.drain(..) {
            let serving: Vec<&Route> = routes
                .iter()
                .filter(|route| route.handler == handler.name)
                .collect();
            // A handler this scan found but no route serves is not this lint's
            // business: it may be dead code, it may be mounted by a name the
            // router does not carry, and guessing either way is worse than
            // saying nothing.
            for route in serving {
                let declared = declared_statuses(document, &route.path, &route.method);
                for (status, line, constructor) in &handler.statuses {
                    if declared.contains(status) {
                        continue;
                    }
                    report.push(
                        "unhandled_error_variant",
                        at(&handler.file, *line, 1),
                        format!(
                            "`{}..)` produces {status}, which `{} {}` does not declare",
                            constructor,
                            route.method.to_uppercase(),
                            route.path
                        ),
                        "the documented responses are the contract; a status a client is never \
                         told about is a status it will not handle",
                        format!(
                            "declare it — `#[endpoint(errors = MyError)]` on `{}`, with {status} \
                             among the type's variants",
                            handler.name
                        ),
                    );
                }
            }
        }
    });
}

/// The operation object for one route, if the document has one.
fn operation<'a>(document: &'a Value, path: &str, method: &str) -> Option<&'a Value> {
    document.get("paths")?.get(path)?.get(method.to_lowercase())
}

/// The status codes one operation declares.
fn declared_statuses(document: &Value, path: &str, method: &str) -> Vec<u16> {
    operation(document, path, method)
        .and_then(|operation| operation.get("responses"))
        .and_then(Value::as_object)
        .map(|responses| {
            responses
                .keys()
                .filter_map(|key| key.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// `env_example_drift`.
///
/// The comparison itself lives in
/// [`config_check::example_drift`](super::config_check::example_drift), which
/// `moso config --check` also calls. Two commands report this by name, and a
/// second comparison here would let them disagree about whether a given file
/// has drifted — which is the failure mode that makes a check worse than no
/// check, because whichever one a team runs is the one they believe.
fn check_env_example(root: &Path, regenerated: &str, report: &mut Report) {
    let path = root.join(".env.example");
    let Ok(committed) = std::fs::read_to_string(&path) else {
        // No committed file is not drift. `moso config --env-example` is how one
        // comes to exist, and nagging about a file a project chose not to keep
        // is how a lint gets switched off wholesale. `moso config --check` is
        // the command that warns about the absence, because a project auditing
        // its configuration has asked for that opinion.
        return;
    };
    let Some(detail) = super::config_check::example_drift(&committed, regenerated) else {
        return;
    };
    report.push(
        "env_example_drift",
        (Some(".env.example"), None, ".env.example".to_owned()),
        format!("`.env.example` is not what the `Config` type generates: {detail}"),
        "the example is the only documentation of what an operator must set, and one that \
         has drifted sends them looking for a variable that no longer exists",
        super::config_check::ENV_EXAMPLE_FIX.to_owned(),
    );
}

/// `missing_authz` and `unknown_permission`.
///
/// # Errors
/// [`Fault::User`](crate::exit::Fault::User) when `--authz` was asked for and
/// the application does not use the battery — that is a request that cannot be
/// satisfied, and answering it with "no problems found" would be the worst
/// possible lie for a deny-by-default check.
fn check_authz(document: &Value, report: &mut Report) -> Outcome<()> {
    if document.get("available").and_then(Value::as_bool) != Some(true) {
        let reason = document
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("this project does not use moso-authz");
        return Err(
            CliError::user(format!("--authz cannot be checked: {reason}")).with_help(
                document.get("help").and_then(Value::as_str).map_or_else(
                    || "add `moso-authz` and implement `fn authz` in src/dump.rs".to_owned(),
                    str::to_owned,
                ),
            ),
        );
    }

    for entry in document
        .get("undeclared")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let method = entry.get("method").and_then(Value::as_str).unwrap_or("?");
        let path = entry.get("path").and_then(Value::as_str).unwrap_or("?");
        let source = entry.get("source").and_then(Value::as_str);
        report.push(
            "missing_authz",
            (
                None,
                None,
                source.map_or_else(|| format!("{method} {path}"), str::to_owned),
            ),
            format!("`{method} {path}` declares no authorization"),
            "deny by default is only provable if every operation says which it is; an \
             endpoint that says nothing is indistinguishable from one that was forgotten",
            "add `#[requires(Perm::..)]`, take an `Authorized<..>` parameter, or mark it \
             `#[public]` if it is meant to be open"
                .to_owned(),
        );
    }

    for entry in document
        .get("problems")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let at = entry.get("at").and_then(Value::as_str).unwrap_or("?");
        let message = entry
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("names a permission the registry does not declare");
        report.push(
            "unknown_permission",
            (None, None, at.to_owned()),
            format!("`{at}`: {message}"),
            "a permission named by a string is checked against the registry at boot; one that \
             is not in it can never be granted, so the endpoint refuses everyone forever",
            entry.get("suggestion").and_then(Value::as_str).map_or_else(
                || "correct the name, or declare it in `moso::permissions!`".to_owned(),
                |suggestion| format!("did you mean `{suggestion}`?"),
            ),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Print the findings and decide the exit code.
fn emit(ui: &Ui, report: &Report, authz: bool) -> Outcome<()> {
    let denied = report
        .findings
        .iter()
        .filter(|finding| finding.level == Level::Deny)
        .count();

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": denied == 0,
            "findings": report.findings.iter().map(Finding::to_json).collect::<Vec<_>>(),
            "denied": denied,
            "total": report.findings.len(),
            "lints": report.levels.iter()
                .map(|(name, level)| (*name, level.as_str()))
                .collect::<BTreeMap<_, _>>(),
        }));
    } else if report.findings.is_empty() {
        ui.blank();
        ui.status(Glyph::Ok, "no problems found", &enabled_summary(report));
        if !authz {
            ui.line(
                &ui.dim("      `moso check --authz` also checks that every operation declares one"),
            );
        }
        ui.blank();
    } else {
        ui.blank();
        for finding in &report.findings {
            ui.line(&format!(
                "{}[{}]: {}",
                ui.bold(finding.level.heading()),
                finding.lint,
                finding.message
            ));
            ui.line(&format!("  --> {}", finding.location));
            ui.line(&ui.dim(&format!("   = note: {}", finding.note)));
            ui.line(&format!("   = help: {}", finding.help));
            ui.blank();
        }
        let warned = report.findings.len() - denied;
        ui.status(
            if denied == 0 {
                Glyph::Warn
            } else {
                Glyph::Fail
            },
            &format!("{denied} error(s), {warned} warning(s)"),
            &enabled_summary(report),
        );
        ui.blank();
    }

    if denied == 0 {
        return Ok(());
    }
    Err(
        CliError::user(format!("{denied} lint(s) fired at `deny`")).with_help(
            "each is printed above with the line to change; `[lints]` in moso.toml sets any \
             of them to \"warn\" or \"allow\"",
        ),
    )
}

/// How many lints ran, for the detail column.
fn enabled_summary(report: &Report) -> String {
    let ran = report
        .levels
        .values()
        .filter(|level| **level != Level::Allow)
        .count();
    format!("({ran} of {} lints enabled)", LINTS.len())
}

// ---------------------------------------------------------------------------
// Reading the dumps
// ---------------------------------------------------------------------------

/// Parse one dump, naming the flag in the failure.
fn parse_json(answer: &str, flag: &str) -> Outcome<Value> {
    serde_json::from_str(answer).map_err(|error| {
        CliError::user(format!(
            "the application's `{flag}` output is not JSON: {error}"
        ))
        .with_help("everything except the document must go to stderr")
    })
}

/// Parse the fields of `--dump-routes` these lints read.
fn parse_routes(answer: &str) -> Outcome<Vec<Route>> {
    let value = parse_json(answer, "--dump-routes")?;
    let routes = value
        .get("routes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CliError::user("the application's `--dump-routes` output has no `routes` array")
                .with_help("compare src/dump.rs with the one `moso new` writes")
        })?;

    Ok(routes
        .iter()
        .map(|route| Route {
            method: route
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_lowercase(),
            path: route
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            handler: route
                .get("handler")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            documented: route
                .get("documented")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            hidden: route
                .get("hidden")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            source: route
                .get("source")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report with every lint at its default, for a test that only cares
    /// about one of them.
    fn report() -> Report {
        Report {
            levels: LINTS.iter().map(|lint| (lint.name, lint.default)).collect(),
            findings: Vec::new(),
        }
    }

    /// Scan one file's worth of source the way `scan_sources` does.
    fn scan(file: &str, source: &str, report: &mut Report) {
        let raw: Vec<&str> = source.lines().collect();
        let clean = strip(source);
        let functions = functions(&raw, &clean);
        check_layering(file, &clean, report);
        check_blocking(file, &clean, &functions, report);
        check_loops(file, &clean, report);
        check_stale_layer(file, &clean, &functions, report);
        record_handlers(file, &clean, &functions, report);
    }

    // ── the scanner ───────────────────────────────────────────────────────

    #[test]
    fn comments_and_strings_are_blanked_and_the_columns_survive() {
        let cleaned = strip("let a = \"std::fs::read\"; // std::fs::read\nlet b = 1;");
        assert!(!cleaned[0].contains("std::fs"));
        assert_eq!(
            cleaned[0].len(),
            "let a = \"std::fs::read\"; // std::fs::read".len()
        );
        assert!(cleaned[0].starts_with("let a = "));
        assert_eq!(cleaned[1].trim_end(), "let b = 1;");
    }

    #[test]
    fn a_block_comment_spanning_lines_is_blanked_and_nests() {
        let cleaned = strip("a /* one\n /* two */ still\n */ b");
        assert!(cleaned[0].trim_end().ends_with('a'));
        assert!(cleaned[1].trim().is_empty());
        assert_eq!(cleaned[2].trim(), "b");
    }

    #[test]
    fn a_raw_string_is_blanked_and_a_lifetime_is_not_a_char_literal() {
        let cleaned = strip("let a = r#\"a \" b\"#; let c: &'static str = \"x\"; let d = 'y';");
        assert!(!cleaned[0].contains("a \" b"));
        // The lifetime must not swallow the rest of the line.
        assert!(cleaned[0].contains("let c"));
        assert!(cleaned[0].contains("let d"));
    }

    #[test]
    fn a_function_is_found_with_its_body_and_its_shape() {
        let source = "\
/// Mount it.
#[endpoint]
pub async fn list() -> Result<Vec<u8>> {
    Ok(Vec::new())
}

fn router() -> Router {
    Router::new()
}
";
        let raw: Vec<&str> = source.lines().collect();
        let found = functions(&raw, &strip(source));
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "list");
        assert!(found[0].is_async);
        assert!(found[0].is_endpoint);
        assert!(!found[0].returns_router);
        assert_eq!(found[0].start, 2);
        assert_eq!(found[0].end, 4);
        assert_eq!(found[1].name, "router");
        assert!(found[1].returns_router);
        assert!(!found[1].is_endpoint);
    }

    #[test]
    fn a_closure_bound_is_not_mistaken_for_a_definition() {
        let source = "fn take(f: impl Fn(u8) -> u8) -> u8 { f(1) }\n";
        let raw: Vec<&str> = source.lines().collect();
        let found = functions(&raw, &strip(source));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "take");
    }

    // ── the lexical lints ─────────────────────────────────────────────────

    #[test]
    fn blocking_calls_are_reported_only_inside_an_async_fn() {
        let mut report = report();
        scan(
            "src/routes/users.rs",
            "\
async fn slow() {
    std::fs::read(\"x\").ok();
}

fn fine() {
    std::fs::read(\"x\").ok();
}
",
            &mut report,
        );
        let found: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|finding| finding.lint == "blocking_in_async")
            .collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, Some(2));
        assert_eq!(found[0].level, Level::Deny);
    }

    #[test]
    fn a_load_inside_a_loop_is_the_n_plus_one_and_one_outside_is_not() {
        let mut report = report();
        scan(
            "src/services/posts.rs",
            "\
async fn go() {
    post.load(Post::AUTHOR, &db).await?;
    for post in &mut posts {
        post.load(Post::AUTHOR, &db).await?;
    }
}
",
            &mut report,
        );
        let found: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|finding| finding.lint == "n_plus_one")
            .collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, Some(4));
    }

    #[test]
    fn a_loop_written_on_one_line_is_still_a_loop() {
        let mut report = report();
        scan(
            "src/services/posts.rs",
            "async fn go() {\n    for post in &mut posts { post.load(A, &db).await?; }\n}\n",
            &mut report,
        );
        let found: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|finding| finding.lint == "n_plus_one")
            .collect();
        assert_eq!(found.len(), 1, "{:#?}", report.findings);
        assert_eq!(found[0].line, Some(2));
    }

    #[test]
    fn an_impl_for_is_not_a_loop() {
        let mut report = report();
        scan(
            "src/models/post.rs",
            "\
impl Load for Post {
    fn go(&self) { self.load(X, &db); }
}
",
            &mut report,
        );
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.lint == "n_plus_one"),
            "{:#?}",
            report.findings
        );
    }

    #[test]
    fn a_layer_after_every_route_is_reported_and_one_before_is_not() {
        let mut report = report();
        scan(
            "src/routes.rs",
            "\
pub fn router() -> Router {
    Router::new()
        .get(\"/a\", ep!(a))
        .layer(AuthLayer::new())
}

pub fn other() -> Router {
    Router::new()
        .get(\"/b\", ep!(b))
        .layer(AuthLayer::new())
        .get(\"/c\", ep!(c))
}
",
            &mut report,
        );
        let found: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|finding| finding.lint == "stale_layer")
            .collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, Some(4));
        assert!(found[0].message.contains("router"));
    }

    #[test]
    fn a_layer_in_a_doc_comment_is_not_a_layer() {
        let mut report = report();
        scan(
            "src/routes.rs",
            "\
/// Never write .layer() last.
pub fn router() -> Router {
    Router::new().get(\"/a\", ep!(a))
}
",
            &mut report,
        );
        assert!(report.findings.is_empty(), "{:#?}", report.findings);
    }

    #[test]
    fn layering_reports_the_import_the_layer_may_not_have() {
        let mut report = report();
        scan("src/routes/users.rs", "use sqlx::query;\n", &mut report);
        scan("src/services/users.rs", "use axum::Json;\n", &mut report);
        scan("src/models/user.rs", "use crate::routes::x;\n", &mut report);
        let found: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|finding| finding.lint == "layering")
            .collect();
        assert_eq!(found.len(), 3);
        assert!(found.iter().all(|finding| finding.level == Level::Deny));
    }

    #[test]
    fn a_file_outside_a_named_layer_is_not_layered() {
        let mut report = report();
        scan("src/lib.rs", "use sqlx::query;\n", &mut report);
        assert!(report.findings.is_empty(), "{:#?}", report.findings);
    }

    // ── the document lints ────────────────────────────────────────────────

    const ROUTES: &str = r#"{
      "routes": [
        {"method":"GET","path":"/users","handler":"list","documented":true,"hidden":false,
         "source":"src/routes/users.rs:14"},
        {"method":"POST","path":"/users","handler":"create","documented":false,"hidden":false,
         "source":null},
        {"method":"GET","path":"/_internal","handler":"internal","documented":false,
         "hidden":true,"source":null}
      ]
    }"#;

    #[test]
    fn an_undocumented_route_is_reported_and_a_hidden_one_is_not() {
        let routes = parse_routes(ROUTES).expect("parsed");
        let mut report = report();
        check_routes(&routes, None, &mut report);
        let found: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|finding| finding.lint == "undocumented_endpoint")
            .collect();
        assert_eq!(found.len(), 1);
        assert!(found[0].message.contains("POST /users"));
    }

    #[test]
    fn a_route_absent_from_the_document_is_reported_against_its_source_line() {
        let routes = parse_routes(ROUTES).expect("parsed");
        let document = serde_json::json!({"paths": {"/users": {"post": {"responses": {}}}}});
        let mut report = report();
        check_routes(&routes, Some(&document), &mut report);
        let found: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|finding| finding.lint == "route_not_in_document")
            .collect();
        assert_eq!(found.len(), 1);
        assert!(found[0].message.contains("GET /users"));
        assert_eq!(found[0].location, "src/routes/users.rs:14");
    }

    #[test]
    fn a_constructed_status_the_operation_does_not_declare_is_reported() {
        HANDLERS.with_borrow_mut(Vec::clear);
        let mut report = report();
        scan(
            "src/routes/users.rs",
            "\
#[endpoint]
async fn list() -> Result<u8> {
    Err(Error::conflict(\"taken\"))
}
",
            &mut report,
        );
        let routes = parse_routes(ROUTES).expect("parsed");
        let document = serde_json::json!({
            "paths": {"/users": {"get": {"responses": {"200": {}, "422": {}}}}}
        });
        check_routes(&routes, Some(&document), &mut report);

        let found: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|finding| finding.lint == "unhandled_error_variant")
            .collect();
        assert_eq!(found.len(), 1);
        assert!(found[0].message.contains("409"));
        assert_eq!(found[0].line, Some(3));
    }

    #[test]
    fn a_declared_status_is_not_reported() {
        HANDLERS.with_borrow_mut(Vec::clear);
        let mut report = report();
        scan(
            "src/routes/users.rs",
            "#[endpoint]\nasync fn list() -> Result<u8> { Err(Error::conflict(\"x\")) }\n",
            &mut report,
        );
        let routes = parse_routes(ROUTES).expect("parsed");
        let document = serde_json::json!({
            "paths": {"/users": {"get": {"responses": {"409": {}}}}}
        });
        check_routes(&routes, Some(&document), &mut report);
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.lint == "unhandled_error_variant"),
            "{:#?}",
            report.findings
        );
    }

    #[test]
    fn an_authz_answer_from_a_project_without_the_battery_is_a_user_error() {
        let mut report = report();
        let document = serde_json::json!({"available": false, "reason": "no moso-authz"});
        let error = check_authz(&document, &mut report).expect_err("cannot be checked");
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert!(error.message.contains("no moso-authz"));
    }

    #[test]
    fn undeclared_operations_and_mistyped_permissions_both_report() {
        let mut report = Report {
            levels: LINTS.iter().map(|lint| (lint.name, lint.default)).collect(),
            findings: Vec::new(),
        };
        let document = serde_json::json!({
            "available": true,
            "undeclared": [{"method": "POST", "path": "/posts", "source": null}],
            "problems": [{"at": "POST /posts", "message": "unknown permission `posts.pubish`",
                          "suggestion": "posts.publish"}],
        });
        check_authz(&document, &mut report).expect("checked");
        assert_eq!(report.findings.len(), 2);
        assert_eq!(report.findings[0].lint, "missing_authz");
        assert_eq!(report.findings[0].level, Level::Warn);
        assert_eq!(report.findings[1].lint, "unknown_permission");
        assert_eq!(report.findings[1].level, Level::Deny);
        assert!(report.findings[1].help.contains("posts.publish"));
    }

    // ── levels ────────────────────────────────────────────────────────────

    #[test]
    fn strict_promotes_warnings_and_leaves_allow_alone() {
        let mut configured = BTreeMap::new();
        configured.insert("n_plus_one".to_owned(), Level::Allow);
        let levels = resolve(&configured, &[], true, true).expect("resolved");
        assert_eq!(levels["n_plus_one"], Level::Allow);
        assert_eq!(levels["stale_layer"], Level::Deny);
        assert_eq!(levels["blocking_in_async"], Level::Deny);
    }

    #[test]
    fn the_authz_lints_are_off_until_authz_is_asked_for() {
        let levels = resolve(&BTreeMap::new(), &[], false, false).expect("resolved");
        assert_eq!(levels["missing_authz"], Level::Allow);
        assert_eq!(levels["unknown_permission"], Level::Allow);

        let asked = resolve(&BTreeMap::new(), &[], false, true).expect("resolved");
        assert_eq!(asked["missing_authz"], Level::Warn);
    }

    #[test]
    fn lint_selects_one_and_switches_the_rest_off() {
        let only = vec!["stale_layer".to_owned()];
        let levels = resolve(&BTreeMap::new(), &only, false, true).expect("resolved");
        assert_eq!(levels["stale_layer"], Level::Warn);
        assert_eq!(levels["blocking_in_async"], Level::Allow);
    }

    #[test]
    fn an_unknown_lint_name_is_a_usage_error_listing_the_real_ones() {
        let only = vec!["nplus1".to_owned()];
        let error = resolve(&BTreeMap::new(), &only, false, true).expect_err("rejected");
        assert_eq!(error.fault, crate::exit::Fault::Usage);
        assert!(error.help.is_some_and(|help| help.contains("n_plus_one")));
    }

    #[test]
    fn moso_toml_sets_a_level_and_an_unknown_key_is_returned_rather_than_ignored() {
        let root = std::env::temp_dir().join(format!("moso-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch");
        std::fs::write(
            root.join("moso.toml"),
            "[lints]\nmissing_authz = \"deny\"\nmising_authz = \"deny\"\n",
        )
        .expect("moso.toml");

        let (levels, unknown) = configured_levels(&root).expect("read");
        assert_eq!(levels["missing_authz"], Level::Deny);
        assert_eq!(unknown, vec!["mising_authz".to_owned()]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_level_that_is_not_one_of_the_three_words_is_a_user_error() {
        let root = std::env::temp_dir().join(format!("moso-check-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch");
        std::fs::write(root.join("moso.toml"), "[lints]\nlayering = \"forbid\"\n")
            .expect("moso.toml");

        let error = configured_levels(&root).expect_err("rejected");
        assert_eq!(error.fault, crate::exit::Fault::User);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_project_with_no_moso_toml_uses_the_defaults() {
        let root = std::env::temp_dir().join(format!("moso-check-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch");
        let (levels, unknown) = configured_levels(&root).expect("read");
        assert!(levels.is_empty());
        assert!(unknown.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_lint_name_is_unique_and_has_a_description() {
        let mut names: Vec<&str> = LINTS.iter().map(|lint| lint.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), LINTS.len(), "two lints share a name");
        for lint in LINTS {
            assert!(!lint.catches.is_empty(), "{} has no description", lint.name);
            assert_ne!(
                lint.default,
                Level::Allow,
                "{} is off by default",
                lint.name
            );
        }
    }

    #[test]
    fn env_example_drift_is_silent_without_a_committed_file() {
        let root = std::env::temp_dir().join(format!("moso-check-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch");

        let mut report = report();
        check_env_example(&root, "SHOP__GREETING=hello\n", &mut report);
        assert!(report.findings.is_empty());

        std::fs::write(root.join(".env.example"), "SHOP__GREETING=hi\n").expect("example");
        check_env_example(&root, "SHOP__GREETING=hello\n", &mut report);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].lint, "env_example_drift");

        // Trailing whitespace is not drift: the dump is trimmed and the file is
        // whatever an editor left behind.
        report.findings.clear();
        check_env_example(&root, "SHOP__GREETING=hi", &mut report);
        assert!(report.findings.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn this_lint_and_moso_config_check_agree_about_what_drift_is() {
        // The two commands report `env_example_drift` by name, so they have to
        // answer the same way. They do because they share one comparison; this
        // fails the day somebody writes a second one here.
        use super::super::config_check::example_drift;

        let root = std::env::temp_dir().join(format!("moso-check-agree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch");

        let cases = [
            ("A=1\n", "A=1\n"),
            ("A=1   \n", "A=1\n"),
            ("A=1", "A=1\n\n"),
            ("A=1\n", "A=1\nB=2\n"),
            ("A=1\nB=2\n", "A=1\n"),
            ("# one\nA=1\n", "# two\nA=1\n"),
        ];
        for (committed, generated) in cases {
            std::fs::write(root.join(".env.example"), committed).expect("example");
            let mut report = report();
            check_env_example(&root, generated, &mut report);
            assert_eq!(
                report.findings.len(),
                usize::from(example_drift(committed, generated).is_some()),
                "the lint and `moso config --check` disagree about {committed:?} vs {generated:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
