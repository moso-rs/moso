#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "The `moso` command line interface."]
//!
//! Scaffolds a project, runs the edit loop, generates resources into an
//! existing project, interrogates one, lints it, drives its migrations, its
//! queues and its permissions, and checks that the machine can build any of it.
//! What it deliberately does not do is anything it would have to invent an
//! answer for: a subcommand that cannot be finished is absent from the command
//! tree rather than printing "coming soon", and a command whose answer depends
//! on a battery the project has not wired says so and exits non-zero rather than
//! printing an empty table.
//!
//! # How it talks to your application
//!
//! Several commands need something only your binary knows: the
//! assembled router, the generated OpenAPI document, the configuration after
//! six sources have been layered, the migrations that are pending. The CLI
//! cannot link your crate, so it asks: it builds the package with cargo and runs
//! the resulting binary with a `--dump-*` or `--db-*` flag, reading one document
//! off standard output. The application's half of that protocol is ordinary code
//! in `src/dump.rs` and `src/db.rs`, written by `moso new`. [`project`]
//! documents it in full.
//!
//! # Exit codes
//!
//! 0 ok, 1 user error, 2 usage error, 3 environment problem — see [`exit`].

mod cli;
mod client;
mod commands;
mod exit;
mod naming;
mod project;
mod template;
mod ui;
mod watch;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;
use crate::ui::Ui;

fn main() -> ExitCode {
    // Clap handles `--help`, `--version` and a malformed command line itself,
    // exiting 2 on error — which is exactly the documented usage-error code.
    let cli = Cli::parse();
    let ui = Ui::new(
        cli.global.color,
        cli.global.json,
        cli.global.quiet,
        cli.global.verbose,
    );

    match commands::dispatch(&ui, &cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            ui.error(&error);
            error.code()
        }
    }
}
