//! `moso self completions <shell>`.
//!
//! Generated from the same clap tree the CLI parses with, so a flag that exists
//! is a flag that completes and a flag that is removed stops completing in the
//! same commit.

use std::io::Write;

use clap::CommandFactory;
use clap_complete::Shell;

use crate::cli::Cli;
use crate::exit::Outcome;
use crate::ui::Ui;

/// The binary name the completion script is written for.
const BINARY: &str = "moso";

/// Run `moso self completions`.
///
/// The script goes to stdout even under `--json`: it is a shell script, it is
/// meant to be redirected into a file, and wrapping it in JSON would only mean
/// every user had to unwrap it again.
///
/// # Errors
/// Never, in practice: `clap_complete` writes into an in-memory buffer that
/// cannot fail, and a broken stdout is not something to report on stdout.
pub fn run(ui: &Ui, shell: Shell) -> Outcome<()> {
    let mut command = Cli::command();
    let mut buffer: Vec<u8> = Vec::new();
    clap_complete::generate(shell, &mut command, BINARY, &mut buffer);

    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(&buffer);
    let _ = stdout.flush();

    if ui.is_verbose() {
        ui.line(&format!("  generated {shell} completions for `{BINARY}`"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(shell: Shell) -> String {
        let mut command = Cli::command();
        let mut buffer = Vec::new();
        clap_complete::generate(shell, &mut command, BINARY, &mut buffer);
        String::from_utf8(buffer).expect("completions are UTF-8")
    }

    #[test]
    fn every_supported_shell_produces_a_non_empty_script() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::Elvish,
            Shell::PowerShell,
        ] {
            let generated = script(shell);
            assert!(!generated.is_empty(), "{shell} produced nothing");
            assert!(
                generated.contains("moso"),
                "{shell} script does not mention the binary"
            );
        }
    }

    #[test]
    fn every_subcommand_and_sub_subcommand_appears_in_the_completion_script() {
        // Walked out of the tree rather than listed here: a hand-written list
        // silently stops covering the command added after it was written, which
        // is the one whose completions nobody would think to check.
        let tree = Cli::command();
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let generated = script(shell);
            for subcommand in tree.get_subcommands() {
                assert!(
                    generated.contains(subcommand.get_name()),
                    "`{}` is missing from the {shell} completions",
                    subcommand.get_name()
                );
                for nested in subcommand.get_subcommands() {
                    assert!(
                        generated.contains(nested.get_name()),
                        "`{} {}` is missing from the {shell} completions",
                        subcommand.get_name(),
                        nested.get_name()
                    );
                }
            }
        }
    }

    #[test]
    fn the_global_flags_appear_too() {
        let generated = script(Shell::Bash);
        assert!(generated.contains("--json"), "{generated}");
        assert!(generated.contains("--color"), "{generated}");
    }
}
