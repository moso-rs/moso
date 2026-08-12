//! `release` — the four mechanical steps of cutting a version.
//!
//! `docs/00-foundations/03-crate-layout.md`: *"All Moso crates version in
//! lockstep and carry `=x.y.z` path+version pins on each other, declared once in
//! `[workspace.dependencies]`."* Lockstep versioning is easy to describe and
//! easy to get wrong by hand — one pin left at the old version and the published
//! crate cannot resolve. So the bump is a command,
//! and the publish order is computed from the dependency graph rather than
//! remembered.
//!
//! # What this will not do
//!
//! It will not publish. `xtask release publish` runs `cargo publish --dry-run`
//! for every crate in dependency order and then prints the real commands for a
//! human to run. An `--execute` flag that uploads eight crates to a permanent,
//! irrevocable registry from inside a build tool is not a convenience; it is a
//! way to publish `0.1.0` twice.
//!
//! Nothing writes to a file without `--write`, and nothing tags without it
//! either. The default output of every subcommand is what it *would* do.

use crate::bail;
use crate::meta::Workspace;
use crate::util::{Cmd, Error, Result, ui};

/// The steps of a release, in order.
///
/// ```
/// use xtask::release::Step;
///
/// assert_eq!(Step::ALL.len(), 3);
/// assert_eq!(Step::Bump.id(), "bump");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Rewrite the workspace version and every intra-workspace pin.
    Bump,
    /// `cargo publish --dry-run` for every crate, in dependency order.
    Publish,
    /// Create the annotated git tag.
    Tag,
}

impl Step {
    /// Every step, in the order a release performs them.
    ///
    /// ```
    /// use xtask::release::Step;
    ///
    /// assert_eq!(Step::ALL[2], Step::Tag);
    /// ```
    pub const ALL: [Step; 3] = [Self::Bump, Self::Publish, Self::Tag];

    /// The step's name on the command line.
    ///
    /// ```
    /// assert_eq!(xtask::release::Step::Bump.id(), "bump");
    /// ```
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Bump => "bump",
            Self::Publish => "publish",
            Self::Tag => "tag",
        }
    }
}

/// Options for one release step.
///
/// ```
/// use xtask::release::Options;
///
/// let options = Options { version: "0.2.0".into(), write: false, ..Options::default() };
/// assert_eq!(options.version, "0.2.0");
/// assert!(!options.write);
/// ```
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The version being released.
    pub version: String,
    /// Actually change files and create the tag. Off by default.
    pub write: bool,
}

/// Prints everything a release of `version` would do, and does nothing.
///
/// ```no_run
/// let options = xtask::release::Options { version: "0.2.0".into(), ..Default::default() };
/// xtask::release::plan(&options)?;
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn plan(options: &Options) -> Result<()> {
    let root = crate::util::workspace_root()?;
    let workspace = Workspace::load()?;
    let version = validated_version(&options.version)?;
    let current = workspace
        .package("moso")
        .map(|package| package.version.clone())
        .unwrap_or_default();

    ui::headline(&format!("release {version} (plan only)"));
    ui::note(&format!("current workspace version: {current}"));

    let manifest = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)?;
    let (_, changes) = bump_manifest(&text, &current, version)?;
    if changes.is_empty() {
        ui::warn("Cargo.toml has nothing to bump — is the version already set?");
    }
    for change in &changes {
        ui::note(&format!("Cargo.toml  {change}"));
    }

    let order = workspace.publish_order()?;
    let publishable: Vec<&str> = order
        .iter()
        .filter(|package| package.publishable)
        .map(|package| package.name.as_str())
        .collect();
    ui::note(&format!("publish order: {}", publishable.join(" -> ")));
    let skipped: Vec<&str> = order
        .iter()
        .filter(|package| !package.publishable)
        .map(|package| package.name.as_str())
        .collect();
    if !skipped.is_empty() {
        ui::note(&format!("not published: {}", skipped.join(", ")));
    }
    ui::note(&format!("tag: v{version}"));
    Ok(())
}

