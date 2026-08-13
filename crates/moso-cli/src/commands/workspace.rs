//! `moso generate workspace` — one crate becomes a Cargo workspace.
//!
//! ```text
//! shop/                        shop/
//! ├── Cargo.toml               ├── Cargo.toml          [workspace] + the profiles
//! ├── src/                     ├── Dockerfile          .env.example, README, .cargo/
//! ├── tests/            ──▶    └── crates/
//! ├── migrations/                  └── shop/           Cargo.toml, src/, tests/,
//! └── Dockerfile                                       migrations/, config/
//! ```
//!
//! `00-foundations/04-project-structure.md` asks for this at roughly 20k lines,
//! and asks for it to be **mechanical and reversible**. Three decisions follow
//! from that word.
//!
//! **The package keeps its name.** It moves to `crates/shop`, not to
//! `crates/shop-app`. A rename would mean rewriting `use shop::…` in the binary
//! and in every integration test — the one thing in the tree that names the
//! library rather than reaching it through `crate::` — and it would move
//! `target/release/shop`, which the generated `Dockerfile` copies by name. The
//! layout is a file move; nothing textual has to be right for the project to
//! still compile.
//!
//! **Only the package moves.** `Cargo.toml`, `src/`, `tests/`, `benches/`,
//! `examples/`, `build.rs`, `migrations/` and `config/` are things cargo or the
//! process resolves relative to the package, so they follow it. `.env`,
//! `README.md`, the `Dockerfile` and `.cargo/config.toml` are things the
//! *project* has one of, so they stay at the root where every tool already
//! looks for them.
//!
//! **Two things in the manifest are rewritten, and only two.** `[profile.*]` is
//! lifted to the root, because cargo ignores a profile declared in a
//! non-root manifest and only says so in a warning; and a relative `path = "…"`
//! dependency is re-rooted, because the manifest it lives in just moved two
//! directories down. Every other line — every comment, every version, every
//! feature list — is moved byte for byte.
//!
//! # Failing rather than half-migrating
//!
//! Everything is planned before anything is touched: the preconditions are
//! checked, the destination is proved empty, and the two manifests are rendered
//! in memory. A move that fails part-way is undone, in reverse, before the
//! error is reported. In a git repository the tree must also be clean unless
//! `--force` is given — not because the command needs it, but because a clean
//! tree is what makes the whole thing one `git checkout` to undo.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::GenerateArgs;
use crate::exit::{CliError, Outcome, io as io_error};
use crate::project::Project;
use crate::ui::{Level, Ui};

/// The root manifest the split writes.
const ROOT_MANIFEST: &str = include_str!("../../templates/generate/workspace.toml.tpl");

/// Where the members go.
const CRATES: &str = "crates";

/// What moves with the package, in the order it is listed to the reader.
///
/// Each of these is resolved relative to the package: the first six by cargo,
/// `migrations/` and `config/` by the application at runtime, from the working
/// directory cargo gives it. Everything else a project has — `.env`,
/// `README.md`, `Dockerfile`, `.cargo/config.toml`, `openapi.json` — belongs to
/// the project rather than to the package and stays where every tool already
/// looks for it.
const PACKAGE_FILES: &[&str] = &[
    "Cargo.toml",
    "src",
    "tests",
    "benches",
    "examples",
    "build.rs",
    "migrations",
    "config",
];

/// Manifest keys that name a path relative to the manifest and that this
/// command cannot fix, because the file they name is a project file that stays
/// at the root.
const ROOT_RELATIVE_KEYS: &[&str] = &["readme", "license-file", "include", "exclude"];

/// How far the package manifest descends: `crates/<name>/`.
const HOPS: usize = 2;

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// One thing to move, as absolute paths.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Move {
    /// Where it is now.
    from: PathBuf,
    /// Where it goes.
    to: PathBuf,
    /// How it reads in the report, relative to the project root.
    label: String,
}

/// Everything one invocation will do, decided before anything is touched.
#[derive(Debug, Clone)]
struct Plan {
    /// The directory that becomes the workspace root.
    root: PathBuf,
    /// The package's name, which is also its new directory.
    package: String,
    /// The moves, in order.
    moves: Vec<Move>,
    /// The new root manifest.
    root_manifest: String,
    /// The package manifest, rewritten.
    package_manifest: String,
    /// What the command could not do for the reader.
    warnings: Vec<String>,
}

