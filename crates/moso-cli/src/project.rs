//! Finding the user's project, and asking it questions.
//!
//! # The dump protocol
//!
//! `moso routes`, `moso openapi` and `moso config` all need something only the
//! user's own binary knows: the router after it has been assembled, the OpenAPI
//! document after every extractor has contributed to it, the configuration
//! after six sources have been layered. None of that can be recovered by
//! parsing source, and linking the user's crate into the CLI is not possible —
//! the CLI is one prebuilt binary and the application is arbitrary code.
//!
//! So the CLI asks the application:
//!
//! ```text
//! cargo build --message-format=json   →  the path of the binary
//! <that binary> --dump-openapi        →  one JSON document on stdout
//! ```
//!
//! which is `cargo run -- --dump-openapi` with the compile step pulled out in
//! front. Three things are bought by separating them: compiler diagnostics
//! reach the terminal unmangled, the *run* can be given a short timeout without
//! that timeout also applying to a cold build, and a project with several
//! binaries produces a comprehensible error instead of a cargo one.
//!
//! The application's side of the protocol is `src/dump.rs`, which `moso new`
//! writes into the project as ordinary, editable code. Eight flags are defined:
//! `--dump-openapi`, `--dump-routes`, `--dump-config`, `--dump-env-example`,
//! `--dump-middleware`, `--dump-jobs`, `--dump-authz` and `--dump-auth`. Each
//! prints exactly one document to stdout and exits 0; everything else goes to
//! stderr.
//!
//! # Questions that carry a request
//!
//! The first five are pure functions of an application that has already been
//! built, so the flag alone is the whole question and [`Dump`] is a fieldless
//! enum. The last three are not: `moso jobs dlq --job send_welcome --limit 50`,
//! `moso authz explain --actor usr_1 --action publish` and
//! `moso auth calibrate --target-ms 250` carry parameters, and one of them
//! changes something. They are [`Battery`], which passes one JSON request
//! document as the next argument — so a new filter is a field rather than a
//! flag, and the two halves cannot drift over argument order.
//!
//! `--dump-auth` is there for a third reason on top of the parameter: its answer
//! is a *measurement*. Argon2id parameters are a property of the hardware the
//! hash will run on, so the only place the question can honestly be asked is
//! inside the binary that will do the hashing — which is exactly what this
//! protocol reaches.
//!
//! # The database protocol
//!
//! `moso db` speaks the same shape with a different vocabulary — `--db-status`,
//! `--db-migrate`, `--db-migrate-tenants`, `--db-rollback <n>`, `--db-redo`,
//! `--db-make-migration <name>`, `--db-check`, `--db-squash` and
//! `--db-seed [name]` — answered by `src/db.rs`, which `moso new --with-db`
//! writes. It is a separate set of flags rather than more `Dump` variants for
//! two reasons: several of them take arguments, and they are given an hour
//! rather than a minute, because a migration is not a pure function of
//! already-loaded state and must never be killed halfway.
//!
//! The arguments are plain adjacent tokens rather than the JSON request
//! [`Battery`] carries, because `src/db.rs` is a file the user reads and edits:
//! a `position` and five `any` calls are something they can follow, and a
//! `serde` round trip in the middle of it is not.
//!
//! Why it delegates at all, rather than the CLI opening the database itself, is
//! in [`commands::db`](crate::commands::db) — short version: a migration may be
//! Rust that lives in the user's crate, and this binary cannot link it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cli::AppArgs;
use crate::exit::{CliError, Outcome, io as io_error};

/// How long the application is given to answer a `--dump-*`, once it has been
/// built.
///
/// Generous for what is a pure function of already-loaded state, because a cold
/// filesystem and a loaded machine both make it slower than it looks.
const DUMP_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a `--db-*` is given.
///
/// Much longer, because it is not a pure function of anything: it opens a
/// connection, waits for an advisory lock another process may be holding, and
/// then runs DDL against a table that may have a hundred million rows. A
/// migration killed halfway is the failure this timeout exists *not* to cause,
/// so it is set past the point where a human would have intervened anyway.
const DB_TIMEOUT: Duration = Duration::from_secs(3600);

/// How long a `--dump-jobs` or `--dump-authz` is given.
///
/// Between the two: these open the queue backend or the role source, so they are
/// not the pure function of loaded state a `--dump-*` normally is, and a bulk
/// dead-letter retry moves rows. But none of them is a migration, and a read
/// that has not answered in five minutes is a connection problem the operator
/// wants told about rather than waited out.
const BATTERY_TIMEOUT: Duration = Duration::from_secs(300);

