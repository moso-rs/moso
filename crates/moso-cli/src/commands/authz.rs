//! `moso authz` — the permission registry, the roles, and why one decision went
//! the way it did.
//!
//! ```text
//! DENY  posts.publish
//!
//!   actor      usr_123 (alice@example.com)
//!   roles      Editor (global)
//!   perms      posts.read, posts.create  (from Editor)
//!   required   posts.publish
//!   reason     "not the author and not an admin"
//! ```
//!
//! # The offline entry point
//!
//! `Explanation::render` has existed and been snapshot tested since the battery
//! landed, and the `X-Moso-Authz-Explain` header reaches it on a live request.
//! What was missing was a way to ask the question without one — which is the way
//! it is nearly always asked, because "why can't Alice publish" arrives as a
//! support ticket rather than as a request you can re-issue with an extra header.
//!
//! # Why this refuses in production
//!
//! An explain trace is a description of your whole authorization model: the
//! roles the actor holds, every permission each grants, the policy that ran, its
//! source location and its reason. The header is honoured in a development
//! profile and nowhere else, and an offline entry point that did not hold the
//! same line would simply be the easier way through. So the *application*
//! refuses — it is the half that knows its own profile — and this command passes
//! `--allow-production` through, prints the refusal's reason, and exits 1.
//!
//! # Why the rendering is not done here
//!
//! `moso authz explain` prints what the application sent, verbatim. The format
//! is `Explanation::render`'s and it is snapshot tested in `moso-authz`; a
//! second renderer here would be a second thing to keep in step, and the first
//! divergence would be invisible. `--json` carries the structured `Explanation`
//! for anything that wants to lay it out differently.

use serde_json::Value;

use crate::cli::{AppArgs, AuthzArgs, AuthzCommand, AuthzExplainArgs, AuthzPermissionsArgs};
use crate::exit::{CliError, Outcome};
use crate::project::{Battery, Project};
use crate::ui::{Level, Ui};

/// Dispatch one `moso authz` subcommand.
///
/// # Errors
/// Anything the dump protocol can fail with; a project that does not use
/// `moso-authz`, which is a user error naming what to add; and an `explain`
/// refused by the production profile.
pub fn run(ui: &Ui, command: &AuthzCommand) -> Outcome<()> {
    match command {
        AuthzCommand::Permissions(args) => permissions(ui, args),
        AuthzCommand::Roles(args) => roles(ui, args),
        AuthzCommand::Explain(args) => explain(ui, args),
    }
}

/// `moso authz permissions`.
fn permissions(ui: &Ui, args: &AuthzPermissionsArgs) -> Outcome<()> {
    let mut body = serde_json::json!({ "view": "permissions" });
    if let Some(group) = &args.group {
        body["group"] = Value::String(group.clone());
    }
    let document = ask(&args.app, &body)?;

    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    let permissions = array(&document, "permissions");
    if permissions.is_empty() {
        return Err(empty(
            args.group.as_deref(),
            "this application declares no permissions",
            "declare them with `moso::permissions! { posts.read = \"View posts\", .. }`",
        ));
    }

    let rows: Vec<Vec<String>> = permissions
        .iter()
        .map(|permission| {
            vec![
                dash(&text(permission, "name")),
                dash(&text(permission, "group")),
                dash(&text(permission, "description")),
            ]
        })
        .collect();

    ui.blank();
    ui.table(&["PERMISSION", "GROUP", "DESCRIPTION"], &rows);
    ui.blank();
    ui.status(
        Level::Ok,
        &format!("{} permission(s)", permissions.len()),
        &fingerprint(&document),
    );
    // Bit order is declaration order, so the fingerprint is what tells a stored
    // permission set from one that now means something else.
    ui.line(&ui.dim("      a stored PermSet is only meaningful against this fingerprint"));
    Ok(())
}