/// Rewrites the workspace version and every `=x.y.z` intra-workspace pin.
///
/// ```no_run
/// let options = xtask::release::Options { version: "0.2.0".into(), write: true,
///     ..Default::default() };
/// xtask::release::bump(&options)?;
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn bump(options: &Options) -> Result<()> {
    let root = crate::util::workspace_root()?;
    let workspace = Workspace::load()?;
    let version = validated_version(&options.version)?;
    let current = workspace
        .package("moso")
        .map(|package| package.version.clone())
        .ok_or_else(|| Error::new("the facade is not in the workspace; nothing to version"))?;

    let manifest = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)?;
    let (rewritten, changes) = bump_manifest(&text, &current, version)?;

    ui::headline(&format!("release bump {current} -> {version}"));
    for change in &changes {
        ui::note(change);
    }
    if !options.write {
        ui::warn("nothing written; pass --write to apply");
        return Ok(());
    }
    if changes.is_empty() {
        bail!("nothing to bump: no `version` or `=` pin matched {current}");
    }
    std::fs::write(&manifest, rewritten)?;
    ui::ok(&format!("{} rewritten", manifest.display()));
    ui::note("run `cargo check --workspace` next, so Cargo.lock records the new version");
    Ok(())
}

/// Rewrites the workspace manifest for a new version.
///
/// Two kinds of line change: the `version` key under `[workspace.package]`, and
/// every `=x.y.z` requirement in `[workspace.dependencies]`. Everything else —
/// comments, ordering, formatting — is left byte-identical, because a release
/// commit whose diff is the whole manifest is a release commit nobody reviews.
///
/// ```
/// use xtask::release::bump_manifest;
///
/// // Written with escapes rather than as a block, because a line starting with
/// // `#` inside a doc comment is a hidden doctest line, not text.
/// let manifest = concat!(
///     "[workspace.package]\n",
///     "version = \"0.1.0\"\n\n",
///     "[workspace.dependencies]\n",
///     "# the rationale lives in the comment above each entry\n",
///     "moso-core = { version = \"=0.1.0\", path = \"crates/moso-core\" }\n",
///     "axum = \"0.8.9\"\n",
/// );
/// let (rewritten, changes) = bump_manifest(manifest, "0.1.0", "0.2.0")?;
/// assert!(rewritten.contains("version = \"0.2.0\""));
/// assert!(rewritten.contains("\"=0.2.0\""));
/// assert!(rewritten.contains("axum = \"0.8.9\""), "third-party pins are untouched");
/// assert!(rewritten.contains("rationale lives in"), "comments survive");
/// assert_eq!(changes.len(), 2);
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn bump_manifest(text: &str, from: &str, to: &str) -> Result<(String, Vec<String>)> {
    let mut table = String::new();
    let mut out: Vec<String> = Vec::new();
    let mut changes: Vec<String> = Vec::new();

    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            table = trimmed.to_owned();
        }
        let pin = format!("\"={from}\"");
        if line.contains(&pin) {
            let replaced = line.replace(&pin, &format!("\"={to}\""));
            changes.push(format!("line {}: pin {from} -> {to}", number + 1));
            out.push(replaced);
            continue;
        }
        if table == "[workspace.package]"
            && trimmed.starts_with("version")
            && trimmed.contains(&format!("\"{from}\""))
        {
            let replaced = line.replace(&format!("\"{from}\""), &format!("\"{to}\""));
            changes.push(format!(
                "line {}: [workspace.package] version {from} -> {to}",
                number + 1
            ));
            out.push(replaced);
            continue;
        }
        out.push(line.to_owned());
    }

    let mut rewritten = out.join("\n");
    if text.ends_with('\n') {
        rewritten.push('\n');
    }
    Ok((rewritten, changes))
}