/// One of the five fieldless questions the CLI can ask an application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dump {
    /// The OpenAPI document.
    OpenApi,
    /// The route table.
    Routes,
    /// The resolved configuration.
    Config,
    /// The regenerated `.env.example`.
    EnvExample,
    /// The composed middleware stack, outermost first.
    Middleware,
}

impl Dump {
    /// The flag passed to the application.
    pub const fn flag(self) -> &'static str {
        match self {
            Dump::OpenApi => "--dump-openapi",
            Dump::Routes => "--dump-routes",
            Dump::Config => "--dump-config",
            Dump::EnvExample => "--dump-env-example",
            Dump::Middleware => "--dump-middleware",
        }
    }
}

/// One question about a battery, and the request document it carries.
///
/// Separate from [`Dump`] for the reason given in the module header: these take
/// an argument. The payload is the request as JSON text — built by the command
/// that asks, parsed by `src/dump.rs`, and never inspected in between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Battery {
    /// `--dump-jobs <request>`.
    Jobs(String),
    /// `--dump-authz <request>`.
    Authz(String),
    /// `--dump-auth <request>`.
    Auth(String),
}

impl Battery {
    /// The arguments passed to the application.
    #[must_use]
    pub fn flags(&self) -> Vec<String> {
        match self {
            Battery::Jobs(request) => vec!["--dump-jobs".to_owned(), request.clone()],
            Battery::Authz(request) => vec!["--dump-authz".to_owned(), request.clone()],
            Battery::Auth(request) => vec!["--dump-auth".to_owned(), request.clone()],
        }
    }

    /// The flag alone, which is how this reads in a diagnostic.
    ///
    /// The request is deliberately left out: it is a JSON blob that would fill
    /// the line, and the reader already knows what they asked for.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Battery::Jobs(_) => "--dump-jobs",
            Battery::Authz(_) => "--dump-authz",
            Battery::Auth(_) => "--dump-auth",
        }
    }
}

/// One database operation, as the application's `--db-*` protocol spells it.
///
/// Every variant is one primary flag; the ones carrying data spell it as the
/// tokens that follow, adjacent to the flag they belong to. `src/db.rs` reads
/// them back with a `position` and a handful of lookups rather than a parser,
/// which is only sound because the order here is the order it expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Db {
    /// `--db-status`.
    Status,
    /// `--db-migrate`.
    Migrate,
    /// `--db-migrate-tenants`.
    MigrateTenants,
    /// `--db-rollback <steps>`.
    Rollback(usize),
    /// `--db-redo`.
    Redo,
    /// `--db-make-migration <name> [--db-dry-run] [--db-rename <old:new>]…`.
    MakeMigration {
        /// What the migration is called, before slugification.
        name: String,
        /// Build the files and write nothing.
        dry_run: bool,
        /// `old:new` answers to the rename questions a diff cannot settle.
        renames: Vec<String>,
        /// Answer every remaining rename question as a drop and an add.
        drop_and_add: bool,
    },
    /// `--db-check`.
    Check,
    /// `--db-squash [--db-apply]`.
    Squash {
        /// Write the baseline and delete the files it replaces.
        apply: bool,
    },
    /// `--db-seed [name] [--db-force]`.
    Seed {
        /// Which seed to run. Every registered one when absent.
        name: Option<String>,
        /// Run even under a production profile.
        force: bool,
    },
}

impl Db {
    /// The arguments passed to the application.
    #[must_use]
    pub fn flags(&self) -> Vec<String> {
        match self {
            Db::Status => vec!["--db-status".to_owned()],
            Db::Migrate => vec!["--db-migrate".to_owned()],
            Db::MigrateTenants => vec!["--db-migrate-tenants".to_owned()],
            Db::Redo => vec!["--db-redo".to_owned()],
            Db::Rollback(steps) => vec!["--db-rollback".to_owned(), steps.to_string()],
            Db::Check => vec!["--db-check".to_owned()],
            Db::Squash { apply } => {
                let mut flags = vec!["--db-squash".to_owned()];
                if *apply {
                    flags.push("--db-apply".to_owned());
                }
                flags
            }
            Db::Seed { name, force } => {
                let mut flags = vec!["--db-seed".to_owned()];
                // The name comes first and unadorned, because `src/db.rs` reads
                // it as "the token right after the command" — putting a
                // modifier in between would make `--db-force` the seed's name.
                if let Some(name) = name {
                    flags.push(name.clone());
                }
                if *force {
                    flags.push("--db-force".to_owned());
                }
                flags
            }
            Db::MakeMigration {
                name,
                dry_run,
                renames,
                drop_and_add,
            } => {
                let mut flags = vec!["--db-make-migration".to_owned(), name.clone()];
                if *dry_run {
                    flags.push("--db-dry-run".to_owned());
                }
                for rename in renames {
                    flags.push("--db-rename".to_owned());
                    flags.push(rename.clone());
                }
                if *drop_and_add {
                    flags.push("--db-drop-and-add".to_owned());
                }
                flags
            }
        }
    }

