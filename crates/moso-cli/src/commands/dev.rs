//! `moso dev` — the edit loop.
//!
//! ```text
//! build → run → watch → change → rebuild → replace → watch → …
//! ```
//!
//! `00-vision.md` names the edit loop as one of only two rows Moso can lose on,
//! and `01-goals.md` puts a number on it: p50 under three seconds from save to
//! serving. This command is where that number is either met or not, so the
//! design decisions below are all about *not adding to it*.
//!
//! # What happens when the build fails
//!
//! The previous server keeps running.
//!
//! This is the one behaviour here worth arguing about, because the alternative —
//! stop the server, show the error, wait — is what most watchers do. It is
//! wrong. In the middle of an edit the code is broken most of the time, and a
//! loop that tears the process down on every intermediate state means the
//! browser tab you are refreshing shows a connection error rather than the last
//! thing that worked. Keeping the old binary serving means a failed compile
//! costs you the compiler's message and nothing else; the moment it compiles,
//! the swap happens. Pass `--exit-on-error` for CI-shaped usage where a failed
//! build should end the process instead.
//!
//! # How the old process is stopped
//!
//! [`Child::kill`](std::process::Child::kill), which is `SIGKILL`, not the
//! `SIGTERM` a production deployment sends. Moso applications drain in-flight
//! requests on `SIGTERM` within `server.grace` — 25 seconds by default — and
//! waiting 25 seconds to restart a development server would destroy the very
//! number this command exists to protect. Draining matters when real clients are
//! attached; on `127.0.0.1` with one browser tab it is latency for nothing.
//!
//! Sending `SIGTERM` instead would also mean either `libc` (an `unsafe` call,
//! and the workspace is `forbid(unsafe_code)`) or another dependency. Both are
//! the wrong price for behaviour we do not want.
//!
//! # Ctrl-C
//!
//! Not handled, deliberately. A terminal delivers `SIGINT` to the whole
//! foreground process group, so the child receives it at the same moment this
//! process does and shuts down through its own signal handling — the graceful
//! path, which is the right one when the human is leaving. [`Server`]'s `Drop`
//! is a backstop for the cases where this process exits without a signal.
//!
//! The caveat that follows from that, stated because it is easy to hit and
//! confusing to diagnose: `Drop` does **not** run when this process is killed by
//! a signal it does not handle. `kill <pid of moso dev>` — as opposed to Ctrl-C,
//! which reaches the group — therefore orphans the application, which keeps
//! holding the port and makes the next `moso dev` fail to bind. Closing that
//! properly needs a `SIGTERM` handler, which needs either `unsafe` (the
//! workspace forbids it) or another dependency; neither is worth it for a case
//! the documented way of stopping the loop does not produce. If it happens,
//! the orphan is an ordinary process and `kill` ends it.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::cli::DevArgs;
use crate::exit::{CliError, Outcome, io as io_error};
use crate::project::Project;
use crate::ui::{Level, Ui};
use crate::watch::{Snapshot, Watcher};

/// How long a burst of filesystem activity must be quiet before a rebuild.
///
/// A "save all" across six files, a `git checkout`, or `cargo fmt` over the
/// workspace all produce many events in quick succession. Rebuilding on the
/// first one wastes a compile that the second one immediately invalidates, so
/// each change restarts this timer and the build begins when the tree settles.
const QUIET_PERIOD: Duration = Duration::from_millis(150);

/// The longest the quiet period may push a rebuild back.
///
/// A process that rewrites a watched file continuously — a code generator on its
/// own watch loop, a log file that someone put in `config/` — would otherwise
/// defer the rebuild forever. After this much delay the build starts regardless.
const MAX_DEFER: Duration = Duration::from_secs(2);