/// Runs `cargo publish --dry-run` for every publishable crate, in dependency
/// order, and prints the real commands.
///
/// ```no_run
/// let options = xtask::release::Options { version: "0.2.0".into(), ..Default::default() };
/// let ok = xtask::release::publish_dry_run(&options)?;
/// assert!(ok);
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn publish_dry_run(options: &Options) -> Result<bool> {
    let root = crate::util::workspace_root()?;
    let workspace = Workspace::load()?;
    let order = workspace.publish_order()?;
    let publishable: Vec<&crate::meta::Package> = order
        .into_iter()
        .filter(|package| package.publishable)
        .collect();

    ui::headline("release publish (dry run)");
    let mut ok = true;

    let blockers = workspace.publish_blockers();
    for blocker in &blockers {
        ui::fail(blocker);
        ok = false;
    }

    for package in &publishable {
        // `--allow-dirty` because a release runs before the release commit
        // exists; `--locked` because a publish that resolves new dependencies is
        // publishing something nobody built.
        let cmd = Cmd::cargo().cwd(&root).args([
            "publish",
            "--dry-run",
            "--allow-dirty",
            "--locked",
            "--package",
            &package.name,
        ]);
        let output = cmd.capture()?;
        if output.ok() {
            ui::ok(&format!("{} {} packages", package.name, package.version));
            continue;
        }
        match unpublished_member(&output.stderr, &workspace) {
            Some(missing) => ui::warn(&format!(
                "{}: cannot be verified yet — it requires a version of `{missing}` that is not on \
                 the registry. Expected before the first release: the dry run passes once \
                 `{missing}` has actually been published",
                package.name
            )),
            None => {
                ui::fail(&format!("{}: {}", package.name, output.stderr_tail(8)));
                ok = false;
            }
        }
    }

    if ok {
        ui::note("");
        ui::note("to publish for real, in this order:");
        for package in &publishable {
            ui::note(&format!("  cargo publish --locked -p {}", package.name));
        }
        ui::note(&format!(
            "  git tag -a v{0} -m \"moso v{0}\" && git push --tags",
            options.version
        ));
    }
    Ok(ok)
}

/// The workspace member a failed `cargo publish --dry-run` could not resolve.
///
/// Before the first release, every crate except the first in the order fails
/// this way, and it is not a defect — it is what "not published yet" looks like.
/// Anything else is a real packaging failure and has to be reported as one, so
/// the two are told apart rather than lumped together.
///
/// ```
/// use xtask::meta::Workspace;
/// use xtask::release::unpublished_member;
///
/// let json = r#"{"packages":[{"name":"moso-core","version":"0.1.0",
///   "manifest_path":"/w/c/Cargo.toml","publish":null,
///   "targets":[{"name":"moso-core","kind":["lib"]}],"dependencies":[]}],
///   "workspace_root":"/w"}"#;
/// let workspace = Workspace::from_metadata_json(json, "/w".into())?;
///
/// let stderr = "error: failed to prepare local package for uploading\n\
///   Caused by:\n  failed to select a version for the requirement `moso-core = \"=0.1.0\"`\n\
///   candidate versions found which didn't match: 0.0.0\n";
/// assert_eq!(unpublished_member(stderr, &workspace).as_deref(), Some("moso-core"));
///
/// // A dependency that is not a workspace member is a genuine failure.
/// let other = "failed to select a version for the requirement `sqlx = \"0.9\"`";
/// assert_eq!(unpublished_member(other, &workspace), None);
/// assert_eq!(unpublished_member("error: some other problem", &workspace), None);
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[must_use]
pub fn unpublished_member(stderr: &str, workspace: &Workspace) -> Option<String> {
    let after = stderr
        .split("failed to select a version for the requirement `")
        .nth(1)?;
    let name = after.split_whitespace().next()?;
    workspace.package(name).map(|package| package.name.clone())
}

/// Creates the annotated tag for a version.
///
/// ```no_run
/// let options = xtask::release::Options { version: "0.2.0".into(), write: true,
///     ..Default::default() };
/// xtask::release::tag(&options)?;
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn tag(options: &Options) -> Result<()> {
    let root = crate::util::workspace_root()?;
    let version = validated_version(&options.version)?;
    let name = format!("v{version}");

    ui::headline(&format!("release tag {name}"));
    let existing = Cmd::new("git")
        .cwd(&root)
        .args(["tag", "--list", &name])
        .capture()?;
    if !existing.stdout.trim().is_empty() {
        bail!("the tag {name} already exists");
    }

    let workspace = Workspace::load()?;
    let declared = workspace
        .package("moso")
        .map(|package| package.version.clone())
        .unwrap_or_default();
    if declared != version {
        bail!(
            "the workspace is at {declared} and the tag would be {name}; run \
             `cargo xtask release bump --version {version} --write` first"
        );
    }

    let cmd = Cmd::new("git")
        .cwd(&root)
        .args(["tag", "-a", &name, "-m", &format!("moso {name}")]);
    if !options.write {
        ui::warn(&format!("nothing tagged; would run: {}", cmd.rendered()));
        return Ok(());
    }
    cmd.run()?;
    ui::ok(&format!("{name} created; `git push --tags` to publish it"));
    Ok(())
}