/// Run `moso generate workspace`.
///
/// # Errors
/// [`Fault::User`](crate::exit::Fault::User) when the project is already a
/// workspace, when `crates/` is in the way, or when the git tree is dirty and
/// `--force` was not given; and
/// [`Fault::Environment`](crate::exit::Fault::Environment) when the filesystem
/// refuses a move — in which case every move already made has been undone.
pub fn run(ui: &Ui, args: &GenerateArgs) -> Outcome<()> {
    let project = discover(args.manifest_path.as_deref())?;
    project.require_moso()?;
    let plan = plan(&project, args.force)?;

    if args.dry_run {
        return preview(ui, &plan);
    }

    apply(&plan)?;
    report(ui, &plan);
    Ok(())
}

/// Find the package, and recognise a project this command has already split.
///
/// The check runs **before** [`Project::discover`], and the order is the whole
/// point. After a split the directory the user is standing in is a *virtual*
/// workspace root — a `Cargo.toml` with no `[package]` — and discovery resolves
/// that to the single member below it, which is correct for every other command
/// and exactly wrong for this one: it would find the package it moved last time
/// and split it again, into `crates/shop/crates/shop`. Asking "is this already a
/// workspace root" first turns a second run into "already split" rather than
/// into a nested one.
fn discover(explicit: Option<&Path>) -> Outcome<Project> {
    match split_already(explicit) {
        Some(error) => Err(error),
        None => Project::discover(explicit),
    }
}

/// The "already a workspace" error, when that is what the failure really was.
fn split_already(explicit: Option<&Path>) -> Option<CliError> {
    if explicit.is_some() {
        return None;
    }
    already_split_at(&std::env::current_dir().ok()?)
}

/// Whether `start` is inside a project this command has already split.
///
/// Takes the directory rather than reading it from the process, so that the
/// test for it does not have to move the process into a scratch tree — a cwd
/// two tests can both be inside is a flake waiting for a slow machine.
fn already_split_at(start: &Path) -> Option<CliError> {
    let manifest = start
        .ancestors()
        .map(|directory| directory.join("Cargo.toml"))
        .find(|candidate| candidate.is_file())?;

    let text = std::fs::read_to_string(&manifest).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    if value.get("package").is_some() {
        return None;
    }
    let workspace = value.get("workspace")?;
    let members = workspace
        .get("members")
        .and_then(toml::Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    Some(
        CliError::user(format!(
            "`{}` is already a workspace root",
            manifest.display()
        ))
        .with_help(if members.is_empty() {
            "add the next crate with `cargo new --lib crates/<name>`".to_owned()
        } else {
            format!(
                "its members are {members}; add the next crate with \
                 `cargo new --lib crates/<name>` and the glob picks it up"
            )
        }),
    )
}

/// Check every precondition and decide everything, touching nothing.
fn plan(project: &Project, force: bool) -> Outcome<Plan> {
    let root = project.root.clone();
    let text = std::fs::read_to_string(&project.manifest_path)
        .map_err(|error| io_error("could not read", &project.manifest_path, &error))?;
    let manifest: toml::Value = toml::from_str(&text).map_err(|error| {
        CliError::user(format!(
            "`{}` is not valid TOML: {error}",
            project.manifest_path.display()
        ))
    })?;

    already_a_workspace(&manifest)?;

    let destination = root.join(CRATES);
    if destination.exists() {
        return Err(CliError::user(format!(
            "`{CRATES}/` already exists in `{}`",
            root.display()
        ))
        .with_help(
            "this project has already been split, or something else lives there; move \
                     it aside and run the command again",
        ));
    }
    if !force {
        clean_tree(&root)?;
    }

    let package_root = destination.join(&project.name);
    let moves: Vec<Move> = PACKAGE_FILES
        .iter()
        .map(|name| Move {
            from: root.join(name),
            to: package_root.join(name),
            label: (*name).to_owned(),
        })
        .filter(|entry| entry.from.exists())
        .collect();

    let (remainder, profiles) = lift(&text);
    let package_manifest = reroot(&remainder, HOPS);
    let lib_name = project.name.replace('-', "_");
    let root_manifest = ROOT_MANIFEST
        .replace("@@PROFILES@@", profiles.trim_end())
        .replace("@@CRATE_NAME@@", &project.name)
        .replace("@@LIB_NAME@@", &lib_name);

    Ok(Plan {
        warnings: warnings(&manifest, &root),
        root,
        package: project.name.clone(),
        moves,
        root_manifest,
        package_manifest,
    })
}

/// Refuse a manifest that is already a workspace root.
///
/// An *empty* `[workspace]` table is not one: it is the stanza `moso new`
/// writes when the project sits inside somebody else's workspace, and detaching
/// from an outer workspace is exactly the state this command starts from. Any
/// other `workspace` content — members, `workspace.package`,
/// `workspace.dependencies` — is a split that has already happened, and
/// overwriting it would throw away whatever it says.
fn already_a_workspace(manifest: &toml::Value) -> Outcome<()> {
    let Some(workspace) = manifest.get("workspace") else {
        return Ok(());
    };
    let empty = workspace.as_table().is_some_and(|table| table.is_empty());
    if empty {
        return Ok(());
    }
    Err(
        CliError::user("this package is already a workspace root").with_help(
            "add the next crate with `cargo new --lib crates/<name>`; the `crates/*` glob \
             picks it up",
        ),
    )
}

/// Refuse to move files over uncommitted work.
fn clean_tree(root: &Path) -> Outcome<()> {
    if !git(root, &["rev-parse", "--is-inside-work-tree"]) {
        return Ok(());
    }
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Ok(());
    };
    if String::from_utf8_lossy(&output.stdout).trim().is_empty() {
        return Ok(());
    }
    Err(
        CliError::user("the working tree has uncommitted changes").with_help(
            "commit or stash first — this moves your files, and a clean tree is what makes \
             `git checkout .` undo it; or pass --force",
        ),
    )
}

