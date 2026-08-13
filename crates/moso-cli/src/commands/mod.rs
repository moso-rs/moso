//! One module per subcommand.
//!
//! Every `run` takes a [`Ui`] and its own argument struct and
//! returns an [`Outcome`]. Nothing here calls
//! `std::process::exit`: the exit code is derived from the error in `main`, so
//! that a command cannot accidentally exit 0 after printing a failure.

pub mod auth;
pub mod authz;
pub mod build;
pub mod check;
pub mod client;
pub mod completions;
pub mod config;
pub mod config_check;
pub mod db;
pub mod deploy;
pub mod dev;
pub mod doctor;
pub mod generate;
pub mod jobs;
pub mod middleware;
pub mod new;
pub mod openapi;
pub mod routes;
pub mod run;
pub mod secret;
pub mod test;
pub mod update;
pub mod workspace;

use crate::cli::{Command, OpenapiCommand, SelfCommand};
use crate::exit::Outcome;
use crate::ui::Ui;

/// Dispatch one parsed command line.
///
/// # Errors
/// Whatever the chosen subcommand returns.
pub fn dispatch(ui: &Ui, command: &Command) -> Outcome<()> {
    match command {
        Command::New(args) => new::run(ui, args),
        Command::Openapi { command } => match command {
            OpenapiCommand::Export(args) => openapi::export(ui, args),
            OpenapiCommand::Check(args) => openapi::check(ui, args),
        },
        Command::Db { command } => db::run(ui, command),
        Command::Generate(args) => generate::run(ui, args),
        Command::Dev(args) => dev::run(ui, args),
        Command::Client(args) => client::run(ui, args),
        Command::Routes(args) => routes::run(ui, args),
        Command::Middleware(args) => middleware::run(ui, args),
        Command::Check(args) => check::run(ui, args),
        Command::Jobs { command } => jobs::run(ui, command),
        Command::Auth { command } => auth::run(ui, command),
        Command::Authz { command } => authz::run(ui, command),
        Command::Doctor(args) => doctor::run(ui, args),
        Command::Config(args) => config::run(ui, args),
        Command::Run(args) => run::run(ui, args),
        Command::Build(args) => build::run(ui, args),
        Command::Test(args) => test::run(ui, args),
        Command::Deploy { command } => deploy::run(ui, command),
        Command::Own { command } => match command {
            SelfCommand::Completions { shell } => completions::run(ui, *shell),
            SelfCommand::Update(args) => update::run(ui, args),
        },
    }
}