/// Run the development loop.
///
/// Does not return while the loop is running: the only exits are a fatal error,
/// `--exit-on-error` with a failing build, or a signal.
///
/// # Errors
/// [`Fault::Environment`](crate::exit::Fault::Environment) when the project
/// cannot be found or the binary cannot be spawned, and
/// [`Fault::User`](crate::exit::Fault::User) for the first build failing or for
/// any failure under `--exit-on-error`.
pub fn run(ui: &Ui, args: &DevArgs) -> Outcome<()> {
    // The server inherits stdout, so this command's own console goes to stderr —
    // the same reason `moso run` does it, and it keeps `moso dev > app.log`
    // holding the application's output rather than a mixture of the two.
    let ui = &ui.on_stderr();
    let project = Project::discover(args.app.manifest_path.as_deref())?;
    project.require_moso()?;

    let watcher = if args.watch.is_empty() {
        Watcher::for_project(&project.root)
    } else {
        Watcher::new(&project.root, &args.watch)
    };

    // Catches both "every --watch path is missing" and "they exist but hold no
    // files", which look identical to the user and have the same fix.
    let mut seen = watcher.snapshot();
    if seen.is_empty() {
        return Err(CliError::user("there is nothing to watch").with_help(
            "every watched path is missing or empty; pass a --watch that exists, or drop \
             the flag to watch src/, Cargo.toml and config/",
        ));
    }

    let poll = Duration::from_millis(args.poll);

    ui.status(Level::Ok, "watching", &describe_roots(&project, &watcher));
    ui.line(&ui.dim("press ctrl-c to stop"));

    // The first build is the one failure that is fatal even without
    // `--exit-on-error`: there is no previous server to fall back to, so
    // continuing would mean watching a tree with nothing running behind it.
    let mut server = match build(ui, &project, args).and_then(|built| {
        let server = start(ui, &project, args, &built)?;
        Ok(server)
    }) {
        Ok(server) => Some(server),
        Err(error) if args.exit_on_error => return Err(error),
        Err(error) => {
            ui.error(&error);
            ui.line(&ui.dim("waiting for a change"));
            None
        }
    };

    loop {
        std::thread::sleep(poll);

        // A server that exited on its own — a panic at startup, a port already
        // bound, someone killing it from another terminal — is reported rather
        // than silently left dead, then the loop keeps watching so that the next
        // save brings it back.
        if let Some(running) = &mut server
            && let Some(status) = running.exited()
        {
            ui.warn(&format!("the server exited with status {status}"));
            ui.line(&ui.dim("waiting for a change"));
            server = None;
        }

        let current = watcher.snapshot();
        let changes = current.changes_since(&seen);
        if changes.is_empty() {
            continue;
        }

        let changes = settle(&watcher, current, changes, poll);

        ui.blank();
        ui.status(Level::Ok, "changed", &describe_changes(&project, &changes));

        // Build *before* stopping the old server, so that a build which fails
        // leaves it serving — the behaviour this command exists to provide. The
        // old process is only torn down once there is a new binary to replace it
        // with, which also keeps the window in which the port is unbound down to
        // the length of a spawn rather than the length of a compile.
        //
        // Overwriting the executable of a running process is fine on the
        // platforms `moso dev` targets: cargo writes a new file and renames over
        // the old one, and Unix keeps the running image alive through its open
        // inode.
        match build(ui, &project, args) {
            Ok(executable) => {
                if let Some(mut running) = server.take() {
                    running.stop();
                }
                match start(ui, &project, args, &executable) {
                    Ok(started) => server = Some(started),
                    Err(error) if args.exit_on_error => return Err(error),
                    Err(error) => {
                        ui.error(&error);
                        ui.line(&ui.dim("waiting for a change"));
                    }
                }
            }
            Err(error) if args.exit_on_error => return Err(error),
            Err(error) => {
                ui.error(&error);
                ui.line(&if server.is_some() {
                    ui.dim("the previous build is still serving; waiting for a change")
                } else {
                    ui.dim("waiting for a change")
                });
            }
        }

        // Re-snapshot after the build: `cargo build` touches `Cargo.lock` on a
        // dependency change, and the build script may write into a watched
        // directory. Without this the next poll would see the build's own
        // output as a change and rebuild forever.
        seen = watcher.snapshot();
    }
}

/// Wait for the filesystem to go quiet, returning everything that changed.
///
/// Each new change restarts [`QUIET_PERIOD`]; [`MAX_DEFER`] bounds the total.
/// The settled snapshot is deliberately not returned: the caller re-snapshots
/// after building anyway, to absorb whatever the build itself wrote.
fn settle(
    watcher: &Watcher,
    mut current: Snapshot,
    mut changes: Vec<PathBuf>,
    poll: Duration,
) -> Vec<PathBuf> {
    let deadline = Instant::now() + MAX_DEFER;
    let mut quiet_since = Instant::now();
    // Poll faster than the outer loop while settling: the whole point is to
    // notice the *end* of a burst promptly.
    let step = poll.min(QUIET_PERIOD / 2).max(Duration::from_millis(10));

    loop {
        if quiet_since.elapsed() >= QUIET_PERIOD || Instant::now() >= deadline {
            return changes;
        }
        std::thread::sleep(step);
        let next = watcher.snapshot();
        let more = next.changes_since(&current);
        if !more.is_empty() {
            changes.extend(more);
            changes.sort();
            changes.dedup();
            quiet_since = Instant::now();
        }
        current = next;
    }
}