    /// How this reads in a diagnostic.
    #[must_use]
    pub fn label(&self) -> String {
        self.flags().join(" ")
    }
}

/// A Cargo package the CLI can drive.
#[derive(Debug, Clone)]
pub struct Project {
    /// The package's `Cargo.toml`.
    pub manifest_path: PathBuf,
    /// The directory containing it. Every command runs with this as its cwd, so
    /// that `.env`, `config/` and relative output paths resolve the way they do
    /// when the application is started by hand.
    pub root: PathBuf,
    /// The package name.
    pub name: String,
    /// `package.rust-version`, when the manifest declares one.
    pub rust_version: Option<String>,
    /// Whether `moso` is among the dependencies.
    pub uses_moso: bool,
}

impl Project {
    /// Find the package to operate on.
    ///
    /// With `--manifest-path`, that file. Without, the nearest `Cargo.toml`
    /// with a `[package]` table at or above the working directory — the same
    /// rule cargo itself uses, so `moso routes` works from a subdirectory.
    ///
    /// # Errors
    /// [`Fault::Environment`](crate::exit::Fault::Environment) when there is no
    /// such manifest, or when the one found does not parse.
    pub fn discover(explicit: Option<&Path>) -> Outcome<Self> {
        let manifest_path = match explicit {
            Some(path) => {
                let path = if path.is_dir() {
                    path.join("Cargo.toml")
                } else {
                    path.to_path_buf()
                };
                if !path.is_file() {
                    return Err(CliError::environment(format!(
                        "no manifest at `{}`",
                        path.display()
                    ))
                    .with_help("point --manifest-path at a Cargo.toml"));
                }
                path
            }
            None => find_manifest()?,
        };

        // Absolutise before anything else uses it. Every command runs cargo with
        // `--manifest-path <this>` *and* `current_dir(root)`, so a relative path
        // — which is what `--manifest-path examples/crud/Cargo.toml` gives — is
        // resolved a second time against the directory it already points into,
        // and cargo reports a manifest that "does not exist" while the user is
        // looking straight at it.
        //
        // `std::path::absolute` rather than `canonicalize`: the path has already
        // been proven to exist, and resolving symlinks would rewrite the
        // `/tmp/...` the user typed into the `/private/tmp/...` macOS actually
        // uses, which then appears in every diagnostic.
        let manifest_path = std::path::absolute(&manifest_path).unwrap_or(manifest_path);

        let root = manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let text = std::fs::read_to_string(&manifest_path)
            .map_err(|error| io_error("could not read", &manifest_path, &error))?;
        let manifest: toml::Value = toml::from_str(&text).map_err(|error| {
            CliError::environment(format!(
                "`{}` is not valid TOML: {error}",
                manifest_path.display()
            ))
        })?;

        let package = manifest.get("package").ok_or_else(|| {
            CliError::environment(format!(
                "`{}` has no [package] table",
                manifest_path.display()
            ))
            .with_help(
                "this is a workspace root; run the command inside a package, or pass \
                 --manifest-path <package>/Cargo.toml",
            )
        })?;

        let name = package
            .get("name")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| {
                CliError::environment(format!("`{}` has no package.name", manifest_path.display()))
            })?
            .to_owned();

        let rust_version = package
            .get("rust-version")
            .and_then(toml::Value::as_str)
            .map(str::to_owned);

        let uses_moso = ["dependencies", "dev-dependencies", "build-dependencies"]
            .iter()
            .filter_map(|table| manifest.get(*table))
            .any(|table| table.get("moso").is_some());

