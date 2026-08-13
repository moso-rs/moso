//! `moso doctor` — is this machine able to build and run a Moso project?
//!
//! `40-cli.md`: "doctor is the first thing support asks a user to run, so it
//! must be thorough and its `--fix` suggestions must actually work." Every
//! check here therefore ends in a command that can be pasted, and no check
//! reports a problem it cannot explain.
//!
//! Only two conditions exit non-zero: no toolchain, and a toolchain older than
//! the project's MSRV. A missing fast linker is worth telling someone about and
//! is not a reason for a CI job to fail.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::cli::DoctorArgs;
use crate::exit::{CliError, Outcome};
use crate::project::Project;
use crate::ui::{Level, Ui};

/// How long `target/` may be walked before the answer is reported as a floor.
const SIZE_BUDGET: Duration = Duration::from_millis(1500);

/// The MSRV assumed when the project does not declare one.
const DEFAULT_MSRV: &str = "1.90";

/// One thing that was looked at.
#[derive(Debug, Clone)]
pub struct Check {
    /// The left column: what was checked.
    pub name: String,
    /// How it went.
    pub level: Level,
    /// The right column: what was found.
    pub detail: String,
    /// A command that fixes it.
    pub fix: Option<String>,
}

impl Check {
    /// A passing check.
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: Level::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    /// Something worth knowing that does not block anything.
    fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: Level::Warn,
            detail: detail.into(),
            fix: None,
        }
    }

    /// Something that stops the project from building.
    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: Level::Fail,
            detail: detail.into(),
            fix: None,
        }
    }

    /// A row with no verdict attached.
    fn info(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: Level::Info,
            detail: detail.into(),
            fix: None,
        }
    }

    /// Attach the command that resolves it.
    #[must_use]
    fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    /// The `--json` rendering.
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "level": self.level.as_str(),
            "detail": self.detail,
            "fix": self.fix,
        })
    }
}

/// Run `moso doctor`.
///
/// # Errors
/// [`Fault::Environment`](crate::exit::Fault::Environment) when a check failed.
/// Never fails for a warning.
pub fn run(ui: &Ui, args: &DoctorArgs) -> Outcome<()> {
    // A project is optional: `moso doctor` on a bare machine is a legitimate
    // thing to run before creating anything.
    let project = Project::discover(args.manifest_path.as_deref()).ok();

    let mut checks = Vec::new();
    let rustc = toolchain(&mut checks, project.as_ref());
    checks.push(cargo_check());
    checks.extend(linker(rustc.as_deref(), project.as_ref()));
    checks.push(nextest());
    if let Some(project) = &project {
        checks.push(project_check(project));
        checks.extend(cargo_config(project));
        checks.extend(disk(project));
        checks.extend(dotenv(project));
    } else {
        checks.push(
            Check::info("project", "none found from here")
                .with_fix("moso new <name>, or cd into a Moso project"),
        );
    }

    let failed = checks
        .iter()
        .filter(|check| check.level == Level::Fail)
        .count();

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": failed == 0,
            "checks": checks.iter().map(Check::to_json).collect::<Vec<_>>(),
            "failed": failed,
        }));
    } else {
        ui.blank();
        for check in &checks {
            ui.status(check.level, &check.name, &check.detail);
            if let Some(fix) = &check.fix {
                ui.fix(fix);
            }
        }
        ui.blank();
    }

    if failed == 0 {
        return Ok(());
    }
    Err(CliError::environment(format!("{failed} checks failed"))
        .with_help("apply the fixes above, then run `moso doctor` again"))
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

/// `rustc`, and whether it satisfies the project's MSRV.
///
/// Returns the host triple, which the linker check needs.
fn toolchain(checks: &mut Vec<Check>, project: Option<&Project>) -> Option<String> {
    let Some(verbose) = capture("rustc", &["-vV"]) else {
        checks.push(
            Check::fail("rustc", "not on PATH").with_fix("curl https://sh.rustup.rs -sSf | sh"),
        );
        return None;
    };

    let version = field(&verbose, "release:").unwrap_or_else(|| "unknown".to_owned());
    let host = field(&verbose, "host:");
    let msrv = project
        .and_then(|project| project.rust_version.clone())
        .unwrap_or_else(|| DEFAULT_MSRV.to_owned());

    match (parse_version(&version), parse_version(&msrv)) {
        (Some(have), Some(want)) if have < want => checks.push(
            Check::fail("rustc", format!("{version} (MSRV {msrv} not satisfied)"))
                .with_fix("rustup update stable"),
        ),
        (Some(_), Some(_)) => checks.push(Check::ok(
            "rustc",
            format!("{version} (MSRV {msrv} satisfied)"),
        )),
        _ => checks.push(Check::warn(
            "rustc",
            format!("{version} (could not compare against MSRV {msrv})"),
        )),
    }

    host
}

