//! Output: human first, machine on request.
//!
//! Three rules, from `40-cli.md`:
//!
//! 1. `--json` on every command, and when it is on **nothing** but the JSON
//!    document goes to stdout. Progress and warnings move to stderr.
//! 2. `NO_COLOR` wins over everything except an explicit `--color always`.
//! 3. Colour is off when stdout is not a terminal, because the most common
//!    consumer of a non-terminal stdout is a file someone will later read.

use std::io::{IsTerminal, Write};

use clap::ValueEnum;

/// When to colour output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum ColorChoice {
    /// Colour when stdout is a terminal and `NO_COLOR` is unset.
    #[default]
    Auto,
    /// Always colour, even into a pipe.
    Always,
    /// Never colour.
    Never,
}

/// Decide whether to emit escape sequences.
///
/// `NO_COLOR` is honoured for any value, including the empty string, which is
/// what <https://no-color.org> asks for. `--color always` still wins, because a
/// user who typed it meant it.
pub fn use_color(choice: ColorChoice, is_terminal: bool, no_color: bool) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => is_terminal && !no_color,
    }
}

/// The status of one line of output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Good.
    Ok,
    /// Worth knowing, not blocking.
    Warn,
    /// Broken.
    Fail,
    /// Neither: a plain informational row.
    Info,
}

impl Level {
    /// The glyph in the left column.
    const fn glyph(self) -> &'static str {
        match self {
            Level::Ok => "✓",
            Level::Warn => "⚠",
            Level::Fail => "✗",
            Level::Info => " ",
        }
    }

    /// The SGR parameter for the glyph, when colour is on.
    const fn sgr(self) -> &'static str {
        match self {
            Level::Ok => "32",
            Level::Warn => "33",
            Level::Fail => "31",
            Level::Info => "0",
        }
    }

    /// The machine-readable name used in `--json`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
            Level::Info => "info",
        }
    }
}

/// Everything the CLI prints goes through here.
#[derive(Debug, Clone)]
pub struct Ui {
    color: bool,
    json: bool,
    quiet: bool,
    verbose: bool,
    stderr: bool,
}