        Ok(Self {
            manifest_path,
            root,
            name,
            rust_version,
            uses_moso,
        })
    }

    /// Refuse to continue if this is not a Moso project.
    ///
    /// # Errors
    /// [`Fault::User`](crate::exit::Fault::User), naming the package that was
    /// found so the reader can see they are in the wrong directory.
    pub fn require_moso(&self) -> Outcome<()> {
        if self.uses_moso {
            return Ok(());
        }
        Err(CliError::user(format!(
            "`{}` does not depend on moso, so it cannot answer this",
            self.name
        ))
        .with_help("cargo add moso, or run the command inside a Moso project"))
    }

    /// Ask the application one question and return its answer verbatim.
    ///
    /// # Errors
    /// Every failure mode of the protocol, each with the next step: no binary
    /// target, a build that failed, a non-zero exit, an empty answer (the
    /// project has no `src/dump.rs`) and a run that never terminated (the flag
    /// was ignored and the application started serving).
    pub fn dump(&self, args: &AppArgs, dump: Dump) -> Outcome<String> {
        self.dump_with_env(args, dump, &[])
    }

    /// Ask the application one question with `env` added to its environment.
    ///
    /// `moso deploy checklist` is the reason this exists: the configuration it
    /// has to audit is the one the *production* profile resolves, and the
    /// profile is chosen by `MOSO_PROFILE` inside the application. Setting it in
    /// this process instead would need `std::env::set_var`, which is `unsafe` in
    /// edition 2024 and this crate forbids `unsafe`.
    ///
    /// The variables are added to the inherited environment, never substituted
    /// for it: an application still needs its `PATH` and its `DATABASE_URL`.
    ///
    /// # Errors
    /// As [`dump`](Self::dump).
    pub fn dump_with_env(
        &self,
        args: &AppArgs,
        dump: Dump,
        env: &[(&str, &str)],
    ) -> Outcome<String> {
        let executable = self.build(args)?;
        self.run(
            &executable,
            &[dump.flag().to_owned()],
            dump.flag(),
            DUMP_TIMEOUT,
            NO_ANSWER_HELP,
            env,
        )
    }

    /// Ask the application one question that carries a request document.
    ///
    /// Separate from [`dump`](Self::dump) in two ways: the flag takes an
    /// argument, and the timeout is five minutes rather than one, because the
    /// answer comes from a queue backend or a role source rather than from
    /// memory.
    ///
    /// # Errors
    /// As [`dump`](Self::dump). An application that has not wired the battery
    /// still answers — with `{"available": false, ..}` — so "not wired" arrives
    /// here as a successful document rather than as a failure.
    pub fn battery(&self, args: &AppArgs, battery: &Battery) -> Outcome<String> {
        let executable = self.build(args)?;
        self.run(
            &executable,
            &battery.flags(),
            battery.label(),
            BATTERY_TIMEOUT,
            NO_ANSWER_HELP,
            &[],
        )
    }

    /// Ask the application to perform one database operation.
    ///
    /// Separate from [`dump`](Self::dump) in exactly three ways, all of which
    /// matter: the flag takes an argument, the timeout is an hour rather than a
    /// minute, and the "it printed nothing" diagnostic points at `src/db.rs`
    /// and `moso new --with-db` rather than at `src/dump.rs`.
    ///
    /// # Errors
    /// As [`dump`](Self::dump), plus whatever the migration itself failed with —
    /// which the application has already printed to stderr.
    pub fn db(&self, args: &AppArgs, command: &Db) -> Outcome<String> {
        let executable = self.build(args)?;
        let label = command.label();
        self.run(
            &executable,
            &command.flags(),
            &label,
            DB_TIMEOUT,
            "this project has no database story; create one with `moso new --with-db`, \
             or copy src/db.rs and the migrations/ directory from a project that has one",
            &[],
        )
    }

    /// Build the package and return the path of the binary.
    ///
    /// Compiler diagnostics go straight to the terminal — stderr is inherited,
    /// and `--message-format=json-render-diagnostics` is what puts the rendered
    /// form there rather than only the JSON. Only the machine-readable half is
    /// captured, which is where the artefact path comes from.
    ///
    /// Public because `moso dev` rebuilds on every change and needs the path
    /// without also running the dump protocol.
    ///
    /// # Errors
    /// [`Fault::User`](crate::exit::Fault::User) when the package does not
    /// compile, and [`Fault::Environment`](crate::exit::Fault::Environment)
    /// when cargo itself cannot be run.
    pub fn build(&self, args: &AppArgs) -> Outcome<PathBuf> {
        let mut command = Command::new(cargo());
        command
            .arg("build")
            .arg("--message-format=json-render-diagnostics")
            .arg("--manifest-path")
            .arg(&self.manifest_path)
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        if let Some(bin) = &args.bin {
            command.arg("--bin").arg(bin);
        }
        if args.release {
            command.arg("--release");
        }
        if let Some(features) = &args.features {
            command.arg("--features").arg(features);
        }

        let output = command.output().map_err(|error| {
            CliError::environment(format!("could not run cargo: {error}"))
                .with_help("install Rust from https://rustup.rs")
        })?;

        if !output.status.success() {
            return Err(CliError::user(format!("`{}` did not compile", self.name))
                .with_help("fix the errors above, then run this command again"));
        }

        let executables = executables(&String::from_utf8_lossy(&output.stdout));
        select_executable(executables, args.bin.as_deref(), &self.name)
    }

    /// Run the built binary with `flags` and capture stdout.
    ///
    /// `label` is how the invocation reads in a diagnostic, and `empty_help` is
    /// what to suggest when the application answers with nothing — which means
    /// different things for the two protocols, so the caller supplies it.
    /// `env` is added to the inherited environment, never substituted for it.
    fn run(
        &self,
        executable: &Path,
        flags: &[String],
        label: &str,
        timeout: Duration,
        empty_help: &str,
        env: &[(&str, &str)],
    ) -> Outcome<String> {
        let mut command = Command::new(executable);
        command
            .args(flags)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command
            .spawn()
            .map_err(|error| io_error("could not run", executable, &error))?;

        let stdout = child.stdout.take().ok_or_else(|| {
            CliError::environment("could not capture the application's standard output")
        })?;

        // Read on a thread: a pipe that nobody drains fills up, and an
        // application blocked writing a large OpenAPI document would look
        // exactly like an application that hung.
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let result = std::io::BufReader::new(stdout).read_to_end(&mut buffer);
            let _ = sender.send(result.map(|_| buffer));
        });

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => return Err(io_error("could not wait for", executable, &error)),
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(CliError::user(format!(
                    "`{}` did not answer `{label}` within {}s",
                    self.name,
                    timeout.as_secs()
                ))
                .with_help(
                    "the binary ignored the flag and started serving; `main` must check \
                     for the flag before `serve()` — see src/dump.rs and src/db.rs in a \
                     project created by `moso new`",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        let captured = receiver
            .recv()
            .unwrap_or_else(|_| Ok(Vec::new()))
            .map_err(|error| io_error("could not read the output of", executable, &error))?;
        let _ = reader.join();

        if !status.success() {
            let code = status.code().unwrap_or(-1);
            return Err(CliError::user(format!(
                "`{}` exited with status {code} while answering `{label}`",
                self.name
            ))
            .with_help("the failure is printed above; it came from your application"));
        }

        let answer = String::from_utf8(captured).map_err(|_| {
            CliError::user(format!(
                "`{}` answered `{label}` with something that is not UTF-8",
                self.name
            ))
        })?;

        if answer.trim().is_empty() {
            return Err(
                CliError::user(format!("`{}` printed nothing for `{label}`", self.name))
                    .with_help(empty_help),
            );
        }

        Ok(answer)
    }
}

