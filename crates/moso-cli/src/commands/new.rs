//! `moso new` — scaffold a project.
//!
//! The generated project is plain Rust with comments explaining the choices, as
//! `40-cli.md` requires: nothing is hidden in a framework file, and everything
//! the `moso` CLI later relies on — the dump protocol, the environment prefix,
//! the composition root — is visible in the project's own source.
//!
//! What it is *not*, in this build: interactive. Of the questions `40-cli.md`
//! shows, two are answerable — `--with-db` and `--auth` — and the rest select
//! batteries that do not exist yet, so asking them would be theatre. Flags,
//! then, and one surviving prompt, which is the one that protects something:
//! overwriting a directory that already has files in it.
//!
//! `--auth` is the only flag that puts a secret on disk. It writes a `.env`
//! holding a session signing key taken from the operating system's random
//! number generator, because the key is required configuration with no default
//! and a generated project that will not boot is not a generated project.
//! `.gitignore` already excludes `.env`, so the `git add --all` below cannot
//! pick it up.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::NewArgs;
use crate::exit::{CliError, Outcome, io as io_error};
use crate::template::Vars;
use crate::ui::{Level, Ui};

/// What happened to the git repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Git {
    /// `--no-git`, or a repository was already there.
    Skipped,
    /// Initialised, but nothing committed.
    Initialised,
    /// Initialised, with a first commit.
    Committed,
}

/// Run `moso new`.
///
/// # Errors
/// A bad project name (1), a non-empty target directory the user declined to
/// overwrite (1), a prompt that cannot be shown because stdin is not a terminal
/// (2), or a filesystem that refused (3).
pub fn run(ui: &Ui, args: &NewArgs) -> Outcome<()> {
    let mut vars = Vars::for_name(&args.name)?;
    let target = args
        .path
        .clone()
        .unwrap_or_else(|| PathBuf::from(&vars.crate_name));

    ensure_writable(&target, args)?;

    if let Some(path) = &args.moso_path {
        vars = vars.with_moso_path(path);
    }
    if args.with_db {
        vars = vars.with_database();
    }
    if args.auth {
        // Thirty-two bytes, straight from the kernel, and never anything
        // weaker: the value signs the session cookie, and a scaffolder that
        // stretched a timestamp into a "key" would have put a forgeable session
        // in every project it ever created.
        let secret = crate::commands::secret::base64(&crate::commands::secret::random_bytes(32)?);
        vars = vars.with_auth(&secret);
    }
    if let Some(root) = enclosing_workspace(&target) {
        if ui.is_verbose() {
            ui.status(
                Level::Info,
                "inside a workspace",
                &root.display().to_string(),
            );
        }
        vars = vars.detached_from_workspace();
    }

    let files = vars.render_all();
    for (relative, contents) in &files {
        let path = target.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| io_error("could not create", parent, &error))?;
        }
        std::fs::write(&path, contents)
            .map_err(|error| io_error("could not write", &path, &error))?;
    }

    let git = if args.no_git {
        Git::Skipped
    } else {
        initialise_git(ui, &target)
    };

    report(ui, &target, &vars, &files, git);
    Ok(())
}

/// Refuse, or ask, before writing into a directory that already has files.
fn ensure_writable(target: &Path, args: &NewArgs) -> Outcome<()> {
    let occupied = std::fs::read_dir(target)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    if !occupied {
        return Ok(());
    }
    if args.force || args.yes {
        return Ok(());
    }

    let question = format!("`{}` is not empty. Write into it anyway?", target.display());
    if confirm(&question, false)? {
        Ok(())
    } else {
        Err(
            CliError::user(format!("`{}` already has files in it", target.display())).with_help(
                format!(
                    "choose another name, or run `moso new {} --force`",
                    args.name
                ),
            ),
        )
    }
}