/// `moso authz roles`.
fn roles(ui: &Ui, args: &AuthzArgs) -> Outcome<()> {
    let document = ask(&args.app, &serde_json::json!({ "view": "roles" }))?;

    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    let roles = array(&document, "roles");
    if roles.is_empty() {
        return Err(empty(
            None,
            "this application declares no roles",
            "declare them with `moso::roles! { Viewer = [posts.read], .. }`",
        ));
    }

    let rows: Vec<Vec<String>> = roles
        .iter()
        .map(|role| {
            let granted = strings(role, "permissions");
            vec![
                dash(&text(role, "name")),
                granted.len().to_string(),
                if granted.is_empty() {
                    "-".to_owned()
                } else {
                    granted.join(", ")
                },
            ]
        })
        .collect();

    ui.blank();
    ui.table(&["ROLE", "COUNT", "GRANTS"], &rows);
    ui.blank();

    // A role that grants nothing is not a bug, but it is nearly always one: it
    // is what an empty `roles!` right-hand side and a deleted permission both
    // look like from here.
    let barren = roles
        .iter()
        .filter(|role| strings(role, "permissions").is_empty())
        .count();
    if barren > 0 {
        ui.status(
            Level::Warn,
            &format!("{barren} role(s) grant nothing"),
            "(anyone holding one is refused everywhere)",
        );
    }
    Ok(())
}

/// `moso authz explain`.
fn explain(ui: &Ui, args: &AuthzExplainArgs) -> Outcome<()> {
    let body = serde_json::json!({
        "view": "explain",
        "actor": args.actor,
        "action": args.action,
        "resource": args.resource,
        "scope": args.scope,
        "allow_production": args.allow_production,
    });
    let document = ask(&args.app, &body)?;

    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    // Printed verbatim: this is `Explanation::render`'s format, and it is
    // snapshot tested where it is written rather than here.
    let rendered = document
        .get("rendered")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CliError::user("the application answered `explain` without a rendered explanation")
                .with_help(
                    "`fn authz` in src/dump.rs must send `explanation.render()` as `rendered`; \
                     this command prints it verbatim rather than inventing a second format",
                )
        })?;

    ui.blank();
    ui.emit_raw(rendered.trim_end());
    ui.blank();
    Ok(())
}

/// Build the application, ask it, and reject an answer that is not usable.
///
/// The two refusals it turns into errors are different facts: `available: false`
/// means the battery is not wired, and `refused: true` means it is wired and
/// this profile will not answer.
fn ask(app: &AppArgs, body: &Value) -> Outcome<Value> {
    let project = Project::discover(app.manifest_path.as_deref())?;
    project.require_moso()?;

    let answer = project.battery(app, &Battery::Authz(body.to_string()))?;
    let document: Value = serde_json::from_str(&answer).map_err(|error| {
        CliError::user(format!(
            "`{}` answered `--dump-authz` with something that is not JSON: {error}",
            project.name
        ))
        .with_help("src/dump.rs must print exactly one JSON document to stdout")
    })?;

    if document.get("refused").and_then(Value::as_bool) == Some(true) {
        return Err(refusal(&document));
    }

    if document.get("available").and_then(Value::as_bool) != Some(true) {
        let reason = document
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("this project does not declare permissions");
        return Err(with_help(
            CliError::user(reason.to_owned()),
            &document,
            "add `moso-authz` to Cargo.toml and implement `fn authz` in src/dump.rs",
        ));
    }

    Ok(document)
}

/// The error for an answer the application deliberately withheld.
fn refusal(document: &Value) -> CliError {
    let reason = document
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("the application refused to answer in this profile");
    let profile = document
        .get("profile")
        .and_then(Value::as_str)
        .unwrap_or("production");
    with_help(
        CliError::user(format!("{reason} (profile: {profile})")),
        document,
        "pass --allow-production if this terminal is the right place for the trace",
    )
}