impl Ui {
    /// Build from the resolved global flags.
    pub fn new(choice: ColorChoice, json: bool, quiet: bool, verbose: bool) -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        Self {
            color: use_color(choice, std::io::stdout().is_terminal(), no_color),
            json,
            quiet,
            verbose,
            stderr: false,
        }
    }

    /// A `Ui` that never colours and never prints, for unit tests.
    #[cfg(test)]
    pub fn silent() -> Self {
        Self {
            color: false,
            json: false,
            quiet: true,
            verbose: false,
            stderr: false,
        }
    }

    /// A copy of this `Ui` that prints nothing.
    ///
    /// For a command that drives another command and reports the combined
    /// result itself: `moso build --openapi --json` calls
    /// [`openapi::export`](crate::commands::openapi::export), and two JSON
    /// documents on one stdout is not JSON. The colour choice is carried over so
    /// that a later un-muted line still looks the same.
    #[must_use]
    pub const fn muted(&self) -> Self {
        Self {
            color: self.color,
            json: false,
            quiet: true,
            verbose: false,
            stderr: self.stderr,
        }
    }

    /// A copy of this `Ui` whose prose goes to standard error.
    ///
    /// For the two commands that hand standard output to a child process,
    /// `moso run` and `moso dev`. The child inherits this process's stdout, so
    /// anything the CLI writes there lands *in the middle of the application's
    /// own output* — `moso run -- --dump-routes | jq` reads
    /// `  ✓ building shop` before the document and fails on it. Progress about a
    /// process is not the process's output, and putting it on the other stream
    /// is what makes the wrapper transparent.
    ///
    /// Errors are unaffected: they already go to stderr.
    #[must_use]
    pub const fn on_stderr(&self) -> Self {
        Self {
            color: self.color,
            json: self.json,
            quiet: self.quiet,
            verbose: self.verbose,
            stderr: true,
        }
    }

    /// Whether the caller should emit a JSON document instead of prose.
    pub const fn is_json(&self) -> bool {
        self.json
    }

    /// Whether `--verbose` was given.
    pub const fn is_verbose(&self) -> bool {
        self.verbose
    }

    /// Wrap `text` in an SGR sequence, or return it untouched.
    fn paint(&self, sgr: &str, text: &str) -> String {
        if self.color {
            format!("\u{1b}[{sgr}m{text}\u{1b}[0m")
        } else {
            text.to_owned()
        }
    }

    /// Dim text — used for the secondary half of a line.
    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    /// Bold text — used for headings and for the thing the reader came for.
    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    /// Where prose goes.
    ///
    /// stdout normally; stderr under `--json`, so that `moso routes --json`
    /// stays pipeable even when something wants to warn, and stderr for a
    /// command that has given stdout away — see [`on_stderr`](Self::on_stderr).
    fn prose(&self, line: &str) {
        if self.quiet {
            return;
        }
        if self.json || self.stderr {
            let mut stderr = std::io::stderr();
            let _ = writeln!(stderr, "{line}");
        } else {
            let mut stdout = std::io::stdout();
            let _ = writeln!(stdout, "{line}");
        }
    }

    /// A blank line, for separating blocks.
    pub fn blank(&self) {
        self.prose("");
    }

    /// A line with no status glyph.
    pub fn line(&self, text: &str) {
        self.prose(text);
    }

    /// A heading, printed bold.
    pub fn heading(&self, text: &str) {
        self.prose(&self.bold(text));
    }

    /// A status line: glyph, label, and an optional detail column.
    ///
    /// ```text
    ///   ✓ rustc 1.97.1                    (MSRV 1.90 satisfied)
    /// ```
    pub fn status(&self, level: Level, label: &str, detail: &str) {
        // An informational row has no glyph, so painting it would emit an
        // escape sequence around a space and nothing else.
        let glyph = if level == Level::Info {
            level.glyph().to_owned()
        } else {
            self.paint(level.sgr(), level.glyph())
        };
        if detail.is_empty() {
            self.prose(&format!("  {glyph} {label}"));
        } else {
            let padded = pad(label, 32);
            self.prose(&format!("  {glyph} {padded}{detail}"));
        }
    }

    /// The `→ do this` line under a status.
    pub fn fix(&self, fix: &str) {
        self.prose(&format!("      {} {fix}", self.paint("36", "→")));
    }

    /// A warning that is not part of a check list.
    pub fn warn(&self, text: &str) {
        self.prose(&format!("  {} {text}", self.paint("33", "⚠")));
    }

    /// Emit a JSON document on stdout, pretty-printed.
    ///
    /// Pretty rather than compact because the output is as often read by a
    /// person as by `jq`, and every consumer of JSON copes with whitespace.
    pub fn emit_json(&self, value: &serde_json::Value) {
        let rendered =
            serde_json::to_string_pretty(value).unwrap_or_else(|_| "{\"ok\":false}".to_owned());
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "{rendered}");
    }

    /// Write raw text to stdout with no decoration and no `--quiet` filtering.
    ///
    /// For the commands whose output *is* a document: `openapi export` without
    /// `--out`, `config --env-example`, `self completions`.
    pub fn emit_raw(&self, text: &str) {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(text.as_bytes());
        if !text.ends_with('\n') {
            let _ = stdout.write_all(b"\n");
        }
    }

    /// Print a left-aligned table with a dim header row.
    pub fn table(&self, headers: &[&str], rows: &[Vec<String>]) {
        if rows.is_empty() {
            return;
        }
        let widths = column_widths(headers, rows);

        let header = render_row(
            &headers.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>(),
            &widths,
        );
        self.prose(&self.dim(&header));
        for row in rows {
            self.prose(&render_row(row, &widths));
        }
    }

    /// Print an error, in the shape `41-diagnostics.md` asks for.
    ///
    /// Always to stderr, whatever the mode: a failure must not end up inside a
    /// file someone redirected stdout into.
    pub fn error(&self, error: &crate::exit::CliError) {
        if self.json {
            let rendered = serde_json::to_string_pretty(&error.to_json())
                .unwrap_or_else(|_| "{\"ok\":false}".to_owned());
            let mut stderr = std::io::stderr();
            let _ = writeln!(stderr, "{rendered}");
            return;
        }
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, "{}: {}", self.paint("1;31", "error"), error.message);
        if let Some(help) = &error.help {
            let _ = writeln!(stderr, "{}: {help}", self.paint("1;36", "help"));
        }
    }
}