/// `cargo`.
fn cargo_check() -> Check {
    match capture("cargo", &["--version"]) {
        Some(text) => Check::ok("cargo", first_line(&text)),
        None => Check::fail("cargo", "not on PATH").with_fix("rustup component add cargo"),
    }
}

/// Whether a fast linker is available, and whether it is actually configured.
fn linker(host: Option<&str>, project: Option<&Project>) -> Vec<Check> {
    let mut checks = Vec::new();

    let available: Vec<&str> = ["mold", "sold", "lld", "ld64.lld", "zld", "wild"]
        .into_iter()
        .filter(|linker| which(linker).is_some())
        .collect();

    let bundled = rust_lld(host);
    let configured = project.and_then(|project| configured_linker(&project.root));

    match (&configured, available.is_empty()) {
        (Some(what), _) => checks.push(Check::ok("linker", format!("configured: {what}"))),
        (None, false) => checks.push(
            Check::warn(
                "linker",
                format!(
                    "using the default; {} is installed but not configured",
                    available.join(", ")
                ),
            )
            .with_fix(format!(
                "uncomment the {} stanza in .cargo/config.toml",
                host.unwrap_or("target")
            )),
        ),
        (None, true) => checks.push(
            Check::warn(
                "linker",
                "using the default; a faster one would save link time",
            )
            .with_fix(install_hint()),
        ),
    }

    if bundled {
        checks.push(Check::info(
            "rust-lld",
            "shipped with this toolchain (-C link-arg=-fuse-ld=lld)",
        ));
    }

    checks
}

/// `cargo-nextest`, which `moso test` will wrap once it exists.
fn nextest() -> Check {
    match capture("cargo", &["nextest", "--version"]) {
        Some(text) => Check::ok("cargo-nextest", first_line(&text)),
        None => Check::info("cargo-nextest", "not installed (cargo test still works)")
            .with_fix("cargo install cargo-nextest --locked"),
    }
}

/// Whether the discovered package actually is a Moso project.
fn project_check(project: &Project) -> Check {
    if project.uses_moso {
        Check::ok(
            "project",
            format!("{} ({})", project.name, project.manifest_path.display()),
        )
    } else {
        Check::warn(
            "project",
            format!("{} does not depend on moso", project.name),
        )
        .with_fix("cargo add moso")
    }
}

/// Whether `.cargo/config.toml` exists and parses.
fn cargo_config(project: &Project) -> Vec<Check> {
    let path = project.root.join(".cargo/config.toml");
    if !path.is_file() {
        return vec![
            Check::info(".cargo/config.toml", "absent (cargo defaults apply)")
                .with_fix("moso new writes one; copy it from a fresh project"),
        ];
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return vec![Check::warn(".cargo/config.toml", "present but unreadable")];
    };
    match toml::from_str::<toml::Value>(&text) {
        Ok(_) => vec![Check::ok(".cargo/config.toml", "present and valid")],
        Err(error) => vec![
            Check::fail(".cargo/config.toml", format!("does not parse: {error}"))
                .with_fix(format!("fix the TOML in {}", path.display())),
        ],
    }
}