/// Ask a yes/no question on the terminal.
///
/// # Errors
/// [`Fault::Usage`](crate::exit::Fault::Usage) when stdin is not a terminal:
/// a script that reaches a prompt has hit a bug in the script, and hanging or
/// silently guessing are both worse than saying so.
fn confirm(question: &str, default: bool) -> Outcome<bool> {
    if !std::io::stdin().is_terminal() {
        return Err(
            CliError::usage(format!("cannot ask `{question}`: stdin is not a terminal"))
                .with_help("pass --yes to accept, or --force to overwrite"),
        );
    }

    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "{question} {suffix} ");
    let _ = stderr.flush();

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return Ok(default);
    }
    Ok(match answer.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

/// The nearest ancestor manifest that declares a workspace, if any.
///
/// A project created inside someone else's workspace is claimed by it, and the
/// first thing the user sees is a cargo error about a member that is not
/// listed. Detecting it here costs one directory walk and turns that into a
/// three-line comment in the generated manifest.
fn enclosing_workspace(target: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(target).ok()?;
    for directory in absolute.ancestors().skip(1) {
        let manifest = directory.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        if toml::from_str::<toml::Value>(&text).is_ok_and(|value| value.get("workspace").is_some())
        {
            return Some(manifest);
        }
    }
    None
}

/// `git init`, `git add`, `git commit`, tolerating every failure.
///
/// A missing git, or a machine with no configured identity, must not make
/// `moso new` fail: the project on disk is already correct.
fn initialise_git(ui: &Ui, target: &Path) -> Git {
    if target.join(".git").exists() {
        return Git::Skipped;
    }
    if !git(target, &["init", "--quiet"]) {
        ui.warn("git is not available; skipped `git init`");
        return Git::Skipped;
    }
    if !git(target, &["add", "--all"]) {
        return Git::Initialised;
    }
    if git(
        target,
        &["commit", "--quiet", "--message", "Initial commit"],
    ) {
        Git::Committed
    } else {
        ui.warn("`git commit` failed; is user.name/user.email configured?");
        Git::Initialised
    }
}

/// Run one git subcommand, silently, and report whether it worked.
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

/// Print what was created.
fn report(ui: &Ui, target: &Path, vars: &Vars, files: &[(PathBuf, String)], git: Git) {
    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": true,
            "path": target.display().to_string(),
            "package": vars.crate_name,
            "env_prefix": vars.env_prefix,
            "db": vars.with_db,
            "auth": vars.with_auth,
            "files": files
                .iter()
                .map(|(path, _)| path.display().to_string())
                .collect::<Vec<_>>(),
            "git": match git {
                Git::Skipped => "skipped",
                Git::Initialised => "initialised",
                Git::Committed => "committed",
            },
        }));
        return;
    }

    ui.blank();
    ui.status(
        Level::Ok,
        &format!("created {}/", target.display()),
        &format!("({} files)", files.len()),
    );
    ui.status(
        Level::Ok,
        "wrote .cargo/config.toml",
        "(build settings; `moso doctor` explains them)",
    );
    ui.status(
        Level::Ok,
        "wrote .env.example",
        &format!("({}__GREETING)", vars.env_prefix),
    );
    if vars.with_auth {
        ui.status(
            Level::Ok,
            "wrote src/auth.rs",
            "(register, login, logout, sessions, password reset)",
        );
        ui.status(
            Level::Ok,
            "wrote .env",
            &format!("({}__SESSION_SECRET, from this machine)", vars.env_prefix),
        );
    }
    match git {
        Git::Committed => ui.status(Level::Ok, "initialised git, first commit", ""),
        Git::Initialised => ui.status(Level::Ok, "initialised git", "(nothing committed)"),
        Git::Skipped => {}
    }

    ui.blank();
    ui.line("  next:");
    ui.line(&format!("    cd {}", target.display()));
    ui.line("    cargo test");
    ui.line("    cargo run");
    ui.blank();
    ui.line("  then open http://localhost:3000/");
    if vars.with_auth {
        ui.blank();
        ui.line("  the hashing parameters are OWASP's floor until you measure this machine:");
        ui.line("    moso auth calibrate");
    }
    ui.blank();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::Vars;

    /// A scratch directory that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "moso-cli-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::create_dir_all(&path).expect("scratch directory");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_generated_project_has_every_template_file_on_disk() {
        let scratch = Scratch::new("files");
        let target = scratch.0.join("shop");
        let args = NewArgs {
            name: "shop".to_owned(),
            path: Some(target.clone()),
            yes: true,
            no_git: true,
            force: true,
            moso_path: None,
            with_db: false,
            auth: false,
        };
        run(&Ui::silent(), &args).expect("generated");

        for relative in [
            "Cargo.toml",
            ".gitignore",
            ".env.example",
            ".cargo/config.toml",
            "README.md",
            "src/lib.rs",
            "src/main.rs",
            "src/routes.rs",
            "src/dump.rs",
            "tests/api.rs",
        ] {
            assert!(target.join(relative).is_file(), "missing {relative}");
        }

        let manifest = std::fs::read_to_string(target.join("Cargo.toml")).expect("manifest");
        assert!(manifest.contains("name = \"shop\""), "{manifest}");
        // Nothing outside the workspace: the scratch directory has no manifest
        // above it, so no `[workspace]` stanza is needed.
        assert!(!manifest.contains("[workspace]"), "{manifest}");
    }

    #[test]
    fn a_project_inside_a_workspace_is_detached_from_it() {
        let scratch = Scratch::new("workspace");
        std::fs::write(
            scratch.0.join("Cargo.toml"),
            "[workspace]\nmembers = []\nresolver = \"3\"\n",
        )
        .expect("outer manifest");

        let target = scratch.0.join("shop");
        let args = NewArgs {
            name: "shop".to_owned(),
            path: Some(target.clone()),
            yes: true,
            no_git: true,
            force: true,
            moso_path: None,
            with_db: false,
            auth: false,
        };
        run(&Ui::silent(), &args).expect("generated");

        let manifest = std::fs::read_to_string(target.join("Cargo.toml")).expect("manifest");
        assert!(manifest.contains("[workspace]"), "{manifest}");
    }

    #[test]
    fn a_non_empty_directory_is_refused_without_force() {
        let scratch = Scratch::new("occupied");
        let target = scratch.0.join("shop");
        std::fs::create_dir_all(&target).expect("target");
        std::fs::write(target.join("keep.txt"), "mine").expect("existing file");

        let args = NewArgs {
            name: "shop".to_owned(),
            path: Some(target.clone()),
            yes: false,
            no_git: true,
            force: false,
            moso_path: None,
            with_db: false,
            auth: false,
        };
        // stdin is not a terminal under `cargo test`, so the prompt turns into
        // the usage error that tells a script what to pass.
        let error = run(&Ui::silent(), &args).expect_err("refused");
        assert_eq!(error.fault, crate::exit::Fault::Usage);
        assert!(target.join("keep.txt").is_file(), "nothing was clobbered");
    }

    #[test]
    fn force_writes_into_a_non_empty_directory() {
        let scratch = Scratch::new("forced");
        let target = scratch.0.join("shop");
        std::fs::create_dir_all(&target).expect("target");
        std::fs::write(target.join("keep.txt"), "mine").expect("existing file");

        let args = NewArgs {
            name: "shop".to_owned(),
            path: Some(target.clone()),
            yes: false,
            no_git: true,
            force: true,
            moso_path: None,
            with_db: false,
            auth: false,
        };
        run(&Ui::silent(), &args).expect("generated");
        assert!(target.join("src/main.rs").is_file());
        assert!(target.join("keep.txt").is_file(), "left alone");
    }

    #[test]
    fn the_moso_path_flag_reaches_the_manifest() {
        let scratch = Scratch::new("path-dep");
        let target = scratch.0.join("shop");
        let args = NewArgs {
            name: "shop".to_owned(),
            path: Some(target.clone()),
            yes: true,
            no_git: true,
            force: true,
            moso_path: Some(PathBuf::from("/opt/moso/crates/moso")),
            with_db: false,
            auth: false,
        };
        run(&Ui::silent(), &args).expect("generated");

        let manifest = std::fs::read_to_string(target.join("Cargo.toml")).expect("manifest");
        assert!(
            manifest.contains("path = \"/opt/moso/crates/moso\""),
            "{manifest}"
        );
    }

    #[test]
    fn an_invalid_name_never_touches_the_filesystem() {
        let scratch = Scratch::new("bad-name");
        let target = scratch.0.join("nope");
        let args = NewArgs {
            name: "9lives".to_owned(),
            path: Some(target.clone()),
            yes: true,
            no_git: true,
            force: true,
            moso_path: None,
            with_db: false,
            auth: false,
        };
        assert!(run(&Ui::silent(), &args).is_err());
        assert!(!target.exists());
    }

    #[test]
    fn a_workspace_above_the_target_is_found_and_a_plain_directory_is_not() {
        let scratch = Scratch::new("detect");
        assert_eq!(enclosing_workspace(&scratch.0.join("shop")), None);

        let manifest = scratch.0.join("Cargo.toml");
        std::fs::write(&manifest, "[workspace]\nmembers = []\n").expect("manifest");
        assert_eq!(
            enclosing_workspace(&scratch.0.join("shop")),
            Some(manifest.clone())
        );

        // A package manifest is not a workspace manifest.
        std::fs::write(&manifest, "[package]\nname = \"x\"\n").expect("manifest");
        assert_eq!(enclosing_workspace(&scratch.0.join("shop")), None);
    }

    #[test]
    fn rendered_files_are_the_ones_written() {
        let vars = Vars::for_name("shop").expect("valid");
        let rendered = vars.render_all();
        assert_eq!(rendered.len(), crate::template::FILES.len());
    }
}