/// Attach the application's own `help` line, or a fallback.
///
/// The application's wins: it knows what it is missing, and a generic line from
/// here would be advice about a project this is not.
fn with_help(error: CliError, document: &Value, fallback: &str) -> CliError {
    match document.get("help").and_then(Value::as_str) {
        Some(help) => error.with_help(help.to_owned()),
        None => error.with_help(fallback.to_owned()),
    }
}

/// The error for a well-formed answer that contains nothing.
///
/// An empty registry is a real answer and not a failure of the protocol, but it
/// is never what the person typing the command wanted, so it exits non-zero with
/// the line that fills it in. A `--group` that matched nothing says so instead.
fn empty(group: Option<&str>, message: &str, help: &str) -> CliError {
    match group {
        Some(group) => CliError::user(format!("no permission is in the group `{group}`"))
            .with_help("run `moso authz permissions` without --group to see the groups in use"),
        None => CliError::user(message.to_owned()).with_help(help.to_owned()),
    }
}

/// The registry fingerprint, as a detail column.
fn fingerprint(document: &Value) -> String {
    match document.get("fingerprint") {
        Some(Value::String(text)) => format!("(fingerprint {text})"),
        Some(Value::Number(number)) => format!("(fingerprint {number})"),
        _ => String::new(),
    }
}

/// An array field, or an empty vector.
fn array(document: &Value, key: &str) -> Vec<Value> {
    document
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// A string-array field, or an empty vector.
fn strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A string field, or the empty string.
fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// An empty cell reads as a dash rather than as a hole in the table.
fn dash(text: &str) -> String {
    if text.is_empty() {
        "-".to_owned()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_the_profile_and_the_flag_that_overrides_it() {
        let document = serde_json::json!({
            "refused": true,
            "profile": "production",
            "reason": "an explain trace describes the whole authorization model",
        });
        let error = refusal(&document);
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert!(error.message.contains("production"));
        assert!(
            error
                .help
                .is_some_and(|help| help.contains("--allow-production"))
        );
    }

    #[test]
    fn the_applications_own_help_line_wins_over_the_fallback() {
        let document = serde_json::json!({"help": "cargo add moso-authz"});
        let error = with_help(CliError::user("nope"), &document, "the fallback");
        assert_eq!(error.help.as_deref(), Some("cargo add moso-authz"));

        let bare = with_help(
            CliError::user("nope"),
            &serde_json::json!({}),
            "the fallback",
        );
        assert_eq!(bare.help.as_deref(), Some("the fallback"));
    }

    #[test]
    fn an_empty_group_filter_is_reported_as_a_filter_problem() {
        let error = empty(Some("billing"), "no permissions", "declare some");
        assert!(error.message.contains("group `billing`"));
        assert!(error.help.is_some_and(|help| help.contains("--group")));
    }

    #[test]
    fn an_empty_registry_is_reported_as_a_missing_declaration() {
        let error = empty(
            None,
            "this application declares no permissions",
            "declare some",
        );
        assert_eq!(error.message, "this application declares no permissions");
        assert_eq!(error.help.as_deref(), Some("declare some"));
    }

    #[test]
    fn a_fingerprint_renders_whether_it_arrives_as_text_or_as_a_number() {
        assert_eq!(
            fingerprint(&serde_json::json!({"fingerprint": "0x9f2a"})),
            "(fingerprint 0x9f2a)"
        );
        assert_eq!(
            fingerprint(&serde_json::json!({"fingerprint": 40746_u64})),
            "(fingerprint 40746)"
        );
        assert_eq!(fingerprint(&serde_json::json!({})), "");
    }

    #[test]
    fn missing_fields_read_as_empty_rather_than_panicking() {
        let empty = serde_json::json!({});
        assert!(array(&empty, "permissions").is_empty());
        assert!(strings(&empty, "permissions").is_empty());
        assert_eq!(dash(&text(&empty, "name")), "-");
    }
}