/// What the command cannot do, said out loud rather than left to be discovered.
fn warnings(manifest: &toml::Value, root: &Path) -> Vec<String> {
    let mut found = Vec::new();

    if let Some(package) = manifest.get("package") {
        let named: Vec<&str> = ROOT_RELATIVE_KEYS
            .iter()
            .filter(|key| package.get(**key).is_some())
            .copied()
            .collect();
        if !named.is_empty() {
            found.push(format!(
                "[package] still names {} — those paths were relative to the old root",
                named.join(", ")
            ));
        }
    }

    if root.join("Dockerfile").is_file() {
        found.push(
            "Dockerfile: its dependency-cache stage copies `Cargo.toml` and builds a stub, \
             which assumed one package — copy `crates/` too, or drop the stage"
                .to_owned(),
        );
    }

    found
}

// ---------------------------------------------------------------------------
// Rewriting the manifest
// ---------------------------------------------------------------------------

/// Split a manifest's text into everything that stays and every `[profile.*]`.
///
/// Textual rather than a parse-and-serialise round trip, and that is the whole
/// point: `toml::Value` does not carry comments, and the generated manifest is
/// four fifths comments explaining the choices in it. A section is its header,
/// the body up to the next header, and the comment block written immediately
/// above it — which is the comment that explains it and must travel with it.
#[must_use]
pub fn lift(text: &str) -> (String, String) {
    let mut stays: Vec<&str> = Vec::new();
    let mut lifted: Vec<&str> = Vec::new();
    let mut moving = false;

    for line in text.lines() {
        if let Some(header) = table_header(line) {
            let was_moving = moving;
            moving = header == "profile" || header.starts_with("profile.");
            if was_moving && !moving {
                stays.push("");
            }
            if moving {
                // The comment block written directly above the header explains
                // the section, so it goes with it.
                let mut leading = Vec::new();
                while stays
                    .last()
                    .is_some_and(|previous| previous.trim_start().starts_with('#'))
                {
                    leading.push(stays.pop().expect("just checked"));
                }
                leading.reverse();
                while stays
                    .last()
                    .is_some_and(|previous| previous.trim().is_empty())
                {
                    stays.pop();
                }
                if !lifted.is_empty() {
                    lifted.push("");
                }
                lifted.extend(leading);
            }
        }
        if moving {
            lifted.push(line);
        } else {
            stays.push(line);
        }
    }

    let mut remainder = drop_empty_workspace(&stays.join("\n"));
    while remainder.ends_with('\n') {
        remainder.pop();
    }
    remainder.push('\n');

    let mut profiles = lifted.join("\n");
    if !profiles.is_empty() {
        profiles.insert(0, '\n');
        profiles.push('\n');
    }
    (remainder, profiles)
}