/// Pad `text` to `width` display columns, always leaving one trailing space.
fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        format!("{text} ")
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

/// The width of each column: the widest cell, header included.
fn column_widths(headers: &[&str], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            let len = cell.chars().count();
            match widths.get_mut(index) {
                Some(width) if *width < len => *width = len,
                Some(_) => {}
                None => widths.push(len),
            }
        }
    }
    widths
}

/// One row, columns padded, no trailing whitespace.
fn render_row(cells: &[String], widths: &[usize]) -> String {
    let mut out = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        out.push_str(cell);
        if index + 1 < cells.len() {
            let width = widths.get(index).copied().unwrap_or(0);
            let len = cell.chars().count();
            if len < width {
                out.push_str(&" ".repeat(width - len));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_defeats_auto_but_not_always() {
        assert!(!use_color(ColorChoice::Auto, true, true));
        assert!(use_color(ColorChoice::Auto, true, false));
        assert!(use_color(ColorChoice::Always, false, true));
        assert!(!use_color(ColorChoice::Never, true, false));
    }

    #[test]
    fn a_pipe_is_not_coloured() {
        assert!(!use_color(ColorChoice::Auto, false, false));
    }

    #[test]
    fn a_table_pads_to_the_widest_cell_and_never_trails_whitespace() {
        let rows = vec![
            vec!["GET".to_owned(), "/users".to_owned()],
            vec!["DELETE".to_owned(), "/u".to_owned()],
        ];
        let widths = column_widths(&["METHOD", "PATH"], &rows);
        assert_eq!(widths, vec![6, 6]);
        let rendered = render_row(&rows[0], &widths);
        assert_eq!(rendered, "GET     /users");
        assert!(!rendered.ends_with(' '));
    }

    #[test]
    fn padding_counts_characters_not_bytes() {
        // A multi-byte glyph must not consume four columns of padding.
        assert_eq!(pad("é", 3).chars().count(), 3);
    }

    #[test]
    fn a_command_that_hands_over_stdout_writes_its_prose_to_stderr() {
        // Regression: `moso run` printed `  ✓ building shop` to stdout, which
        // the child then wrote its document into — so `moso run -- --dump-routes`
        // did not produce JSON. `on_stderr` is what makes the wrapper
        // transparent, and it must not be undone by any of the other flags.
        let plain = Ui::new(ColorChoice::Never, false, false, false);
        assert!(!plain.stderr, "an ordinary command still owns stdout");

        let handed_over = plain.on_stderr();
        assert!(handed_over.stderr);
        // Everything else is carried across unchanged: `--json`, `--quiet` and
        // `--verbose` still mean what the user typed.
        assert_eq!(handed_over.json, plain.json);
        assert_eq!(handed_over.quiet, plain.quiet);
        assert_eq!(handed_over.verbose, plain.verbose);
        assert_eq!(handed_over.color, plain.color);

        // And a muted copy of it stays on stderr, so `moso build --openapi`
        // driving `openapi::export` cannot smuggle a line back onto stdout.
        assert!(handed_over.muted().stderr);
    }

    #[test]
    fn painting_is_a_no_op_without_colour() {
        let ui = Ui::silent();
        assert_eq!(ui.bold("x"), "x");
        assert_eq!(ui.dim("x"), "x");
    }

    #[test]
    fn levels_have_stable_names() {
        assert_eq!(Level::Ok.as_str(), "ok");
        assert_eq!(Level::Warn.as_str(), "warn");
        assert_eq!(Level::Fail.as_str(), "fail");
    }
}
