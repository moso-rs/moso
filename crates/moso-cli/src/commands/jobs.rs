//! `moso jobs` — the operator's view of the background queues.
//!
//! ```text
//! QUEUE    READY  RUNNING  RETRYING  DEAD  OLDEST READY
//! default     12        3         0     1  4s
//! mail         0        0         2     7  -
//! ```
//!
//! # What this is, and what it deliberately is not
//!
//! It is `list`, `status`, `schedules`, `dlq`, `retry` and `discard`: the six
//! things somebody does at three in the morning when a queue is backed up. It is
//! **not** `moso worker`. A worker is a long-lived process that loads your job
//! bodies, holds leases, and has to be deployed, scaled and drained alongside the
//! web process — so it belongs in your own binary, next to the `App` it shares a
//! registry with, and `Worker::run` is a line you write rather than a command the
//! CLI hides. A CLI-owned worker would also have to link your crate, which
//! ADR-0004 says it cannot.
//!
//! # Depth alone is not an answer
//!
//! `status` prints latency beside depth because a queue of ten thousand that
//! drains in a second is healthy and a queue of four whose oldest job has been
//! waiting an hour is not. Depth on its own is the number that gets watched and
//! the number that says least.
//!
//! # Why `retry` and `discard` go through the same protocol as the reads
//!
//! They change something, so they could have been a separate family like
//! `--db-*`. They are not, because the filter a person used to *look* at a page
//! is the filter they then act on, and one request document means the two cannot
//! be spelled differently. What keeps them safe is the limit: it is always sent,
//! it defaults to 50, and `discard` asks before it runs. A bulk operation over an
//! unbounded filter is how a fix becomes an outage.

use std::io::{IsTerminal, Write};

use serde_json::Value;

use crate::cli::{AppArgs, DlqFilterArgs, JobsArgs, JobsBulkArgs, JobsCommand, JobsDlqArgs};
use crate::exit::{CliError, Outcome};
use crate::project::{Battery, Project};
use crate::ui::{Level, Ui};

/// Dispatch one `moso jobs` subcommand.
///
/// # Errors
/// Anything the dump protocol can fail with; a project that does not use
/// `moso-jobs`, which is a user error naming the feature to enable; and a
/// `discard` that was not confirmed.
pub fn run(ui: &Ui, command: &JobsCommand) -> Outcome<()> {
    match command {
        JobsCommand::List(args) => list(ui, args),
        JobsCommand::Status(args) => status(ui, args),
        JobsCommand::Schedules(args) => schedules(ui, args),
        JobsCommand::Dlq(args) => dlq(ui, args),
        JobsCommand::Retry(args) => bulk(ui, args, Act::Retry),
        JobsCommand::Discard(args) => bulk(ui, args, Act::Discard),
    }
}

/// Which of the two bulk operations is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    /// Put the dead letters back on their queues.
    Retry,
    /// Throw them away.
    Discard,
}

impl Act {
    /// The wire name, which is what `src/dump.rs` branches on.
    const fn as_str(self) -> &'static str {
        match self {
            Act::Retry => "retry",
            Act::Discard => "discard",
        }
    }

    /// Whether this destroys work, and therefore has to ask first.
    const fn destructive(self) -> bool {
        matches!(self, Act::Discard)
    }
}