/// Free space, and how much of it `target/` is using.
fn disk(project: &Project) -> Vec<Check> {
    let mut checks = Vec::new();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| project.root.join("target"), PathBuf::from);

    let free = free_space(&project.root);
    let (used, complete) = if target.is_dir() {
        directory_size(&target, Instant::now() + SIZE_BUDGET)
    } else {
        (0, true)
    };

    let target_text = if !target.is_dir() {
        "not built yet".to_owned()
    } else if complete {
        format!("target/ is {}", human_bytes(used))
    } else {
        format!("target/ is at least {}", human_bytes(used))
    };

    match free {
        // Low disk is advisory, not a toolchain failure: a working toolchain on
        // a nearly-full volume must not make `moso doctor` exit non-zero (it is
        // why CI, whose runners fill their disk building `target/`, saw doctor
        // report `ok: false`). It is a `warn`, like the large-`target/` case.
        Some(free) if free < 2 * 1024 * 1024 * 1024 => checks.push(
            Check::warn(
                "disk",
                format!("{} free — {target_text}", human_bytes(free)),
            )
            .with_fix("cargo clean, or free space on this volume"),
        ),
        Some(free) if used > 5 * 1024 * 1024 * 1024 => checks.push(
            Check::warn(
                "disk",
                format!("{} free — {target_text}", human_bytes(free)),
            )
            .with_fix("cargo clean to reclaim it"),
        ),
        Some(free) => checks.push(Check::ok(
            "disk",
            format!("{} free — {target_text}", human_bytes(free)),
        )),
        None => checks.push(Check::info("disk", target_text)),
    }

    checks
}

/// Keys `.env.example` declares that `.env` does not supply.
fn dotenv(project: &Project) -> Vec<Check> {
    let example_path = project.root.join(".env.example");
    if !example_path.is_file() {
        return Vec::new();
    }
    let Ok(example) = std::fs::read_to_string(&example_path) else {
        return vec![Check::warn(".env.example", "present but unreadable")];
    };

    let env_path = project.root.join(".env");
    let env = std::fs::read_to_string(&env_path).unwrap_or_default();
    if !env_path.is_file() {
        let required = required_keys(&example);
        if required.is_empty() {
            return vec![Check::info(".env", "absent (every key has a default)")];
        }
        return vec![
            Check::warn(
                ".env",
                format!("absent; {} keys have no default", required.len()),
            )
            .with_fix("cp .env.example .env"),
        ];
    }

    let missing = missing_keys(&example, &env);
    if missing.is_empty() {
        return vec![Check::ok(".env", "every key from .env.example is set")];
    }
    vec![
        Check::warn(".env", format!("missing {}", missing.join(", ")))
            .with_fix("add them to .env, or set them in the environment"),
    ]
}

// ---------------------------------------------------------------------------
// Helpers, all pure and all tested
// ---------------------------------------------------------------------------

/// Run a program and return its stdout, or `None` if it is not runnable.
pub(super) fn capture(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program).args(arguments).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The first line, trimmed.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

/// The value of a `key: value` line in `rustc -vV` output.
fn field(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .map(|value| value.trim().to_owned())
}

/// Parse `1.97.1`, `1.90` or `1.98.0-nightly` into a comparable triple.
pub(super) fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let core = text
        .trim()
        .split(['-', ' ', '(']) // strip `-nightly` and any trailing metadata
        .next()?;
    let mut parts = core.split('.').map(str::parse::<u64>);
    let major = parts.next()?.ok()?;
    let minor = parts.next().transpose().ok()?.unwrap_or(0);
    let patch = parts.next().transpose().ok()?.unwrap_or(0);
    Some((major, minor, patch))
}

/// Find a program on `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(program);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        let with_extension = directory.join(format!("{program}.exe"));
        if is_executable(&with_extension) {
            return Some(with_extension);
        }
    }
    None
}

/// Whether `path` is a file this process could execute.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Whether `path` is a file this process could execute.
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Whether the active toolchain ships `rust-lld`.
fn rust_lld(host: Option<&str>) -> bool {
    let (Some(sysroot), Some(host)) = (capture("rustc", &["--print", "sysroot"]), host) else {
        return false;
    };
    Path::new(sysroot.trim())
        .join("lib/rustlib")
        .join(host)
        .join("bin/rust-lld")
        .exists()
}

/// The platform-appropriate way to install a fast linker.
fn install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew install llvm, then uncomment the macOS stanza in .cargo/config.toml"
    } else if cfg!(target_os = "windows") {
        "rust-lld is already the default on the MSVC toolchain"
    } else {
        "apt install mold (or dnf install mold), then uncomment the Linux stanza in \
         .cargo/config.toml"
    }
}