/// Compile the package, reporting how long it took.
///
/// Separate from [`start`] so the caller can keep the previous server alive
/// across a failed compile.
fn build(ui: &Ui, project: &Project, args: &DevArgs) -> Outcome<Built> {
    let started = Instant::now();
    ui.status(Level::Ok, "building", &project.name);
    let path = project.build(&args.app)?;
    Ok(Built {
        path,
        elapsed: started.elapsed(),
    })
}

/// Start the binary a [`build`] produced.
fn start(ui: &Ui, project: &Project, args: &DevArgs, built: &Built) -> Outcome<Server> {
    let server = Server::spawn(&built.path, project, &args.args, &[])?;
    ui.status(
        Level::Ok,
        "running",
        &format!(
            "{} (built in {:.2}s)",
            project.name,
            built.elapsed.as_secs_f64()
        ),
    );
    Ok(server)
}

/// A successful compile: where the binary is, and how long it took to get it.
#[derive(Debug, Clone)]
struct Built {
    /// The executable cargo produced.
    path: PathBuf,
    /// Wall-clock time of the compile, reported so the edit loop is visible.
    elapsed: Duration,
}

/// A short description of what is being watched, for the opening line.
fn describe_roots(project: &Project, watcher: &Watcher) -> String {
    let names: Vec<String> = watcher
        .roots()
        .iter()
        .map(|root| relative(project, root))
        .collect();
    names.join(", ")
}

/// Name the changed paths, collapsing a long list into a count.
///
/// A `git checkout` can change hundreds of files and printing all of them would
/// push the compiler's output off the screen, which is the thing the user
/// actually needs to read.
fn describe_changes(project: &Project, changes: &[PathBuf]) -> String {
    const SHOWN: usize = 3;
    let names: Vec<String> = changes
        .iter()
        .take(SHOWN)
        .map(|path| relative(project, path))
        .collect();
    match changes.len().checked_sub(SHOWN) {
        Some(0) | None => names.join(", "),
        Some(rest) => format!("{} and {rest} more", names.join(", ")),
    }
}