/// `moso jobs list`.
fn list(ui: &Ui, args: &JobsArgs) -> Outcome<()> {
    let document = ask(&args.app, &request("registry", Value::Null))?;
    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    let jobs = array(&document, "jobs");
    if jobs.is_empty() {
        ui.warn("this project registers no jobs");
        ui.fix("`JobRegistry::new().register::<SendWelcomeEmail>()`, then hand it to `Jobs::new`");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = jobs
        .iter()
        .map(|job| {
            vec![
                text(job, "name"),
                dash(&text(job, "queue")),
                seconds(job, "timeout_seconds"),
                dash(&text(job, "priority")),
                count(job, "retries"),
                if flag(job, "serial") { "yes" } else { "-" }.to_owned(),
            ]
        })
        .collect();

    ui.blank();
    ui.table(
        &["JOB", "QUEUE", "TIMEOUT", "PRIORITY", "RETRIES", "SERIAL"],
        &rows,
    );
    ui.blank();
    ui.status(
        Level::Ok,
        &format!("{} job type(s) registered", jobs.len()),
        &backend(&document),
    );
    Ok(())
}

/// `moso jobs status`.
fn status(ui: &Ui, args: &JobsArgs) -> Outcome<()> {
    let document = ask(&args.app, &request("queues", Value::Null))?;
    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    let queues = array(&document, "queues");
    if queues.is_empty() {
        ui.warn("this project has no queues to report on");
        ui.fix("a queue appears once a job is registered against it");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = queues
        .iter()
        .map(|queue| {
            vec![
                dash(&text(queue, "queue")),
                count(queue, "ready"),
                count(queue, "running"),
                count(queue, "retrying"),
                count(queue, "dead"),
                oldest(queue),
            ]
        })
        .collect();

    ui.blank();
    ui.table(
        &[
            "QUEUE",
            "READY",
            "RUNNING",
            "RETRYING",
            "DEAD",
            "OLDEST READY",
        ],
        &rows,
    );
    ui.blank();

    let dead: u64 = queues.iter().map(|queue| number(queue, "dead")).sum();
    if dead > 0 {
        ui.status(
            Level::Warn,
            &format!("{dead} job(s) in the dead-letter queue"),
            "(moso jobs dlq)",
        );
    } else {
        ui.status(Level::Ok, "no dead letters", "");
    }
    Ok(())
}

/// `moso jobs schedules`.
fn schedules(ui: &Ui, args: &JobsArgs) -> Outcome<()> {
    let document = ask(&args.app, &request("schedules", Value::Null))?;
    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    let schedules = array(&document, "schedules");
    if schedules.is_empty() {
        ui.warn("this project registers no schedules");
        ui.fix("`JobRegistry::new().schedule(Cron::new(\"0 3 * * *\", MyJob))`");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = schedules
        .iter()
        .map(|schedule| {
            vec![
                dash(&text(schedule, "id")),
                dash(&text(schedule, "job")),
                dash(&text(schedule, "expression")),
                dash(&text(schedule, "timezone")),
                dash(&text(schedule, "last_run")),
                dash(&text(schedule, "next_run")),
            ]
        })
        .collect();

    ui.blank();
    ui.table(&["ID", "JOB", "WHEN", "TZ", "LAST RUN", "NEXT RUN"], &rows);
    ui.blank();

    // "Who leads this schedule" is fleet-wide and comes out of the backend;
    // "am I the leader" is local and absent for a process that runs no
    // scheduler. Saying so beats printing a confident `no`.
    let unled = schedules
        .iter()
        .filter(|schedule| schedule.get("next_run").is_none_or(Value::is_null))
        .count();
    if unled > 0 {
        ui.status(
            Level::Warn,
            &format!("{unled} schedule(s) have no next occurrence"),
            "(the expression never fires again, or it did not parse)",
        );
    }
    Ok(())
}

/// `moso jobs dlq`.
fn dlq(ui: &Ui, args: &JobsDlqArgs) -> Outcome<()> {
    let limit = limit(args.limit)?;
    let mut body = serde_json::json!({ "filter": filter(&args.filter), "limit": limit });
    if let Some(cursor) = &args.cursor {
        body["cursor"] = Value::String(cursor.clone());
    }

    let document = ask(&args.app, &request("dead", body))?;
    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    let dead = array(&document, "dead");
    if dead.is_empty() {
        ui.status(Level::Ok, "the dead-letter queue is empty", "");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = dead
        .iter()
        .map(|letter| {
            vec![
                dash(&text(letter, "id")),
                dash(&text(letter, "name")),
                dash(&text(letter, "queue")),
                count(letter, "attempts"),
                dash(&text(letter, "failed_at")),
                // The first line only, and the whole chain under --json. A
                // table that wraps is a table nobody reads; a truncated chain
                // in a machine document would be a lie.
                first_line(&text(letter, "last_error")),
            ]
        })
        .collect();

    ui.blank();
    ui.table(
        &["ID", "JOB", "QUEUE", "ATTEMPTS", "FAILED AT", "LAST ERROR"],
        &rows,
    );
    ui.blank();
    ui.status(
        Level::Warn,
        &format!("{} dead letter(s) shown", dead.len()),
        "(--json carries the whole error chain and the payload)",
    );

    if let Some(cursor) = document.get("cursor").and_then(Value::as_str) {
        ui.status(Level::Info, "more to come", &format!("--cursor {cursor}"));
    }
    Ok(())
}

/// `moso jobs retry` and `moso jobs discard`.
fn bulk(ui: &Ui, args: &JobsBulkArgs, act: Act) -> Outcome<()> {
    let limit = limit(args.limit)?;

    if act.destructive() && !args.yes {
        let question = format!(
            "discard up to {limit} dead letter(s) matching {}? this cannot be undone",
            describe(&args.filter)
        );
        if !confirm(&question)? {
            return Err(CliError::user("cancelled").with_help("nothing was discarded"));
        }
    }

    let body = serde_json::json!({
        "action": act.as_str(),
        "filter": filter(&args.filter),
        "limit": limit,
    });
    let document = ask(&args.app, &request("dead", body))?;

    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    let affected = number(&document, "affected");
    if affected == 0 {
        ui.status(
            Level::Ok,
            "nothing to do",
            &format!("no dead letter matches {}", describe(&args.filter)),
        );
        return Ok(());
    }
    ui.status(
        Level::Ok,
        match act {
            Act::Retry => "re-enqueued",
            Act::Discard => "discarded",
        },
        &format!("{affected} dead letter(s)"),
    );
    if affected >= u64::from(limit) {
        ui.status(
            Level::Info,
            "the limit was reached",
            "(run it again, or raise --limit)",
        );
    }
    Ok(())
}

/// Build the request document for one view.
fn request(view: &str, body: Value) -> Value {
    let mut document = serde_json::json!({ "view": view });
    if let Some(fields) = body.as_object() {
        for (key, value) in fields {
            document[key] = value.clone();
        }
    }
    document
}

/// The filter, as `src/dump.rs` reads it.
///
/// Every key is present whether or not it was given, so the application does not
/// have to distinguish "absent" from "null" — one shape, always.
fn filter(args: &DlqFilterArgs) -> Value {
    serde_json::json!({
        "job": args.job,
        "queue": args.queue,
        "error": args.error,
        "id": args.id,
    })
}

/// How a filter reads in a confirmation prompt.
fn describe(args: &DlqFilterArgs) -> String {
    let mut parts = Vec::new();
    if let Some(id) = &args.id {
        parts.push(format!("id {id}"));
    }
    if let Some(job) = &args.job {
        parts.push(format!("job {job}"));
    }
    if let Some(queue) = &args.queue {
        parts.push(format!("queue {queue}"));
    }
    if let Some(error) = &args.error {
        parts.push(format!("error containing {error:?}"));
    }
    if parts.is_empty() {
        "every job".to_owned()
    } else {
        parts.join(", ")
    }
}

/// Reject a limit that would act on nothing or on everything.
fn limit(limit: u32) -> Outcome<u32> {
    if limit == 0 {
        return Err(CliError::usage("--limit 0 would act on nothing")
            .with_help("pass a limit of at least 1"));
    }
    if limit > 10_000 {
        return Err(
            CliError::usage(format!("--limit {limit} is above the cap of 10000")).with_help(
                "run it in batches; an unbounded bulk operation is how a fix becomes an outage",
            ),
        );
    }
    Ok(limit)
}

/// Build the application, ask it, and reject an answer that is not usable.
fn ask(app: &AppArgs, body: &Value) -> Outcome<Value> {
    let project = Project::discover(app.manifest_path.as_deref())?;
    project.require_moso()?;

    let answer = project.battery(app, &Battery::Jobs(body.to_string()))?;
    let document: Value = serde_json::from_str(&answer).map_err(|error| {
        CliError::user(format!(
            "`{}` answered `--dump-jobs` with something that is not JSON: {error}",
            project.name
        ))
        .with_help("src/dump.rs must print exactly one JSON document to stdout")
    })?;

    // The application says whether it has a jobs battery at all. Anything else
    // here would be this command guessing, and a guess that produced an empty
    // table would read as "your queues are fine".
    if document.get("available").and_then(Value::as_bool) != Some(true) {
        let reason = document
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("this project does not report on background jobs");
        let error = CliError::user(reason.to_owned());
        return Err(match document.get("help").and_then(Value::as_str) {
            Some(help) => error.with_help(help.to_owned()),
            None => error.with_help(
                "enable the `jobs` feature on the `moso` dependency and implement `fn jobs` \
                 in src/dump.rs",
            ),
        });
    }

    Ok(document)
}

/// Ask a yes/no question on the terminal.
///
/// A non-terminal stdin is a usage error rather than a silent yes: a bulk
/// discard run from a script that did not say `--yes` is exactly the case this
/// exists to stop.
///
/// # Errors
/// [`Fault::Usage`](crate::exit::Fault::Usage) when there is no terminal to ask.
fn confirm(question: &str) -> Outcome<bool> {
    if !std::io::stdin().is_terminal() {
        return Err(
            CliError::usage(format!("cannot ask `{question}`: stdin is not a terminal"))
                .with_help("pass --yes to answer it in advance"),
        );
    }
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "  {question} [y/N] ");
    let _ = stdout.flush();

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

// ---------------------------------------------------------------------------
// Reading the answer
// ---------------------------------------------------------------------------

/// An array field, or an empty vector.
fn array(document: &Value, key: &str) -> Vec<Value> {
    document
        .get(key)
        .and_then(Value::as_array)
        .cloned()
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

/// A boolean field, defaulting to false.
fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// An unsigned field, defaulting to zero.
fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// A count, rendered.
fn count(value: &Value, key: &str) -> String {
    number(value, key).to_string()
}

/// A duration in seconds, rendered, or a dash when the field is absent.
fn seconds(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map_or_else(|| "-".to_owned(), |seconds| format!("{seconds}s"))
}

/// The `OLDEST READY` column: how long the oldest waiting job has waited.
fn oldest(queue: &Value) -> String {
    match queue.get("oldest_ready_seconds").and_then(Value::as_f64) {
        Some(seconds) if seconds >= 60.0 => {
            format!("{}m{}s", seconds as u64 / 60, seconds as u64 % 60)
        }
        Some(seconds) => format!("{seconds:.0}s"),
        None => "-".to_owned(),
    }
}

/// An empty cell reads as a dash rather than as a hole in the table.
fn dash(text: &str) -> String {
    if text.is_empty() {
        "-".to_owned()
    } else {
        text.to_owned()
    }
}

/// The first line of an error chain, for a table cell.
fn first_line(chain: &str) -> String {
    dash(chain.lines().next().unwrap_or_default().trim())
}

/// The backend's name, as a detail column.
fn backend(document: &Value) -> String {
    match document.get("backend").and_then(Value::as_str) {
        Some(name) => format!("(backend: {name})"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_carries_the_view_and_the_body_at_the_top_level() {
        let body = serde_json::json!({ "limit": 10, "filter": {"job": "send"} });
        let request = request("dead", body);
        assert_eq!(request["view"], "dead");
        assert_eq!(request["limit"], 10);
        assert_eq!(request["filter"]["job"], "send");
    }

    #[test]
    fn a_view_with_no_body_is_still_a_well_formed_request() {
        let request = request("registry", Value::Null);
        assert_eq!(request["view"], "registry");
        assert_eq!(request.as_object().expect("object").len(), 1);
    }

    #[test]
    fn every_filter_key_is_present_even_when_it_was_not_given() {
        let filter = filter(&DlqFilterArgs::default());
        for key in ["job", "queue", "error", "id"] {
            assert!(filter.get(key).is_some_and(Value::is_null), "{key} missing");
        }
    }

    #[test]
    fn a_filter_describes_itself_for_the_prompt() {
        assert_eq!(describe(&DlqFilterArgs::default()), "every job");
        let narrowed = DlqFilterArgs {
            job: Some("send_welcome".to_owned()),
            queue: Some("mail".to_owned()),
            ..DlqFilterArgs::default()
        };
        assert_eq!(describe(&narrowed), "job send_welcome, queue mail");
    }

    #[test]
    fn a_limit_of_zero_and_a_limit_above_the_cap_are_both_usage_errors() {
        assert_eq!(limit(0).expect_err("zero").fault, crate::exit::Fault::Usage);
        assert_eq!(
            limit(10_001).expect_err("too large").fault,
            crate::exit::Fault::Usage
        );
        assert_eq!(limit(50).expect("in range"), 50);
        assert_eq!(limit(10_000).expect("at the cap"), 10_000);
    }

    #[test]
    fn the_oldest_ready_column_reads_as_a_duration() {
        assert_eq!(
            oldest(&serde_json::json!({"oldest_ready_seconds": 4.2})),
            "4s"
        );
        assert_eq!(
            oldest(&serde_json::json!({"oldest_ready_seconds": 125.0})),
            "2m5s"
        );
        assert_eq!(oldest(&serde_json::json!({})), "-");
    }

    #[test]
    fn a_table_cell_never_renders_as_a_blank() {
        assert_eq!(dash(""), "-");
        assert_eq!(first_line(""), "-");
        assert_eq!(first_line("boom\ncaused by: nope"), "boom");
        assert_eq!(seconds(&serde_json::json!({}), "timeout_seconds"), "-");
        assert_eq!(
            seconds(
                &serde_json::json!({"timeout_seconds": 30}),
                "timeout_seconds"
            ),
            "30s"
        );
    }

    #[test]
    fn only_discard_has_to_ask() {
        assert!(Act::Discard.destructive());
        assert!(!Act::Retry.destructive());
        assert_eq!(Act::Retry.as_str(), "retry");
        assert_eq!(Act::Discard.as_str(), "discard");
    }

    #[test]
    fn missing_fields_read_as_empty_rather_than_panicking() {
        let empty = serde_json::json!({});
        assert!(array(&empty, "jobs").is_empty());
        assert_eq!(number(&empty, "affected"), 0);
        assert!(!flag(&empty, "serial"));
        assert_eq!(backend(&empty), "");
        // A field of the wrong type is also empty: `src/dump.rs` belongs to the
        // application and may have been edited.
        let wrong = serde_json::json!({"jobs": "not an array"});
        assert!(array(&wrong, "jobs").is_empty());
    }
}