/// What to suggest when a `--dump-*` produced no output.
const NO_ANSWER_HELP: &str = "the project does not implement the dump protocol; copy src/dump.rs from a project \
     created by `moso new`, or run `moso new` in a scratch directory to see what it \
     should contain";

/// The cargo to invoke: the one that launched us, if we were launched by cargo.
pub(crate) fn cargo() -> PathBuf {
    std::env::var_os("CARGO").map_or_else(|| PathBuf::from("cargo"), PathBuf::from)
}

/// Walk up from the working directory looking for a package manifest.
///
/// A virtual workspace root — a `Cargo.toml` with `[workspace]` and no
/// `[package]` — is not the answer, so the walk climbs past it. But it is not
/// nothing either: standing in the root of a workspace is where `moso generate
/// workspace` leaves you, and "no Cargo.toml at or above here" would be a false
/// statement about a directory with a `Cargo.toml` in it. So the nearest one is
/// remembered, and if the climb finds no package at all its members are the
/// fallback — resolved in [`workspace_member`], which either finds exactly one
/// package or names them all.
fn find_manifest() -> Outcome<PathBuf> {
    let start = std::env::current_dir().map_err(|error| {
        CliError::environment(format!("could not read the working directory: {error}"))
    })?;

    let mut workspace_root: Option<PathBuf> = None;
    for directory in start.ancestors() {
        let candidate = directory.join("Cargo.toml");
        if !candidate.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&candidate) else {
            continue;
        };
        let Ok(value) = toml::from_str::<toml::Value>(&text) else {
            continue;
        };
        if value.get("package").is_some() {
            return Ok(candidate);
        }
        if workspace_root.is_none() && value.get("workspace").is_some() {
            workspace_root = Some(candidate);
        }
    }

    if let Some(root) = workspace_root {
        return workspace_member(&root);
    }

    Err(
        CliError::environment(format!("no Cargo.toml at or above `{}`", start.display()))
            .with_help("run this inside a Moso project, or create one with `moso new <name>`"),
    )
}

