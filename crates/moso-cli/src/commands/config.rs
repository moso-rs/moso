//! `moso config` — what the application will actually read, and where from.
//!
//! Four jobs, and this file is the dispatch between them.
//!
//! | Mode | Answers |
//! | --- | --- |
//! | plain | why is this value what it is: every key, the winner, the source |
//! | `--env-example` | what the committed example should say, from the `Config` type |
//! | `--check` | which of those two disagree, and what else is silently wrong |
//! | `--generate-secret` | one value from the operating system's CSPRNG |
//!
//! The first two are here; the other two are large enough to have their own
//! homes, in [`config_check`](super::config_check) and [`secret`](super::secret).
//!
//! `--generate-secret` is dispatched **before** the project is discovered. It
//! is entropy, not configuration: refusing to produce a key because the working
//! directory is not a Cargo package would be a rule with no reason behind it,
//! and the first thing a new project needs is the secret that goes in its
//! `.env`.
//!
//! Secret fields are redacted by the application before they reach us; the CLI
//! never sees them and so cannot leak them into a terminal recording.

use serde_json::Value;

use crate::cli::ConfigArgs;
use crate::exit::{CliError, Outcome, io as io_error};
use crate::project::{Dump, Project};
use crate::ui::{Level, Ui};

/// Run `moso config`.
///
/// # Errors
/// Anything the dump protocol can fail with, a write that is refused, a machine
/// with no random number generator, and — from `--check` — a non-zero exit for
/// every configuration problem it found.
pub fn run(ui: &Ui, args: &ConfigArgs) -> Outcome<()> {
    if args.generate_secret {
        return super::secret::run(ui, args);
    }

    let project = Project::discover(args.app.manifest_path.as_deref())?;
    project.require_moso()?;

    if args.check {
        return super::config_check::run(ui, &project, args);
    }
    if args.env_example {
        return env_example(ui, &project, args);
    }
    resolved(ui, &project, args)
}

/// `moso config --env-example`.
fn env_example(ui: &Ui, project: &Project, args: &ConfigArgs) -> Outcome<()> {
    let mut text = project.dump(&args.app, Dump::EnvExample)?;
    if !text.ends_with('\n') {
        text.push('\n');
    }

    let Some(out) = &args.out else {
        if ui.is_json() {
            ui.emit_json(&serde_json::json!({ "ok": true, "env_example": text }));
        } else {
            ui.emit_raw(&text);
        }
        return Ok(());
    };

    let path = project.root.join(out);
    std::fs::write(&path, &text).map_err(|error| io_error("could not write", &path, &error))?;

    let keys = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#') && line.contains('='))
        .count();
    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": true,
            "path": path.display().to_string(),
            "keys": keys,
        }));
    } else {
        ui.status(
            Level::Ok,
            &format!("wrote {}", out.display()),
            &format!("({keys} keys)"),
        );
    }
    Ok(())
}

/// Plain `moso config`.
fn resolved(ui: &Ui, project: &Project, args: &ConfigArgs) -> Outcome<()> {
    let answer = project.dump(&args.app, Dump::Config)?;
    let document: Value = serde_json::from_str(&answer).map_err(|error| {
        CliError::user(format!(
            "the application's `--dump-config` output is not JSON: {error}"
        ))
        .with_help("everything except the document must go to stderr")
    })?;

    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    let profile = document
        .get("profile")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let entries = document
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CliError::user("the application's `--dump-config` output has no `entries` array")
                .with_help("compare src/dump.rs with the one `moso new` writes")
        })?;

    ui.blank();
    ui.heading(&format!("  profile: {profile}"));
    ui.blank();

    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|entry| {
            vec![
                field(entry, "key"),
                field(entry, "env"),
                field(entry, "value"),
                entry
                    .get("origin")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .to_owned(),
            ]
        })
        .collect();

    if rows.is_empty() {
        ui.warn("this application's Config type has no fields");
        return Ok(());
    }

    ui.table(&["KEY", "ENVIRONMENT", "VALUE", "FROM"], &rows);
    ui.blank();

    let unset = entries
        .iter()
        .filter(|entry| entry.get("origin").is_none_or(Value::is_null))
        .count();
    if unset > 0 {
        ui.status(
            Level::Warn,
            &format!("{unset} keys have no value at all"),
            "(no source supplied them and they have no default)",
        );
        ui.fix("moso config --env-example --out .env.example, then fill in .env");
    }

    Ok(())
}

/// A string field, or a placeholder.
fn field(entry: &Value, key: &str) -> String {
    entry
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = r#"{
      "profile": "dev",
      "entries": [
        {"key":"greeting","env":"SHOP__GREETING","value":"hello","origin":"default","secret":false},
        {"key":"secret_key","env":"SHOP__SECRET_KEY","value":"[redacted]","origin":null,"secret":true}
      ]
    }"#;

    #[test]
    fn entries_render_every_column() {
        let document: Value = serde_json::from_str(ANSWER).expect("json");
        let entries = document["entries"].as_array().expect("array");
        assert_eq!(field(&entries[0], "key"), "greeting");
        assert_eq!(field(&entries[0], "env"), "SHOP__GREETING");
        assert_eq!(field(&entries[0], "value"), "hello");
        assert_eq!(field(&entries[1], "value"), "[redacted]");
    }

    #[test]
    fn a_missing_field_renders_as_a_dash_rather_than_a_panic() {
        let entry: Value = serde_json::json!({});
        assert_eq!(field(&entry, "key"), "-");
    }

    #[test]
    fn a_null_origin_counts_as_unset() {
        let document: Value = serde_json::from_str(ANSWER).expect("json");
        let unset = document["entries"]
            .as_array()
            .expect("array")
            .iter()
            .filter(|entry| entry.get("origin").is_none_or(Value::is_null))
            .count();
        assert_eq!(unset, 1);
    }

    #[test]
    fn keys_are_counted_ignoring_comments_and_blank_lines() {
        let text = "# a comment\n\nSHOP__GREETING=hello\n# SHOP__OTHER=x\nSHOP__B=\n";
        let keys = text
            .lines()
            .filter(|line| !line.trim_start().starts_with('#') && line.contains('='))
            .count();
        assert_eq!(keys, 2);
    }
}