/// Whether `.cargo/config.toml` actually selects a linker, and which.
///
/// Both spellings count: `linker = ".."` and a `rustflags` entry carrying
/// `-fuse-ld=..`. A commented-out stanza does not count, which is the point —
/// the template ships with every stanza commented out, and reporting that as
/// "configured" would make the check useless.
pub fn configured_linker(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(".cargo/config.toml")).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    let targets = value.get("target")?.as_table()?;

    for (triple, settings) in targets {
        if let Some(linker) = settings.get("linker").and_then(toml::Value::as_str) {
            return Some(format!("{linker} for {triple}"));
        }
        let flags = settings.get("rustflags").and_then(toml::Value::as_array);
        if let Some(flags) = flags
            && let Some(found) = flags
                .iter()
                .filter_map(toml::Value::as_str)
                .find_map(|flag| flag.split("-fuse-ld=").nth(1))
        {
            return Some(format!("{found} for {triple}"));
        }
    }
    None
}

/// Free bytes on the volume holding `path`, via `df`.
///
/// Shelling out because the standard library has no answer and the alternative
/// is a `libc` dependency plus `unsafe`, which this crate forbids.
fn free_space(path: &Path) -> Option<u64> {
    if cfg!(windows) {
        return None;
    }
    let output = Command::new("df")
        .arg("-Pk")
        .arg(path)
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    parse_df(&String::from_utf8_lossy(&output.stdout))
}

/// Pull the "available" column out of `df -Pk` output, in bytes.
fn parse_df(text: &str) -> Option<u64> {
    let row = text.lines().nth(1)?;
    let kibibytes: u64 = row.split_whitespace().nth(3)?.parse().ok()?;
    Some(kibibytes * 1024)
}

/// Total size of a directory tree, giving up after `deadline`.
///
/// A cold `target/` has hundreds of thousands of files; a doctor that takes
/// nine seconds to tell you about disk space is a doctor nobody runs. The bool
/// says whether the number is exact or a floor.
fn directory_size(root: &Path, deadline: Instant) -> (u64, bool) {
    let mut total = 0_u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        if Instant::now() >= deadline {
            return (total, false);
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file()
                && let Ok(meta) = entry.metadata()
            {
                total += meta.len();
            }
        }
    }
    (total, true)
}

/// Render a byte count the way a person reads one.
pub(super) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The `KEY=value` pairs of a dotenv-shaped file, comments ignored.
fn env_keys(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| {
            (
                key.trim().trim_start_matches("export ").trim().to_owned(),
                value.trim().to_owned(),
            )
        })
        .collect()
}

/// Keys in `.env.example` that carry no default, and so must be supplied.
fn required_keys(example: &str) -> Vec<String> {
    env_keys(example)
        .into_iter()
        .filter(|(_, value)| value.is_empty())
        .map(|(key, _)| key)
        .collect()
}