/// Render a path relative to the project root when it is inside it.
fn relative(project: &Project, path: &std::path::Path) -> String {
    path.strip_prefix(&project.root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The application process under supervision.
///
/// Owns the child and guarantees it does not outlive this value: without the
/// `Drop`, an error path that returned early would leave an orphaned server
/// holding the port, and the next `moso dev` would fail to bind with a message
/// about an address in use rather than about the real problem.
///
/// Shared with [`commands::run`](super::run), which needs exactly this and
/// nothing more: spawn the binary with the project root as its working
/// directory, keep it, and know when it is gone. A second supervisor would be a
/// second place for the "reap it or the port stays bound" rule to be wrong.
#[derive(Debug)]
pub(super) struct Server {
    /// The running application.
    child: Child,
    /// Set once the child has been reaped, so `Drop` does not kill a pid that
    /// the operating system may since have reused.
    finished: bool,
}

impl Server {
    /// Start `executable` with the project root as its working directory.
    ///
    /// Standard streams are inherited: the application's logs are the point of
    /// having a dev server, and piping them through this process would add a
    /// buffering layer between a `tracing` line and the terminal. Standard
    /// *input* is not, because a served application does not read it and a
    /// `moso dev &` that steals the terminal is a bad afternoon.
    ///
    /// `env` is added to the inherited environment rather than replacing it —
    /// `moso run --profile production` sets one variable and must not take
    /// `PATH` away in the process.
    pub(super) fn spawn(
        executable: &std::path::Path,
        project: &Project,
        args: &[String],
        env: &[(&str, String)],
    ) -> Outcome<Self> {
        let mut command = Command::new(executable);
        command
            .args(args)
            .current_dir(&project.root)
            .stdin(Stdio::null());
        for (name, value) in env {
            command.env(name, value);
        }
        let child = command
            .spawn()
            .map_err(|error| io_error("could not run", executable, &error))?;
        Ok(Self {
            child,
            finished: false,
        })
    }

    /// Block until the child exits, and report how it went.
    ///
    /// Used by `moso run`, which is a wrapper rather than a supervisor: it has
    /// nothing to do while the application serves except stay out of the way and
    /// hold the child so it can be reaped.
    ///
    /// # Errors
    /// [`Fault::Environment`](crate::exit::Fault::Environment) when the child
    /// cannot be waited for at all, which on a healthy machine does not happen.
    pub(super) fn wait(&mut self) -> Outcome<std::process::ExitStatus> {
        let status = self.child.wait().map_err(|error| {
            CliError::environment(format!("could not wait for the application: {error}"))
        })?;
        self.finished = true;
        Ok(status)
    }

    /// The exit status, if the child has already finished.
    ///
    /// Reaps it when it has, which is what stops a finished server becoming a
    /// zombie for the lifetime of the session.
    fn exited(&mut self) -> Option<String> {
        if self.finished {
            return None;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.finished = true;
                Some(
                    status
                        .code()
                        .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                )
            }
            // A child that cannot be waited for is already gone as far as this
            // loop is concerned; treating the error as "still running" would
            // mean never restarting it.
            Ok(None) => None,
            Err(_) => {
                self.finished = true;
                Some("unknown".to_owned())
            }
        }
    }

    /// Kill the child and wait for it, so the port is free before the next bind.
    ///
    /// The `wait` is not optional: `kill` only delivers the signal, and
    /// returning before the process has actually gone leaves a race in which the
    /// replacement tries to bind a port the kernel has not released.
    fn stop(&mut self) {
        if self.finished {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.finished = true;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(root: &str) -> Project {
        Project {
            manifest_path: PathBuf::from(root).join("Cargo.toml"),
            root: PathBuf::from(root),
            name: "shop".to_owned(),
            rust_version: None,
            uses_moso: true,
        }
    }

    #[test]
    fn changed_paths_are_shown_relative_to_the_project_root() {
        let project = project("/tmp/shop");
        let changes = vec![PathBuf::from("/tmp/shop/src/routes.rs")];
        assert_eq!(describe_changes(&project, &changes), "src/routes.rs");
    }

    #[test]
    fn a_long_change_list_is_collapsed_to_a_count() {
        let project = project("/tmp/shop");
        let changes: Vec<PathBuf> = (0..10)
            .map(|n| PathBuf::from(format!("/tmp/shop/src/f{n}.rs")))
            .collect();
        let rendered = describe_changes(&project, &changes);
        assert!(rendered.ends_with("and 7 more"), "{rendered}");
        assert!(
            rendered.starts_with("src/f0.rs, src/f1.rs, src/f2.rs"),
            "{rendered}"
        );
    }

    #[test]
    fn exactly_three_changes_are_all_named_with_no_suffix() {
        let project = project("/tmp/shop");
        let changes: Vec<PathBuf> = (0..3)
            .map(|n| PathBuf::from(format!("/tmp/shop/src/f{n}.rs")))
            .collect();
        assert_eq!(
            describe_changes(&project, &changes),
            "src/f0.rs, src/f1.rs, src/f2.rs"
        );
    }

    #[test]
    fn a_path_outside_the_project_is_shown_in_full() {
        let project = project("/tmp/shop");
        let outside = PathBuf::from("/etc/moso.toml");
        assert_eq!(relative(&project, &outside), "/etc/moso.toml");
    }

    #[test]
    fn a_finished_server_is_only_reported_once() {
        // `true` exits immediately, which is the "the server died on its own"
        // case the loop has to notice exactly once.
        let child = Command::new("true")
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn true");
        let mut server = Server {
            child,
            finished: false,
        };

        // Give it a moment to actually exit before asking.
        for _ in 0..100 {
            if server.exited().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(server.finished, "the child should have been reaped");
        assert!(
            server.exited().is_none(),
            "a reaped child must not be reported again"
        );
    }

    #[test]
    fn stopping_a_running_server_reaps_it() {
        // `sleep` outlives the test unless it is killed, which is what `stop`
        // has to guarantee before the port can be rebound.
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let mut server = Server {
            child,
            finished: false,
        };
        assert!(server.exited().is_none(), "sleep should still be running");
        server.stop();
        assert!(server.finished);
    }
}