/// The table name of a header line, or `None`.
fn table_header(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    // `[[bin]]` is an array of tables; its name is what is left after the
    // second bracket, and it is never a profile.
    Some(inner.trim_start_matches('[').trim_end_matches(']'))
}

/// Remove the bare `[workspace]` stanza `moso new` writes inside a workspace.
///
/// Only the empty one ever reaches here: [`already_a_workspace`] refused
/// anything with content in it.
fn drop_empty_workspace(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        if let Some(header) = table_header(line) {
            let was_skipping = skipping;
            skipping = header == "workspace";
            if skipping {
                while out
                    .last()
                    .is_some_and(|previous| previous.trim_start().starts_with('#'))
                {
                    out.pop();
                }
                while out
                    .last()
                    .is_some_and(|previous| previous.trim().is_empty())
                {
                    out.pop();
                }
                continue;
            }
            // What followed the removed stanza still needs a blank line above
            // it, or the manifest comes out with two tables touching.
            if was_skipping
                && out
                    .last()
                    .is_some_and(|previous| !previous.trim().is_empty())
            {
                out.push("");
            }
        }
        if !skipping {
            out.push(line);
        }
    }
    out.join("\n")
}

/// Re-root every relative `path = "…"` by `hops` directories.
///
/// A path dependency is resolved against the directory of the manifest that
/// declares it, and that manifest is about to move `hops` levels down. Without
/// this, `moso new --moso-path ../moso/crates/moso` produces a workspace that
/// does not resolve — which is the state the CLI's own test suite generates.
#[must_use]
pub fn reroot(text: &str, hops: usize) -> String {
    let prefix = "../".repeat(hops);
    let mut out = text
        .lines()
        .map(|line| reroot_line(line, &prefix))
        .collect::<Vec<_>>()
        .join("\n");
    // `lines()` drops the final terminator, and a manifest that does not end in
    // a newline is one every diff after this one starts with a `\ No newline`.
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Re-root every `path = "…"` value on one line.
///
/// Offsets into the original line rather than a rebuilt one, so whatever
/// spacing the author chose survives: `path="x"` and `path = "x"` are both
/// legal TOML and both come back the way they went in.
fn reroot_line(line: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(line.len() + prefix.len());
    let mut cursor = 0;

    while let Some(found) = line[cursor..].find("path") {
        let key_at = cursor + found;
        let after_key = key_at + "path".len();

        let Some(value_at) = assignment_at(line, key_at, after_key) else {
            out.push_str(&line[cursor..after_key]);
            cursor = after_key;
            continue;
        };
        let Some(end) = line[value_at + 1..].find('"') else {
            out.push_str(&line[cursor..after_key]);
            cursor = after_key;
            continue;
        };

        let value = &line[value_at + 1..value_at + 1 + end];
        out.push_str(&line[cursor..=value_at]);
        if is_relative(value) {
            out.push_str(prefix);
        }
        out.push_str(value);
        out.push('"');
        cursor = value_at + 1 + end + 1;
    }

    out.push_str(&line[cursor..]);
    out
}

/// The offset of the opening quote of `path = "…"`, when this really is the
/// `path` key and it really is assigned a string.
///
/// The boundary check is what keeps `search_paths = [..]` and a `mypath` field
/// out of it: `path` has to be a whole key, not the tail of one.
fn assignment_at(line: &str, key_at: usize, after_key: usize) -> Option<usize> {
    let preceding = line[..key_at].chars().next_back();
    if preceding.is_some_and(|before| before.is_alphanumeric() || before == '_' || before == '-') {
        return None;
    }

    let rest = &line[after_key..];
    let gap = rest.len() - rest.trim_start().len();
    let equals = &rest[gap..];
    let after_equals = equals.strip_prefix('=')?;
    let space = after_equals.len() - after_equals.trim_start().len();
    let quote_at = after_key + gap + 1 + space;
    (line[quote_at..].starts_with('"')).then_some(quote_at)
}

/// Whether a manifest path is relative, on either kind of filesystem.
fn is_relative(path: &str) -> bool {
    !(path.starts_with('/')
        || path.starts_with('\\')
        || path.as_bytes().get(1).is_some_and(|second| *second == b':'))
}

// ---------------------------------------------------------------------------
// Doing it
// ---------------------------------------------------------------------------

/// Perform the plan, undoing every move if one of them fails.
fn apply(plan: &Plan) -> Outcome<()> {
    let package_root = plan.root.join(CRATES).join(&plan.package);
    std::fs::create_dir_all(&package_root)
        .map_err(|error| io_error("could not create", &package_root, &error))?;

    let repository = git(&plan.root, &["rev-parse", "--is-inside-work-tree"]);

    let mut done: Vec<&Move> = Vec::new();
    for entry in &plan.moves {
        if let Err(error) = transfer(&plan.root, entry, repository) {
            for made in done.iter().rev() {
                let back = Move {
                    from: made.to.clone(),
                    to: made.from.clone(),
                    label: made.label.clone(),
                };
                let _ = transfer(&plan.root, &back, repository);
            }
            let _ = std::fs::remove_dir_all(plan.root.join(CRATES));
            return Err(error);
        }
        done.push(entry);
    }

    let manifest = package_root.join("Cargo.toml");
    std::fs::write(&manifest, &plan.package_manifest)
        .map_err(|error| io_error("could not write", &manifest, &error))?;
    let root_manifest = plan.root.join("Cargo.toml");
    std::fs::write(&root_manifest, &plan.root_manifest)
        .map_err(|error| io_error("could not write", &root_manifest, &error))
}

/// Move one entry, preferring `git mv` so the history follows.
///
/// A file git does not track — a gitignored `config/`, a `migrations/` nobody
/// has added yet — makes `git mv` fail, and a plain rename is then exactly
/// right. Falling back rather than failing is what keeps the command usable in
/// a repository that is only partly committed.
fn transfer(root: &Path, entry: &Move, repository: bool) -> Outcome<()> {
    if let Some(parent) = entry.to.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| io_error("could not create", parent, &error))?;
    }
    if repository && git_mv(root, &entry.from, &entry.to) {
        return Ok(());
    }
    std::fs::rename(&entry.from, &entry.to)
        .map_err(|error| io_error("could not move", &entry.from, &error))
}