/// The one package of a virtual workspace, or an error naming the candidates.
///
/// Choosing when there is exactly one is not a guess: a single-member workspace
/// has one package the command could possibly mean, and that is the shape
/// `moso generate workspace` produces, so the split project keeps answering
/// `moso routes` from its root the way it did before the split. Two or more is
/// genuinely ambiguous and says so, listing the manifests to point at — the
/// same advice [`Project::discover`] gives when `--manifest-path` names a
/// workspace root directly.
fn workspace_member(root: &Path) -> Outcome<PathBuf> {
    let directory = root.parent().unwrap_or(Path::new("."));
    let mut packages: Vec<PathBuf> = member_globs(root)
        .into_iter()
        .flat_map(|pattern| expand_member(directory, &pattern))
        .map(|member| member.join("Cargo.toml"))
        .filter(|manifest| is_package(manifest))
        .collect();
    packages.sort();
    packages.dedup();

    match packages.len() {
        1 => Ok(packages.into_iter().next().expect("length checked")),
        0 => Err(CliError::environment(format!(
            "`{}` is a workspace root with no member package",
            root.display()
        ))
        .with_help(
            "create one with `cargo new --lib crates/<name>`, or run this inside a \
             package that already exists",
        )),
        _ => {
            let listed: Vec<String> = packages
                .iter()
                .filter_map(|manifest| manifest.parent())
                .filter_map(|member| member.strip_prefix(directory).ok())
                .map(|member| member.display().to_string())
                .collect();
            Err(CliError::environment(format!(
                "`{}` is a workspace root with {} member packages",
                root.display(),
                packages.len()
            ))
            .with_help(format!(
                "run the command inside one, or pass --manifest-path <member>/Cargo.toml — \
                 the members are: {}",
                listed.join(", ")
            )))
        }
    }
}

/// The `workspace.members` patterns a manifest declares.
fn member_globs(manifest: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return Vec::new();
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return Vec::new();
    };
    value
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Turn one `members` entry into the directories it names.
///
/// Cargo's globbing is richer than this. Only the trailing `*` is handled,
/// because it is the one `moso generate workspace` writes and the one nearly
/// every workspace uses; anything else is treated as a literal path, which for
/// a pattern this cannot expand means the member is simply not found and the
/// caller reports the ambiguity rather than the wrong package.
fn expand_member(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let Some(prefix) = pattern.strip_suffix("/*") else {
        return vec![root.join(pattern)];
    };
    let Ok(entries) = std::fs::read_dir(root.join(prefix)) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

/// Whether a manifest declares a package rather than only a workspace.
fn is_package(manifest: &Path) -> bool {
    std::fs::read_to_string(manifest)
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .is_some_and(|value| value.get("package").is_some())
}

/// One binary target cargo produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executable {
    /// The target's name, which is what `--bin` selects on.
    pub name: String,
    /// Where the binary landed.
    pub path: PathBuf,
}

/// Pull the binary artefacts out of `cargo build --message-format=json` output.
///
/// Every line is one JSON object; the interesting ones have a non-null
/// `executable` and a target whose `kind` contains `bin`. Lines that are not
/// JSON are ignored rather than fatal: cargo is free to add message kinds, and
/// a new one must not break the CLI.
pub fn executables(stdout: &str) -> Vec<Executable> {
    let mut found = Vec::new();
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(path) = message
            .get("executable")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let target = message.get("target");
        let is_bin = target
            .and_then(|target| target.get("kind"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("bin")));
        if !is_bin {
            continue;
        }
        let name = target
            .and_then(|target| target.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let candidate = Executable {
            name,
            path: PathBuf::from(path),
        };
        if !found.contains(&candidate) {
            found.push(candidate);
        }
    }
    found
}