/// Required keys the `.env` does not set.
fn missing_keys(example: &str, env: &str) -> Vec<String> {
    let set: Vec<String> = env_keys(env).into_iter().map(|(key, _)| key).collect();
    required_keys(example)
        .into_iter()
        .filter(|key| !set.contains(key))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(parse_version("1.97.1"), Some((1, 97, 1)));
        assert_eq!(parse_version("1.90"), Some((1, 90, 0)));
        assert_eq!(parse_version("1.98.0-nightly"), Some((1, 98, 0)));
        assert_eq!(parse_version("  1.97.1  "), Some((1, 97, 1)));
        assert_eq!(parse_version("stable"), None);
        assert!(parse_version("1.89.0") < parse_version("1.90"));
    }

    #[test]
    fn rustc_vv_fields_are_read() {
        let output = "rustc 1.97.1 (abc 2026-01-01)\nbinary: rustc\nrelease: 1.97.1\n\
                      host: aarch64-apple-darwin\n";
        assert_eq!(field(output, "release:").as_deref(), Some("1.97.1"));
        assert_eq!(
            field(output, "host:").as_deref(),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(field(output, "nope:"), None);
        assert_eq!(first_line(output), "rustc 1.97.1 (abc 2026-01-01)");
    }

    #[test]
    fn df_output_yields_bytes() {
        let output = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                      /dev/disk3s5 971350180 500000000 400000000 56% /\n";
        assert_eq!(parse_df(output), Some(400_000_000 * 1024));
        assert_eq!(parse_df("Filesystem only\n"), None);
        assert_eq!(parse_df(""), None);
    }

    #[test]
    fn bytes_render_the_way_people_read_them() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(3_221_225_472), "3.0 GB");
    }

    #[test]
    fn dotenv_keys_ignore_comments_and_exports() {
        let text = "# a comment\n\nSHOP__A=1\nexport SHOP__B=2\nSHOP__C=\n";
        let keys = env_keys(text);
        assert_eq!(
            keys,
            vec![
                ("SHOP__A".to_owned(), "1".to_owned()),
                ("SHOP__B".to_owned(), "2".to_owned()),
                ("SHOP__C".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn only_keys_without_a_default_are_required() {
        let example = "SHOP__GREETING=hello\nSHOP__SECRET_KEY=\n";
        assert_eq!(required_keys(example), vec!["SHOP__SECRET_KEY".to_owned()]);
    }

    #[test]
    fn a_supplied_key_is_not_reported_missing() {
        let example = "SHOP__GREETING=hello\nSHOP__SECRET_KEY=\n";
        assert_eq!(missing_keys(example, ""), vec!["SHOP__SECRET_KEY"]);
        assert!(missing_keys(example, "SHOP__SECRET_KEY=abc\n").is_empty());
        // A key with a default is never "missing", however absent it is.
        assert!(!missing_keys(example, "").contains(&"SHOP__GREETING".to_owned()));
    }

    #[test]
    fn a_commented_out_linker_stanza_does_not_count_as_configured() {
        let scratch = std::env::temp_dir().join(format!("moso-doctor-{}", std::process::id()));
        let cargo = scratch.join(".cargo");
        std::fs::create_dir_all(&cargo).expect("scratch");

        let template = crate::template::FILES
            .iter()
            .find(|file| file.path == ".cargo/config.toml")
            .expect("the template ships one");
        std::fs::write(cargo.join("config.toml"), template.contents).expect("write");
        assert_eq!(configured_linker(&scratch), None);

        std::fs::write(
            cargo.join("config.toml"),
            "[target.aarch64-apple-darwin]\nrustflags = [\"-C\", \"link-arg=-fuse-ld=lld\"]\n",
        )
        .expect("write");
        assert_eq!(
            configured_linker(&scratch).as_deref(),
            Some("lld for aarch64-apple-darwin")
        );

        std::fs::write(
            cargo.join("config.toml"),
            "[target.x86_64-unknown-linux-gnu]\nlinker = \"clang\"\n",
        )
        .expect("write");
        assert_eq!(
            configured_linker(&scratch).as_deref(),
            Some("clang for x86_64-unknown-linux-gnu")
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_directory_size_is_the_sum_of_its_files() {
        let scratch = std::env::temp_dir().join(format!("moso-size-{}", std::process::id()));
        std::fs::create_dir_all(scratch.join("nested")).expect("scratch");
        std::fs::write(scratch.join("a"), vec![0_u8; 100]).expect("write");
        std::fs::write(scratch.join("nested/b"), vec![0_u8; 200]).expect("write");

        let (total, complete) = directory_size(&scratch, Instant::now() + SIZE_BUDGET);
        assert_eq!(total, 300);
        assert!(complete);

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_size_walk_that_runs_out_of_time_says_so() {
        let scratch = std::env::temp_dir().join(format!("moso-budget-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).expect("scratch");
        let (_, complete) = directory_size(&scratch, Instant::now() - Duration::from_secs(1));
        assert!(!complete, "an expired budget must report a floor");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn which_finds_something_that_is_certainly_on_path() {
        // `cargo` is running this test, so it is on PATH by construction.
        assert!(which("cargo").is_some() || std::env::var_os("PATH").is_none());
        assert!(which("definitely-not-a-real-program-4f2a").is_none());
    }

    #[test]
    fn a_check_renders_its_level_and_fix() {
        let check = Check::fail("rustc", "not on PATH").with_fix("rustup update");
        let json = check.to_json();
        assert_eq!(json["level"], serde_json::json!("fail"));
        assert_eq!(json["fix"], serde_json::json!("rustup update"));
        assert_eq!(
            Check::ok("cargo", "1.97.1").to_json()["fix"],
            serde_json::Value::Null
        );
    }
}