/// `git mv`, reporting whether git took it.
fn git_mv(root: &Path, from: &Path, to: &Path) -> bool {
    Command::new("git")
        .arg("mv")
        .arg(from)
        .arg(to)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// `--dry-run`: say what would happen and change nothing.
fn preview(ui: &Ui, plan: &Plan) -> Outcome<()> {
    if ui.is_json() {
        ui.emit_json(&json(plan, false));
        return Ok(());
    }
    ui.blank();
    for entry in &plan.moves {
        ui.status(
            Level::Ok,
            "would move",
            &format!(
                "{} → {CRATES}/{}/{}",
                entry.label, plan.package, entry.label
            ),
        );
    }
    ui.status(Level::Ok, "would write", "Cargo.toml (the workspace root)");
    ui.status(
        Level::Ok,
        "would rewrite",
        &format!("{CRATES}/{}/Cargo.toml", plan.package),
    );
    for warning in &plan.warnings {
        ui.warn(warning);
    }
    ui.blank();
    Ok(())
}

/// What happened, and what to do next.
fn report(ui: &Ui, plan: &Plan) {
    if ui.is_json() {
        ui.emit_json(&json(plan, true));
        return;
    }

    ui.blank();
    ui.status(
        Level::Ok,
        &format!("moved {CRATES}/{}/", plan.package),
        &format!("({} entries)", plan.moves.len()),
    );
    ui.status(
        Level::Ok,
        "wrote Cargo.toml",
        "(the workspace root, with the profiles)",
    );
    for warning in &plan.warnings {
        ui.warn(warning);
    }

    ui.blank();
    ui.line("  next:");
    ui.line("    cargo test");
    ui.line(&format!("    cd {CRATES}/{}", plan.package));
    ui.line(&format!(
        "    cargo new --lib {CRATES}/{}-domain   # and move the pure types into it",
        plan.package
    ));
    ui.blank();
    ui.line(&format!(
        "  moso commands still work from here while {CRATES}/ holds one package."
    ));
    ui.line(&format!(
        "  Once it holds several, run them from {CRATES}/{} or pass --manifest-path.",
        plan.package
    ));
    ui.blank();
}

/// The `--json` rendering, shared by the preview and the report.
fn json(plan: &Plan, applied: bool) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "applied": applied,
        "root": plan.root.display().to_string(),
        "package": plan.package,
        "moved": plan
            .moves
            .iter()
            .map(|entry| serde_json::json!({
                "from": entry.label,
                "to": format!("{CRATES}/{}/{}", plan.package, entry.label),
            }))
            .collect::<Vec<_>>(),
        "written": ["Cargo.toml", &format!("{CRATES}/{}/Cargo.toml", plan.package)],
        "warnings": plan.warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "\
[package]
name = \"shop\"
version = \"0.1.0\"

# This directory sits inside another Cargo workspace.
[workspace]

[dependencies]
moso = { path = \"../moso/crates/moso\" }
tokio = { version = \"1\", features = [\"macros\"] }

# Your own code stays unoptimised so it compiles fast.
[profile.dev.package.\"*\"]
opt-level = 2
";

    // ── lifting the profiles ──────────────────────────────────────────────

    #[test]
    fn the_profiles_are_lifted_with_the_comment_that_explains_them() {
        let (stays, profiles) = lift(MANIFEST);
        assert!(
            profiles.contains("[profile.dev.package.\"*\"]"),
            "{profiles}"
        );
        assert!(profiles.contains("opt-level = 2"), "{profiles}");
        assert!(
            profiles.contains("# Your own code stays unoptimised"),
            "the comment above a section explains it and must travel with it: {profiles}"
        );
        assert!(!stays.contains("[profile"), "{stays}");
        assert!(!stays.contains("opt-level"), "{stays}");
    }

    #[test]
    fn everything_else_survives_byte_for_byte() {
        let (stays, _) = lift(MANIFEST);
        assert!(stays.contains("name = \"shop\""), "{stays}");
        assert!(
            stays.contains("tokio = { version = \"1\", features = [\"macros\"] }"),
            "{stays}"
        );
        assert!(stays.ends_with('\n'), "a manifest ends with a newline");
    }

    #[test]
    fn the_detach_stanza_and_its_comment_are_removed() {
        let (stays, _) = lift(MANIFEST);
        assert!(!stays.contains("[workspace]"), "{stays}");
        assert!(
            !stays.contains("sits inside another Cargo workspace"),
            "{stays}"
        );
        assert!(stays.contains("[dependencies]"), "{stays}");
    }

    #[test]
    fn a_manifest_with_no_profiles_is_returned_unchanged() {
        let plain = "[package]\nname = \"shop\"\n\n[dependencies]\nmoso = \"0.1\"\n";
        let (stays, profiles) = lift(plain);
        assert_eq!(stays, plain);
        assert!(profiles.is_empty());
    }

    // ── re-rooting ────────────────────────────────────────────────────────

    #[test]
    fn a_relative_path_dependency_follows_the_manifest_down() {
        let rewritten = reroot("moso = { path = \"../moso/crates/moso\" }", 2);
        assert_eq!(rewritten, "moso = { path = \"../../../moso/crates/moso\" }");
    }

    #[test]
    fn an_absolute_path_is_left_alone() {
        for line in [
            "moso = { path = \"/opt/moso/crates/moso\" }",
            "moso = { path = \"C:\\\\opt\\\\moso\" }",
        ] {
            assert_eq!(reroot(line, 2), line, "{line}");
        }
    }

    #[test]
    fn spacing_and_the_rest_of_the_line_are_preserved() {
        assert_eq!(
            reroot("moso={path=\"../moso\"}", 2),
            "moso={path=\"../../../moso\"}"
        );
        assert_eq!(
            reroot("a = { path = \"x\", version = \"1\" }", 1),
            "a = { path = \"../x\", version = \"1\" }"
        );
    }

    #[test]
    fn two_path_dependencies_on_one_line_are_both_rewritten() {
        assert_eq!(
            reroot("a = { path = \"x\" }  # b = { path = \"y\" }", 1),
            "a = { path = \"../x\" }  # b = { path = \"../y\" }"
        );
    }

    /// `lines()` drops the final terminator, and a manifest that stops without
    /// one makes every later diff open with `\ No newline at end of file`.
    #[test]
    fn a_rewritten_manifest_still_ends_with_a_newline() {
        let (stays, _) = lift(MANIFEST);
        let rewritten = reroot(&stays, HOPS);
        assert!(rewritten.ends_with('\n'), "{rewritten:?}");
        assert!(!rewritten.ends_with("\n\n"), "and only one: {rewritten:?}");
        assert_eq!(reroot("no trailing newline", 2), "no trailing newline");
    }

    /// A word that merely contains `path` is not a `path` key, and a rewrite
    /// that assumed otherwise would corrupt the manifest.
    #[test]
    fn a_key_that_only_looks_like_path_is_untouched() {
        for line in [
            "search_paths = [\"a\"]",
            "description = \"the path of least resistance\"",
            "[package]",
        ] {
            assert_eq!(reroot(line, 2), line, "{line}");
        }
    }

    // ── the preconditions ─────────────────────────────────────────────────

    #[test]
    fn an_empty_workspace_table_is_the_detach_stanza_and_not_a_split() {
        let manifest: toml::Value =
            toml::from_str("[package]\nname=\"shop\"\n[workspace]\n").expect("toml");
        assert!(already_a_workspace(&manifest).is_ok());
    }

    #[test]
    fn a_workspace_with_members_is_refused_rather_than_overwritten() {
        let manifest: toml::Value =
            toml::from_str("[workspace]\nmembers = [\"crates/*\"]\n").expect("toml");
        let error = already_a_workspace(&manifest).expect_err("refused");
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert!(error.help.is_some_and(|help| help.contains("cargo new")));
    }

    #[test]
    fn a_workspace_that_only_declares_shared_dependencies_is_refused_too() {
        let manifest: toml::Value =
            toml::from_str("[workspace.dependencies]\nmoso = \"0.1\"\n").expect("toml");
        assert!(already_a_workspace(&manifest).is_err());
    }

    // ── the plan, end to end, on a real directory ─────────────────────────

    /// A scratch directory that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default();
            let path =
                std::env::temp_dir().join(format!("moso-ws-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).expect("scratch directory");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn project_at(root: &Path) -> Project {
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(root.join("Cargo.toml"), MANIFEST).expect("manifest");
        std::fs::write(root.join("src/lib.rs"), "//! shop\n").expect("lib");
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("main");
        std::fs::write(root.join("README.md"), "# shop\n").expect("readme");
        std::fs::write(root.join(".env"), "SHOP__GREETING=hei\n").expect("env");
        Project {
            manifest_path: root.join("Cargo.toml"),
            root: root.to_path_buf(),
            name: "shop".to_owned(),
            rust_version: None,
            uses_moso: true,
        }
    }

    #[test]
    fn the_package_moves_and_the_project_files_stay() {
        let scratch = Scratch::new("split");
        let project = project_at(&scratch.0);
        let plan = plan(&project, true).expect("planned");
        apply(&plan).expect("applied");

        assert!(scratch.0.join("crates/shop/Cargo.toml").is_file());
        assert!(scratch.0.join("crates/shop/src/lib.rs").is_file());
        assert!(
            scratch.0.join("README.md").is_file(),
            "a project file stays"
        );
        assert!(scratch.0.join(".env").is_file(), "so does .env");
        assert!(!scratch.0.join("src").exists(), "src moved");

        let root = std::fs::read_to_string(scratch.0.join("Cargo.toml")).expect("root manifest");
        assert!(root.contains("[workspace]"), "{root}");
        assert!(root.contains("members = [\"crates/*\"]"), "{root}");
        assert!(root.contains("[profile.dev.package.\"*\"]"), "{root}");
        toml::from_str::<toml::Value>(&root).expect("the root manifest is valid TOML");

        let moved =
            std::fs::read_to_string(scratch.0.join("crates/shop/Cargo.toml")).expect("manifest");
        assert!(
            moved.contains("path = \"../../../moso/crates/moso\""),
            "{moved}"
        );
        assert!(!moved.contains("[profile"), "{moved}");
        assert!(!moved.contains("[workspace]"), "{moved}");
        toml::from_str::<toml::Value>(&moved).expect("the package manifest is valid TOML");
    }

    #[test]
    fn a_second_run_refuses_rather_than_nesting_the_split() {
        let scratch = Scratch::new("twice");
        let project = project_at(&scratch.0);
        apply(&plan(&project, true).expect("planned")).expect("applied");

        // The manifest that is left at the root is the workspace one, which is
        // what the second run reads — and refuses.
        let again = Project {
            manifest_path: scratch.0.join("Cargo.toml"),
            root: scratch.0.clone(),
            name: "shop".to_owned(),
            rust_version: None,
            uses_moso: true,
        };
        let error = plan(&again, true).expect_err("refused");
        assert_eq!(error.fault, crate::exit::Fault::User);
    }

    /// Regression: discovery resolves a single-member workspace root to that
    /// member, which every other command wants and this one must not have —
    /// it would split `crates/shop` into `crates/shop/crates/shop`. The
    /// already-split check therefore runs before discovery, not after it, and
    /// this asserts it fires on the tree a first run leaves behind.
    #[test]
    fn a_second_run_is_refused_before_the_package_is_even_found() {
        let scratch = Scratch::new("twice-discovered");
        let project = project_at(&scratch.0);
        apply(&plan(&project, true).expect("planned")).expect("applied");

        let error = already_split_at(&scratch.0).expect("refuses a project already split");
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert!(
            error.message.contains("already a workspace root"),
            "{}",
            error.message
        );
        assert!(
            error.help.is_some_and(|help| help.contains("crates/*")),
            "the help names the glob that picks up the next crate"
        );

        // And the package below it is still perfectly discoverable, which is
        // what keeps `moso routes` working from the split root.
        assert!(
            crate::project::Project::discover(Some(&scratch.0.join("crates/shop/Cargo.toml")))
                .is_ok()
        );
    }

    #[test]
    fn an_occupied_crates_directory_stops_the_command_before_anything_moves() {
        let scratch = Scratch::new("occupied");
        let project = project_at(&scratch.0);
        std::fs::create_dir_all(scratch.0.join("crates")).expect("crates");

        let error = plan(&project, true).expect_err("refused");
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert!(scratch.0.join("src/lib.rs").is_file(), "nothing moved");
    }

    #[test]
    fn a_dry_run_changes_nothing() {
        let scratch = Scratch::new("dry");
        let project = project_at(&scratch.0);
        let plan = plan(&project, true).expect("planned");
        preview(&Ui::silent(), &plan).expect("previewed");

        assert!(scratch.0.join("src/lib.rs").is_file());
        assert!(!scratch.0.join("crates").exists());
    }

    #[test]
    fn the_dockerfile_and_a_root_relative_manifest_key_are_reported() {
        let scratch = Scratch::new("warn");
        let project = project_at(&scratch.0);
        std::fs::write(scratch.0.join("Dockerfile"), "FROM rust\n").expect("dockerfile");
        std::fs::write(
            scratch.0.join("Cargo.toml"),
            "[package]\nname = \"shop\"\nreadme = \"README.md\"\n",
        )
        .expect("manifest");

        let plan = plan(&project, true).expect("planned");
        assert_eq!(plan.warnings.len(), 2, "{:?}", plan.warnings);
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("readme"))
        );
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("Dockerfile"))
        );
    }
}