/// Choose which binary to interrogate.
///
/// # Errors
/// When there is none, or when there are several and none was named.
pub fn select_executable(
    executables: Vec<Executable>,
    requested: Option<&str>,
    package: &str,
) -> Outcome<PathBuf> {
    if let Some(name) = requested {
        return executables
            .into_iter()
            .find(|executable| executable.name == name)
            .map(|executable| executable.path)
            .ok_or_else(|| {
                CliError::user(format!("`{package}` has no binary called `{name}`"))
                    .with_help("run `cargo build --bins` to see the binaries it does have")
            });
    }

    match executables.len() {
        0 => Err(
            CliError::user(format!("`{package}` has no binary target")).with_help(
                "add a `src/main.rs`, or pass --manifest-path for the package that has one",
            ),
        ),
        1 => Ok(executables.into_iter().next().expect("length checked").path),
        _ => {
            let names: Vec<&str> = executables
                .iter()
                .map(|executable| executable.name.as_str())
                .collect();
            Err(
                CliError::usage(format!("`{package}` has {} binaries", executables.len()))
                    .with_help(format!("pass --bin, one of: {}", names.join(", "))),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARGO_OUTPUT: &str = concat!(
        r#"{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"shop"},"executable":null}"#,
        "\n",
        r#"{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"shop"},"executable":"/t/debug/shop"}"#,
        "\n",
        "not json at all\n",
        r#"{"reason":"build-finished","success":true}"#,
    );

    /// Regression: `--manifest-path <relative>` used to be passed to cargo
    /// verbatim while cargo also ran with `current_dir(root)`, so the path was
    /// resolved twice and every such invocation failed with "manifest path does
    /// not exist". Discovery absolutises it, which is what breaks the cycle.
    #[test]
    fn a_relative_manifest_path_is_made_absolute() {
        let base = std::env::temp_dir().join(format!("moso-project-{}", std::process::id()));
        let package = base.join("shop");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&package).expect("scratch package");
        std::fs::write(
            package.join("Cargo.toml"),
            "[package]\nname = \"shop\"\nversion = \"0.1.0\"\n\n[dependencies]\nmoso = \"0.1\"\n",
        )
        .expect("manifest");

        let previous = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&base).expect("enter scratch");
        let discovered = Project::discover(Some(Path::new("shop/Cargo.toml")));
        std::env::set_current_dir(previous).expect("restore cwd");

        let project = discovered.expect("discovers the package");
        assert!(
            project.manifest_path.is_absolute(),
            "{} should be absolute",
            project.manifest_path.display()
        );
        assert!(
            project.root.is_absolute(),
            "the root should be absolute too"
        );
        assert!(project.manifest_path.starts_with(&project.root));
        assert_eq!(project.name, "shop");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Build a virtual workspace root with `names` as its member packages.
    fn scratch_workspace(tag: &str, names: &[&str]) -> PathBuf {
        let base = std::env::temp_dir().join(format!("moso-ws-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("crates")).expect("scratch workspace");
        std::fs::write(
            base.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
        )
        .expect("workspace manifest");
        for name in names {
            let member = base.join("crates").join(name);
            std::fs::create_dir_all(member.join("src")).expect("member");
            std::fs::write(
                member.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n\
                     [dependencies]\nmoso = \"0.1\"\n"
                ),
            )
            .expect("member manifest");
        }
        base
    }

    /// Regression: `moso generate workspace` leaves the user standing in a
    /// virtual workspace root, and every command that drives the application
    /// answered "no Cargo.toml at or above here" — about a directory with a
    /// `Cargo.toml` in it. One member is not an ambiguity, so it is used.
    #[test]
    fn a_single_member_workspace_root_resolves_to_that_member() {
        let base = scratch_workspace("one", &["shop"]);
        let found = workspace_member(&base.join("Cargo.toml")).expect("resolves the one member");
        assert_eq!(found, base.join("crates/shop/Cargo.toml"));

        let project = Project::discover(Some(&found)).expect("discovers it");
        assert_eq!(project.name, "shop");
        assert!(project.uses_moso);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn several_members_are_an_error_that_lists_them() {
        let base = scratch_workspace("many", &["shop", "shop-domain"]);
        let error = workspace_member(&base.join("Cargo.toml")).expect_err("ambiguous");
        assert_eq!(error.fault, crate::exit::Fault::Environment);
        let help = error.help.expect("names the members");
        assert!(help.contains("crates/shop"), "{help}");
        assert!(help.contains("crates/shop-domain"), "{help}");
        assert!(help.contains("--manifest-path"), "{help}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_workspace_root_with_no_member_says_so_rather_than_denying_the_manifest() {
        let base = scratch_workspace("empty", &[]);
        let error = workspace_member(&base.join("Cargo.toml")).expect_err("no members");
        assert!(
            error.message.contains("no member package"),
            "{}",
            error.message
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_member_entry_that_is_not_a_glob_is_taken_literally() {
        let base = scratch_workspace("literal", &["shop"]);
        assert_eq!(
            expand_member(&base, "crates/shop"),
            vec![base.join("crates/shop")]
        );
        assert_eq!(
            expand_member(&base, "crates/*"),
            vec![base.join("crates/shop")]
        );
        // A pattern this cannot expand finds nothing rather than the wrong
        // directory, and the caller reports the ambiguity.
        assert!(expand_member(&base, "nope/*").is_empty());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_binary_artefacts_are_collected() {
        let found = executables(CARGO_OUTPUT);
        assert_eq!(
            found,
            vec![Executable {
                name: "shop".to_owned(),
                path: PathBuf::from("/t/debug/shop"),
            }]
        );
    }

    #[test]
    fn a_repeated_artefact_is_reported_once() {
        let doubled = format!("{CARGO_OUTPUT}\n{CARGO_OUTPUT}");
        assert_eq!(executables(&doubled).len(), 1);
    }

    #[test]
    fn a_single_binary_needs_no_flag() {
        let path = select_executable(executables(CARGO_OUTPUT), None, "shop").expect("selected");
        assert_eq!(path, PathBuf::from("/t/debug/shop"));
    }

    #[test]
    fn several_binaries_are_a_usage_error_that_lists_them() {
        let found = vec![
            Executable {
                name: "web".to_owned(),
                path: PathBuf::from("/t/web"),
            },
            Executable {
                name: "worker".to_owned(),
                path: PathBuf::from("/t/worker"),
            },
        ];
        let error = select_executable(found.clone(), None, "shop").expect_err("ambiguous");
        assert_eq!(error.fault, crate::exit::Fault::Usage);
        assert!(error.help.is_some_and(|help| help.contains("web, worker")));

        let chosen = select_executable(found, Some("worker"), "shop").expect("named");
        assert_eq!(chosen, PathBuf::from("/t/worker"));
    }

    #[test]
    fn no_binary_is_a_user_error() {
        let error = select_executable(Vec::new(), None, "shop").expect_err("none");
        assert_eq!(error.fault, crate::exit::Fault::User);
    }

    #[test]
    fn an_unknown_binary_name_is_a_user_error() {
        let error = select_executable(executables(CARGO_OUTPUT), Some("nope"), "shop")
            .expect_err("missing");
        assert_eq!(error.fault, crate::exit::Fault::User);
    }

    /// The template as `moso new` would write it, for the protocol assertions.
    fn dump_rs() -> &'static str {
        crate::template::FILES
            .iter()
            .find(|file| file.path == "src/dump.rs")
            .expect("the template ships src/dump.rs")
            .contents
    }

    #[test]
    fn every_dump_has_a_distinct_flag_the_template_implements() {
        let dumps = [
            Dump::OpenApi,
            Dump::Routes,
            Dump::Config,
            Dump::EnvExample,
            Dump::Middleware,
        ];
        let mut flags: Vec<&str> = dumps.iter().map(|dump| dump.flag()).collect();
        flags.sort_unstable();
        flags.dedup();
        assert_eq!(flags.len(), dumps.len(), "two dumps share a flag");

        // The other half of the protocol. If a flag is renamed here without
        // being renamed in the template, `moso routes` starts hanging against
        // every freshly generated project — so assert they agree.
        for flag in flags {
            assert!(
                dump_rs().contains(&format!("\"{flag}\"")),
                "src/dump.rs does not handle `{flag}`"
            );
        }
    }

    #[test]
    fn every_battery_question_is_a_flag_the_template_implements() {
        // The same guarantee for the two that carry a request. A flag the
        // template does not recognise falls through to `serve()`, so the failure
        // would be a five-minute timeout rather than an error.
        for battery in [
            Battery::Jobs(String::from("{}")),
            Battery::Authz(String::from("{}")),
            Battery::Auth(String::from("{}")),
        ] {
            assert_eq!(battery.flags().len(), 2, "the request must be passed on");
            assert_eq!(battery.flags()[0], battery.label());
            assert!(
                dump_rs().contains(&format!("\"{}\"", battery.label())),
                "src/dump.rs does not handle `{}`",
                battery.label()
            );
        }
    }
}