/// Rejects a version that is not `x.y.z` with an optional pre-release.
///
/// ```
/// use xtask::release::validated_version;
///
/// assert_eq!(validated_version("0.2.0")?, "0.2.0");
/// assert_eq!(validated_version("v1.0.0-rc.1")?, "1.0.0-rc.1");
/// assert!(validated_version("0.2").is_err());
/// assert!(validated_version("").is_err());
/// assert!(validated_version("one.two.three").is_err());
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn validated_version(version: &str) -> Result<&str> {
    let version = version.strip_prefix('v').unwrap_or(version);
    if version.is_empty() {
        bail!("--version is required, for example --version 0.2.0");
    }
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty() || !part.chars().all(|character| character.is_ascii_digit())
        })
    {
        bail!("`{version}` is not a semantic version; expected something like 0.2.0 or 1.0.0-rc.1");
    }
    Ok(version)
}

/// Runs one step.
///
/// ```no_run
/// use xtask::release::{Options, Step, run};
///
/// let options = Options { version: "0.2.0".into(), ..Default::default() };
/// let ok = run(Step::Publish, &options)?;
/// assert!(ok);
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn run(step: Step, options: &Options) -> Result<bool> {
    match step {
        Step::Bump => bump(options).map(|()| true),
        Step::Publish => publish_dry_run(options),
        Step::Tag => tag(options).map(|()| true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_outside_the_workspace_package_table_is_left_alone() {
        // A member's own `version = "0.1.0"` would be a mistake to rewrite: the
        // workspace inherits, and a literal there is either deliberate or a bug
        // for a human to see.
        let manifest = "\
[package]
version = \"0.1.0\"

[workspace.package]
version = \"0.1.0\"
";
        let (rewritten, changes) = bump_manifest(manifest, "0.1.0", "0.2.0").expect("valid");
        assert_eq!(changes.len(), 1);
        assert_eq!(
            rewritten,
            "[package]\nversion = \"0.1.0\"\n\n[workspace.package]\nversion = \"0.2.0\"\n"
        );
    }

    #[test]
    fn the_trailing_newline_is_preserved_either_way() {
        let (with, _) = bump_manifest(
            "[workspace.package]\nversion = \"1.0.0\"\n",
            "1.0.0",
            "1.1.0",
        )
        .expect("valid");
        assert!(with.ends_with('\n'));
        let (without, _) =
            bump_manifest("[workspace.package]\nversion = \"1.0.0\"", "1.0.0", "1.1.0")
                .expect("valid");
        assert!(!without.ends_with('\n'));
    }

    #[test]
    fn every_pin_moves_in_lockstep() {
        let manifest = "\
[workspace.package]
version = \"0.1.0\"

[workspace.dependencies]
moso = { version = \"=0.1.0\", path = \"crates/moso\" }
moso-core = { version = \"=0.1.0\", path = \"crates/moso-core\" }
moso-schema = { version = \"=0.1.0\", path = \"crates/moso-schema\" }
";
        let (rewritten, changes) = bump_manifest(manifest, "0.1.0", "0.1.1").expect("valid");
        assert_eq!(changes.len(), 4, "{changes:?}");
        assert_eq!(rewritten.matches("=0.1.1").count(), 3);
        assert!(!rewritten.contains("=0.1.0"));
    }

    #[test]
    fn versions_are_validated_before_anything_is_written() {
        for good in [
            "0.0.1",
            "1.2.3",
            "10.20.30",
            "1.0.0-rc.1",
            "v2.0.0",
            "1.0.0+build",
        ] {
            assert!(validated_version(good).is_ok(), "{good}");
        }
        for bad in ["", "1", "1.2", "1.2.3.4", "1.2.x", "-1.2.3", "1.2.3 "] {
            assert!(validated_version(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_real_manifest_can_be_bumped() {
        let root = crate::util::workspace_root().expect("a workspace");
        let text = std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml");
        let (rewritten, changes) = bump_manifest(&text, "0.1.0", "0.1.1").expect("valid");
        assert!(
            changes.len() >= 2,
            "the workspace version and at least one pin: {changes:?}"
        );
        assert!(rewritten.contains("version = \"0.1.1\""));
        // The rewrite must still be valid TOML.
        toml::from_str::<toml::Value>(&rewritten).expect("still parses");
    }
}
