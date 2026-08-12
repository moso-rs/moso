//! `moso openapi export` and `moso openapi check`.
//!
//! # `export --prefix`
//!
//! `export` can narrow the document to one path prefix before it is written, so
//! a multi-version API becomes one committed document per version. The filter
//! is a structural one on the raw JSON — [`filter_prefix`] retains the `paths`
//! entries at or under the prefix on segment boundaries and leaves
//! `components/schemas` whole, because the CLI depends on no Moso crate and so
//! cannot reuse `Document::filter_prefix`, and pruning components correctly
//! would need the transitive `$ref` trace an over-broad-but-valid component set
//! makes unnecessary.
//!
//! `check` is the `openapi_drift` lint of `40-cli.md` in command form: the
//! committed document is part of the repository's contract with its clients, so
//! a pull request that changes an endpoint without regenerating it should fail
//! CI. Comparing parsed JSON rather than bytes means reformatting the file, or
//! a different indent, is not a failure — only a change in meaning is.
//!
//! # `check --breaking`
//!
//! Default `check` fails on *any* difference. `--breaking` classifies each one
//! and fails only on a change an existing correct client can observe as a
//! regression. The rule set — a removed operation, a removed success response, a
//! narrowed type, a new required request field, a dropped enum value are
//! breaking; an added endpoint, an added optional field, an added error status
//! are not — is the same table `moso-openapi`'s `diff` module owns. The CLI
//! depends on no Moso crate (it drives the application binary over the
//! `--dump-*` protocol), so it cannot import that classifier; it re-derives the
//! same rules over the raw JSON in `breaking_changes`. Presence rules flip
//! between a request and a response, so the walk is `Position`-aware exactly
//! where the two documents read in opposite directions.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use crate::cli::{OpenapiCheckArgs, OpenapiExportArgs};
use crate::exit::{CliError, Outcome, io as io_error};
use crate::project::{Dump, Project};
use crate::ui::{Level, Ui};

/// How many differences `check` prints before it stops.
const MAX_REPORTED: usize = 20;

/// The eight HTTP methods an OpenAPI path item can carry an operation under.
const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Run `moso openapi export`.
///
/// # Errors
/// Anything the dump protocol can fail with, plus a write that is refused.
pub fn export(ui: &Ui, args: &OpenapiExportArgs) -> Outcome<()> {
    let project = Project::discover(args.app.manifest_path.as_deref())?;
    project.require_moso()?;
    let mut document = parse(&project.dump(&args.app, Dump::OpenApi)?)?;
    if let Some(prefix) = &args.prefix {
        filter_prefix(&mut document, prefix);
    }
    let rendered = render(&document, args.compact);

    let Some(out) = &args.out else {
        ui.emit_raw(&rendered);
        return Ok(());
    };

    let path = project.root.join(out);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| io_error("could not create", parent, &error))?;
    }
    std::fs::write(&path, format!("{rendered}\n"))
        .map_err(|error| io_error("could not write", &path, &error))?;

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": true,
            "path": path.display().to_string(),
            "bytes": rendered.len() + 1,
            "operations": operation_count(&document),
        }));
    } else {
        ui.status(
            Level::Ok,
            &format!("wrote {}", out.display()),
            &format!("({} operations)", operation_count(&document)),
        );
    }
    Ok(())
}

/// Run `moso openapi check`.
///
/// # Errors
/// [`Fault::User`](crate::exit::Fault::User) when the committed document is
/// missing, unparseable, or out of date — all three are things the author must
/// fix, and all three should fail a build.
pub fn check(ui: &Ui, args: &OpenapiCheckArgs) -> Outcome<()> {
    let project = Project::discover(args.app.manifest_path.as_deref())?;
    project.require_moso()?;
    let path = project.root.join(&args.path);

    let committed = read_committed(&path, &args.path)?;
    let live = parse(&project.dump(&args.app, Dump::OpenApi)?)?;

    if args.breaking {
        return check_breaking(ui, args, &path, &committed, &live);
    }

    let mut differences = Vec::new();
    diff(&committed, &live, String::new(), &mut differences);

    if differences.is_empty() {
        if ui.is_json() {
            ui.emit_json(&serde_json::json!({
                "ok": true,
                "path": path.display().to_string(),
                "differences": [],
            }));
        } else {
            ui.status(
                Level::Ok,
                &format!("{} is up to date", args.path.display()),
                &format!("({} operations)", operation_count(&live)),
            );
        }
        return Ok(());
    }

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": false,
            "path": path.display().to_string(),
            "differences": differences
                .iter()
                .take(MAX_REPORTED)
                .map(Difference::to_json)
                .collect::<Vec<_>>(),
            "total": differences.len(),
        }));
    } else {
        ui.status(
            Level::Fail,
            &format!("{} is out of date", args.path.display()),
            &format!("({} differences)", differences.len()),
        );
        for difference in differences.iter().take(MAX_REPORTED) {
            ui.line(&format!("      {difference}"));
        }
        if differences.len() > MAX_REPORTED {
            ui.line(&format!(
                "      … and {} more",
                differences.len() - MAX_REPORTED
            ));
        }
    }

    Err(
        CliError::user(format!("`{}` does not match the code", args.path.display()))
            .with_help(format!("moso openapi export --out {}", args.path.display())),
    )
}

/// The `--breaking` arm of `moso openapi check`.
///
/// Classifies each difference and fails only when at least one is breaking. A
/// document that drifted purely additively — a new optional field, a new
/// endpoint — is reported as changed-but-compatible and exits 0, so the flag is
/// what a downstream repository gates a *published* API on rather than an
/// internal freshness check.
fn check_breaking(
    ui: &Ui,
    args: &OpenapiCheckArgs,
    path: &Path,
    committed: &Value,
    live: &Value,
) -> Outcome<()> {
    let breaking = breaking_changes(committed, live);

    // The flat diff still runs, only to count the compatible differences a
    // reader is told about but not failed on.
    let mut differences = Vec::new();
    diff(committed, live, String::new(), &mut differences);
    let total = differences.len();

    if breaking.is_empty() {
        if ui.is_json() {
            ui.emit_json(&serde_json::json!({
                "ok": true,
                "path": path.display().to_string(),
                "breaking": [],
                "differences": total,
            }));
        } else if total == 0 {
            ui.status(
                Level::Ok,
                &format!("{} is up to date", args.path.display()),
                &format!("({} operations)", operation_count(live)),
            );
        } else {
            ui.status(
                Level::Ok,
                &format!("{} has no breaking changes", args.path.display()),
                &format!("({total} compatible differences)"),
            );
        }
        return Ok(());
    }

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": false,
            "path": path.display().to_string(),
            "breaking": breaking
                .iter()
                .take(MAX_REPORTED)
                .map(Breaking::to_json)
                .collect::<Vec<_>>(),
            "total": breaking.len(),
            "differences": total,
        }));
    } else {
        ui.status(
            Level::Fail,
            &format!("{} has breaking changes", args.path.display()),
            &format!("({} breaking of {total} differences)", breaking.len()),
        );
        for change in breaking.iter().take(MAX_REPORTED) {
            ui.line(&format!("      {change}"));
        }
        if breaking.len() > MAX_REPORTED {
            ui.line(&format!(
                "      … and {} more",
                breaking.len() - MAX_REPORTED
            ));
        }
    }

    Err(
        CliError::user(format!("`{}` has breaking changes", args.path.display())).with_help(
            "revert the breaking change, or version the API and export the new document with \
             `moso openapi export`",
        ),
    )
}

/// Read and parse the committed document.
fn read_committed(path: &Path, display: &Path) -> Outcome<Value> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CliError::user(format!("there is no `{}` to check", display.display())).with_help(
                format!(
                    "moso openapi export --out {} && git add {}",
                    display.display(),
                    display.display()
                ),
            )
        } else {
            io_error("could not read", path, &error)
        }
    })?;
    serde_json::from_str(&text).map_err(|error| {
        CliError::user(format!(
            "`{}` is not valid JSON: {error}",
            display.display()
        ))
        .with_help(format!("moso openapi export --out {}", display.display()))
    })
}

/// Parse what the application answered.
fn parse(answer: &str) -> Outcome<Value> {
    serde_json::from_str(answer).map_err(|error| {
        CliError::user(format!(
            "the application's `--dump-openapi` output is not JSON: {error}"
        ))
        .with_help(
            "everything except the document must go to stderr; check for a `println!` in a \
             startup hook",
        )
    })
}

/// Serialise, indented unless `compact`.
///
/// Indented by default because the usual destination is a committed file, and
/// `15-determinism` only pays off if the diff is readable.
fn render(document: &Value, compact: bool) -> String {
    if compact {
        serde_json::to_string(document)
    } else {
        serde_json::to_string_pretty(document)
    }
    .unwrap_or_else(|_| "{}".to_owned())
}

/// Keep only the paths at or under `prefix`, dropping every other operation.
///
/// The match is on segment boundaries, so `/api` keeps `/api` and
/// `/api/v1/users` but never `/apiary`; a trailing slash on the prefix is
/// ignored. Only the `paths` object is filtered — `components/schemas` is left
/// whole, because deciding which schema a surviving path still needs is
/// transitive `$ref` tracing, and an over-broad component set is valid OpenAPI
/// whereas a schema pruned by mistake is a broken document. The paths that
/// remain keep their full key; nothing is stripped. A document with no `paths`
/// object, or one whose `paths` is not an object, is left untouched.
fn filter_prefix(document: &mut Value, prefix: &str) {
    let boundary = prefix.strip_suffix('/').unwrap_or(prefix);
    if let Some(paths) = document.get_mut("paths").and_then(Value::as_object_mut) {
        paths.retain(|path, _| under_prefix(path, boundary));
    }
}

/// Whether `path` is the prefix itself or lies below it on a segment boundary.
fn under_prefix(path: &str, boundary: &str) -> bool {
    path == boundary
        || path
            .strip_prefix(boundary)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// How many operations the document describes.
fn operation_count(document: &Value) -> usize {
    document
        .get("paths")
        .and_then(Value::as_object)
        .map(|paths| {
            paths
                .values()
                .filter_map(Value::as_object)
                .map(|item| {
                    item.keys()
                        .filter(|key| METHODS.contains(&key.as_str()))
                        .count()
                })
                .sum()
        })
        .unwrap_or(0)
}

/// One place the two documents disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    /// An RFC 6901 JSON Pointer into the document.
    pub pointer: String,
    /// What kind of change it is.
    pub change: Change,
}

/// The three ways two JSON documents can differ at one pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Present in the code, absent from the committed file.
    Added,
    /// Present in the committed file, absent from the code.
    Removed,
    /// Present in both, with different values.
    Changed,
}

impl Change {
    /// A stable machine-readable name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Removed => "removed",
            Change::Changed => "changed",
        }
    }
}

impl Difference {
    /// The `--json` rendering.
    fn to_json(&self) -> Value {
        serde_json::json!({ "pointer": self.pointer, "change": self.change.as_str() })
    }
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pointer = if self.pointer.is_empty() {
            "/"
        } else {
            &self.pointer
        };
        write!(f, "{:<9} {pointer}", self.change.as_str())
    }
}

/// Collect every pointer at which `committed` and `live` disagree.
///
/// Object key order is not a difference — `serde_json`'s maps compare
/// order-insensitively — but array order is, because an OpenAPI `parameters`
/// list in a different order is a different document to a code generator.
fn diff(committed: &Value, live: &Value, pointer: String, out: &mut Vec<Difference>) {
    match (committed, live) {
        (Value::Object(left), Value::Object(right)) => {
            for (key, value) in left {
                let child = format!("{pointer}/{}", escape(key));
                match right.get(key) {
                    Some(other) => diff(value, other, child, out),
                    None => out.push(Difference {
                        pointer: child,
                        change: Change::Removed,
                    }),
                }
            }
            for key in right.keys() {
                if !left.contains_key(key) {
                    out.push(Difference {
                        pointer: format!("{pointer}/{}", escape(key)),
                        change: Change::Added,
                    });
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            for (index, value) in left.iter().enumerate() {
                let child = format!("{pointer}/{index}");
                match right.get(index) {
                    Some(other) => diff(value, other, child, out),
                    None => out.push(Difference {
                        pointer: child,
                        change: Change::Removed,
                    }),
                }
            }
            for index in left.len()..right.len() {
                out.push(Difference {
                    pointer: format!("{pointer}/{index}"),
                    change: Change::Added,
                });
            }
        }
        (left, right) if left != right => out.push(Difference {
            pointer,
            change: Change::Changed,
        }),
        _ => {}
    }
}

/// RFC 6901 escaping: `~` becomes `~0`, `/` becomes `~1`.
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

// ---------------------------------------------------------------------------
// Breaking-change classification (`--breaking`)
// ---------------------------------------------------------------------------

/// How deep into a schema tree the walk goes before giving up.
///
/// Reached only by a pathologically nested inline schema; every recursive Moso
/// type goes through a `$ref`, which the cycle guard handles instead.
const MAX_SCHEMA_DEPTH: usize = 24;

/// How many `$ref` hops are followed before a chain is assumed to be a cycle.
const MAX_REF_HOPS: usize = 8;

/// One difference that can stop an existing, correct client from working.
///
/// `check` in its default mode fails on any drift; the [`breaking_changes`]
/// walk narrows that to what a client observes as a regression.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Breaking {
    /// An RFC 6901 JSON Pointer at the change, into whichever document still
    /// contains it — the live document for an addition, the committed one for a
    /// removal. A parameter or media type, which JSON addresses only by array
    /// index, is located by a trailing `(…)` note instead.
    pointer: String,
    /// One line naming why an existing client breaks.
    reason: String,
}

impl Breaking {
    /// The `--json` rendering.
    fn to_json(&self) -> Value {
        serde_json::json!({ "pointer": self.pointer, "reason": self.reason })
    }
}

impl std::fmt::Display for Breaking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}  {}", self.pointer, self.reason)
    }
}

/// Which side of the wire a schema sits on.
///
/// Presence rules flip between the two: a request tolerates a *removed* field
/// and breaks on a new *required* one, while a response is the mirror image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    /// A request body, header, or parameter — read by the server.
    Request,
    /// A response body or header — read by the client.
    Response,
}

/// Every breaking difference between the `committed` document and the `live`
/// one the application currently produces.
///
/// The rules mirror the table in `moso-openapi`'s `diff` module; see the module
/// header for why they are re-derived here rather than imported.
fn breaking_changes(committed: &Value, live: &Value) -> Vec<Breaking> {
    let mut classifier = Classifier {
        committed,
        live,
        out: Vec::new(),
    };
    classifier.paths();
    classifier.out
}

/// Walks two documents and records only the breaking differences.
struct Classifier<'a> {
    committed: &'a Value,
    live: &'a Value,
    out: Vec<Breaking>,
}

impl<'a> Classifier<'a> {
    /// Record one breaking change.
    fn record(&mut self, pointer: String, reason: impl Into<String>) {
        self.out.push(Breaking {
            pointer,
            reason: reason.into(),
        });
    }

    /// Walk every path, comparing operations under each.
    fn paths(&mut self) {
        let (Some(old), Some(new)) = (
            self.committed.get("paths").and_then(Value::as_object),
            self.live.get("paths").and_then(Value::as_object),
        ) else {
            return;
        };
        for (path, old_item) in old {
            let base = format!("/paths/{}", escape(path));
            match new.get(path) {
                None => self.operations_removed(&base, old_item),
                Some(new_item) => self.path_item(&base, old_item, new_item),
            }
        }
    }

    /// A whole path is gone: every operation under it is removed.
    fn operations_removed(&mut self, base: &str, item: &'a Value) {
        for method in METHODS {
            if item.get(method).is_some() {
                self.record(
                    format!("{base}/{method}"),
                    format!(
                        "operation `{}` removed; a client still calling it gets 404",
                        method.to_uppercase()
                    ),
                );
            }
        }
    }

    /// Compare the operations of one path that exists in both documents.
    fn path_item(&mut self, base: &str, old: &'a Value, new: &'a Value) {
        for method in METHODS {
            let pointer = format!("{base}/{method}");
            match (old.get(method), new.get(method)) {
                (Some(_), None) => self.record(
                    pointer,
                    format!(
                        "operation `{}` removed; a client still calling it gets 404",
                        method.to_uppercase()
                    ),
                ),
                (Some(old_op), Some(new_op)) => self.operation(&pointer, old_op, new_op),
                _ => {}
            }
        }
    }

    /// Compare one operation: its parameters, request body and responses.
    fn operation(&mut self, base: &str, old: &'a Value, new: &'a Value) {
        self.parameters(base, old, new);
        self.request_body(base, old, new);
        self.responses(base, old, new);
    }

    // ── parameters ──────────────────────────────────────────────────────

    /// A new required parameter, or one that became required, or one whose
    /// schema narrowed, is breaking.
    fn parameters(&mut self, base: &str, old_op: &'a Value, new_op: &'a Value) {
        let old = old_op.get("parameters").and_then(Value::as_array);
        let new = new_op.get("parameters").and_then(Value::as_array);
        let key = |parameter: &Value| {
            (
                parameter
                    .get("in")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                parameter
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            )
        };

        for parameter in new.into_iter().flatten() {
            let (location, name) = key(parameter);
            let previous = old
                .into_iter()
                .flatten()
                .find(|other| key(other) == (location.clone(), name.clone()));
            let pointer = format!("{base}/parameters ({location} `{name}`)");
            match previous {
                None if required_flag(parameter) => self.record(
                    pointer,
                    format!(
                        "new required {location} parameter `{name}`; a client that omits it is \
                         rejected"
                    ),
                ),
                None => {}
                Some(old_param) => {
                    if !required_flag(old_param) && required_flag(parameter) {
                        self.record(
                            pointer.clone(),
                            format!(
                                "parameter `{name}` is now required; a client that omits it is \
                                 rejected"
                            ),
                        );
                    }
                    if let (Some(old_schema), Some(new_schema)) =
                        (old_param.get("schema"), parameter.get("schema"))
                    {
                        let mut seen = Vec::new();
                        self.schema(
                            &pointer,
                            old_schema,
                            new_schema,
                            Position::Request,
                            &mut seen,
                            0,
                        );
                    }
                }
            }
        }
    }

    // ── request body and responses ──────────────────────────────────────

    /// A request body that appeared and is required, or that became required,
    /// is breaking; its schema is walked in [`Position::Request`].
    fn request_body(&mut self, base: &str, old_op: &'a Value, new_op: &'a Value) {
        let pointer = format!("{base}/requestBody");
        match (old_op.get("requestBody"), new_op.get("requestBody")) {
            (None, Some(body)) if required_flag(body) => self.record(
                pointer,
                "a required request body was added; a client sending none is rejected",
            ),
            (Some(old_body), Some(new_body)) => {
                if !required_flag(old_body) && required_flag(new_body) {
                    self.record(
                        pointer.clone(),
                        "request body is now required; a client sending none is rejected",
                    );
                }
                self.content(&pointer, old_body, new_body, Position::Request);
            }
            _ => {}
        }
    }

    /// A removed success response is breaking; a shared one has its body walked
    /// in [`Position::Response`]. An added or removed error status is not.
    fn responses(&mut self, base: &str, old_op: &'a Value, new_op: &'a Value) {
        let (Some(old), Some(new)) = (
            old_op.get("responses").and_then(Value::as_object),
            new_op.get("responses").and_then(Value::as_object),
        ) else {
            return;
        };
        for (status, old_response) in old {
            let pointer = format!("{base}/responses/{}", escape(status));
            match new.get(status) {
                None if is_success(status) => self.record(
                    pointer,
                    format!(
                        "success response `{status}` removed; a client that relies on it breaks"
                    ),
                ),
                None => {}
                Some(new_response) => {
                    self.content(&pointer, old_response, new_response, Position::Response);
                }
            }
        }
    }

    /// Walk the schema behind each media type a body offers.
    fn content(&mut self, base: &str, old: &'a Value, new: &'a Value, position: Position) {
        let (Some(old), Some(new)) = (
            old.get("content").and_then(Value::as_object),
            new.get("content").and_then(Value::as_object),
        ) else {
            return;
        };
        for (media_type, old_media) in old {
            let Some(new_media) = new.get(media_type) else {
                self.record(
                    format!("{base}/content ({media_type})"),
                    format!("`{media_type}` is no longer offered"),
                );
                continue;
            };
            if let (Some(old_schema), Some(new_schema)) =
                (old_media.get("schema"), new_media.get("schema"))
            {
                let pointer = format!("{base}/content/{}/schema", escape(media_type));
                let mut seen = Vec::new();
                self.schema(&pointer, old_schema, new_schema, position, &mut seen, 0);
            }
        }
    }

    // ── schemas ─────────────────────────────────────────────────────────

    /// Resolve any `$ref` on either side, guard against a reference cycle, then
    /// compare the two schema nodes.
    fn schema(
        &mut self,
        base: &str,
        old: &'a Value,
        new: &'a Value,
        position: Position,
        seen: &mut Vec<(String, String)>,
        depth: usize,
    ) {
        if depth > MAX_SCHEMA_DEPTH {
            return;
        }
        let key = (
            ref_of(old).unwrap_or_default(),
            ref_of(new).unwrap_or_default(),
        );
        let guarded = !key.0.is_empty() || !key.1.is_empty();
        if guarded {
            if seen.contains(&key) {
                return;
            }
            seen.push(key);
        }
        let old = resolve(old, self.committed);
        let new = resolve(new, self.live);
        self.compare_schema(base, old, new, position, seen, depth);
        if guarded {
            seen.pop();
        }
    }

    /// The keyword-by-keyword comparison of two resolved schema nodes.
    fn compare_schema(
        &mut self,
        base: &str,
        old: &'a Value,
        new: &'a Value,
        position: Position,
        seen: &mut Vec<(String, String)>,
        depth: usize,
    ) {
        let old_types = type_set(old);
        let new_types = type_set(new);
        if old_types != new_types && is_narrowing(&old_types, &new_types) {
            self.record(
                format!("{base}/type"),
                format!(
                    "type narrowed {} → {}; a value the old type allowed is now rejected",
                    show_types(&old_types),
                    show_types(&new_types)
                ),
            );
        }

        if let (Some(old_enum), Some(new_enum)) = (
            old.get("enum").and_then(Value::as_array),
            new.get("enum").and_then(Value::as_array),
        ) {
            let lost: Vec<String> = old_enum
                .iter()
                .filter(|value| !new_enum.contains(value))
                .map(compact_value)
                .collect();
            if !lost.is_empty() {
                self.record(
                    format!("{base}/enum"),
                    format!("enum no longer accepts {}", lost.join(", ")),
                );
            }
        }

        self.properties(base, old, new, position, seen, depth);

        if let (Some(old_items), Some(new_items)) = (old.get("items"), new.get("items")) {
            self.schema(
                &format!("{base}/items"),
                old_items,
                new_items,
                position,
                seen,
                depth + 1,
            );
        }
    }

    /// Compare an object's properties and their requiredness.
    fn properties(
        &mut self,
        base: &str,
        old: &'a Value,
        new: &'a Value,
        position: Position,
        seen: &mut Vec<(String, String)>,
        depth: usize,
    ) {
        let old_props = old.get("properties").and_then(Value::as_object);
        let new_props = new.get("properties").and_then(Value::as_object);
        let old_required = required_names(old);
        let new_required = required_names(new);

        for (name, old_schema) in old_props.into_iter().flatten() {
            let pointer = format!("{base}/properties/{}", escape(name));
            match new_props.and_then(|props| props.get(name)) {
                None => {
                    if position == Position::Response {
                        self.record(
                            pointer,
                            format!(
                                "response field `{name}` removed; a client that reads it breaks"
                            ),
                        );
                    }
                }
                Some(new_schema) => {
                    let was_required = old_required.contains(name);
                    let is_required = new_required.contains(name);
                    if position == Position::Request && !was_required && is_required {
                        self.record(
                            pointer.clone(),
                            format!(
                                "field `{name}` is now required; a client that omits it is rejected"
                            ),
                        );
                    }
                    if position == Position::Response && was_required && !is_required {
                        self.record(
                            pointer.clone(),
                            format!(
                                "response field `{name}` may now be absent; a client that assumes \
                                 it breaks"
                            ),
                        );
                    }
                    self.schema(&pointer, old_schema, new_schema, position, seen, depth + 1);
                }
            }
        }

        for name in new_props.into_iter().flatten().map(|(name, _)| name) {
            let already = old_props.is_some_and(|props| props.contains_key(name));
            if !already && position == Position::Request && new_required.contains(name) {
                self.record(
                    format!("{base}/properties/{}", escape(name)),
                    format!(
                        "new required request field `{name}`; a client that omits it is rejected"
                    ),
                );
            }
        }
    }
}

/// Whether a body or parameter object carries `"required": true`.
fn required_flag(value: &Value) -> bool {
    value
        .get("required")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The set of property names a schema lists as required.
fn required_names(schema: &Value) -> BTreeSet<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The declared type(s) of a schema, as a set. OpenAPI 3.1 allows `type` to be
/// a single string or an array; a missing `type` is the empty set, meaning
/// "any".
fn type_set(schema: &Value) -> BTreeSet<String> {
    match schema.get("type") {
        Some(Value::String(one)) => std::iter::once(one.clone()).collect(),
        Some(Value::Array(many)) => many
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => BTreeSet::new(),
    }
}

/// Whether the move from `old` to `new` types is a narrowing.
///
/// An empty set is "any": constraining it is narrowing, relaxing back to it is
/// widening. Otherwise a strict-subset move narrows, as does `number` →
/// `integer`. Mirrors `moso-openapi`'s `is_narrowing`.
fn is_narrowing(old: &BTreeSet<String>, new: &BTreeSet<String>) -> bool {
    if old.is_empty() || new.is_empty() {
        return old.is_empty() && !new.is_empty();
    }
    (new.is_subset(old) && new.len() < old.len())
        || (old.contains("number") && new.contains("integer") && !new.contains("number"))
}

/// Render a type set for a message: `any` when empty, else `a|b`.
fn show_types(types: &BTreeSet<String>) -> String {
    if types.is_empty() {
        return "any".to_owned();
    }
    types.iter().cloned().collect::<Vec<_>>().join("|")
}

/// A compact one-line rendering of a JSON value for a message.
fn compact_value(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "?".to_owned())
}

/// Whether a response status key is a 2xx success (`"2XX"` included).
fn is_success(status: &str) -> bool {
    status.starts_with('2') && (status.parse::<u16>().is_ok() || status.eq_ignore_ascii_case("2xx"))
}

/// The `$ref` string of a schema node, if it is a reference.
fn ref_of(schema: &Value) -> Option<String> {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Follow an internal `#/…` `$ref` into its document, up to [`MAX_REF_HOPS`].
///
/// A reference that points outside the document, or that cannot be resolved, is
/// returned unchanged: an unresolvable `$ref` is not something this command can
/// classify, and inventing a breaking change from it would be dishonest.
fn resolve<'a>(node: &'a Value, document: &'a Value) -> &'a Value {
    let mut current = node;
    for _ in 0..MAX_REF_HOPS {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            return current;
        };
        let Some(rest) = reference.strip_prefix("#/") else {
            return current;
        };
        let mut cursor = document;
        for segment in rest.split('/') {
            let segment = segment.replace("~1", "/").replace("~0", "~");
            match cursor.get(&segment) {
                Some(next) => cursor = next,
                None => return current,
            }
        }
        current = cursor;
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn differences(committed: &Value, live: &Value) -> Vec<Difference> {
        let mut out = Vec::new();
        diff(committed, live, String::new(), &mut out);
        out
    }

    #[test]
    fn identical_documents_have_no_differences() {
        let document = json!({"openapi": "3.1.0", "paths": {"/users": {"get": {}}}});
        assert!(differences(&document, &document).is_empty());
    }

    #[test]
    fn key_order_is_not_a_difference() {
        let a: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).expect("json");
        let b: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).expect("json");
        assert!(differences(&a, &b).is_empty());
    }

    #[test]
    fn array_order_is_a_difference() {
        let a = json!({"tags": ["a", "b"]});
        let b = json!({"tags": ["b", "a"]});
        assert_eq!(differences(&a, &b).len(), 2);
    }

    #[test]
    fn a_new_operation_is_reported_as_added_at_its_pointer() {
        let committed = json!({"paths": {"/users": {"get": {}}}});
        let live = json!({"paths": {"/users": {"get": {}, "post": {}}}});
        let found = differences(&committed, &live);
        assert_eq!(
            found,
            vec![Difference {
                pointer: "/paths/~1users/post".to_owned(),
                change: Change::Added,
            }]
        );
    }

    #[test]
    fn a_removed_field_is_reported_as_removed() {
        let committed = json!({"info": {"title": "shop", "version": "1"}});
        let live = json!({"info": {"title": "shop"}});
        let found = differences(&committed, &live);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].change, Change::Removed);
        assert_eq!(found[0].pointer, "/info/version");
    }

    #[test]
    fn a_changed_scalar_is_reported_at_the_leaf() {
        let committed = json!({"info": {"version": "1"}});
        let live = json!({"info": {"version": "2"}});
        assert_eq!(
            differences(&committed, &live),
            vec![Difference {
                pointer: "/info/version".to_owned(),
                change: Change::Changed,
            }]
        );
    }

    #[test]
    fn a_slash_in_a_key_is_escaped_as_rfc_6901_asks() {
        assert_eq!(escape("/users/{id}"), "~1users~1{id}");
        assert_eq!(escape("a~b"), "a~0b");
        assert_eq!(escape("~/"), "~0~1");
    }

    #[test]
    fn a_type_change_is_one_difference_not_a_tree_of_them() {
        let committed = json!({"x": {"a": 1}});
        let live = json!({"x": [1, 2, 3]});
        assert_eq!(differences(&committed, &live).len(), 1);
    }

    #[test]
    fn operations_are_counted_across_paths_and_methods() {
        let document = json!({
            "paths": {
                "/users": {"get": {}, "post": {}, "parameters": []},
                "/users/{id}": {"get": {}, "delete": {}},
            }
        });
        assert_eq!(operation_count(&document), 4);
        assert_eq!(operation_count(&json!({})), 0);
    }

    #[test]
    fn compact_and_pretty_carry_the_same_document() {
        let document = json!({"openapi": "3.1.0"});
        let compact = render(&document, true);
        let pretty = render(&document, false);
        assert!(!compact.contains('\n'));
        assert!(pretty.contains('\n'));
        assert_eq!(
            serde_json::from_str::<Value>(&compact).expect("json"),
            serde_json::from_str::<Value>(&pretty).expect("json")
        );
    }

    #[test]
    fn a_difference_prints_its_pointer() {
        let difference = Difference {
            pointer: "/paths/~1users/get".to_owned(),
            change: Change::Added,
        };
        assert!(difference.to_string().contains("/paths/~1users/get"));
        assert_eq!(difference.to_json()["change"], json!("added"));
    }

    // ── prefix filtering ────────────────────────────────────────────────

    /// The path keys of a document, sorted, for order-insensitive assertions.
    fn path_keys(document: &Value) -> Vec<String> {
        let mut keys: Vec<String> = document["paths"]
            .as_object()
            .expect("paths object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn a_prefix_keeps_only_the_paths_under_it() {
        let mut document = json!({"paths": {
            "/api/users": {"get": {}},
            "/api/posts": {"get": {}},
            "/health": {"get": {}},
            "/metrics": {"get": {}},
        }});
        filter_prefix(&mut document, "/api");
        assert_eq!(path_keys(&document), vec!["/api/posts", "/api/users"]);
    }

    #[test]
    fn a_prefix_matches_on_segment_boundaries_not_characters() {
        let mut document = json!({"paths": {
            "/api": {"get": {}},
            "/api/v1": {"get": {}},
            "/apiary": {"get": {}},
        }});
        filter_prefix(&mut document, "/api");
        // `/api` and `/api/v1` survive; `/apiary` is a different resource.
        assert_eq!(path_keys(&document), vec!["/api", "/api/v1"]);
    }

    #[test]
    fn a_trailing_slash_on_the_prefix_is_ignored() {
        let mut document = json!({"paths": {
            "/api/users": {"get": {}},
            "/health": {"get": {}},
        }});
        filter_prefix(&mut document, "/api/");
        assert_eq!(path_keys(&document), vec!["/api/users"]);
    }

    #[test]
    fn the_kept_paths_are_not_stripped_and_components_are_left_whole() {
        let mut document = json!({
            "paths": {"/api/users": {"get": {}}, "/health": {"get": {}}},
            "components": {"schemas": {"User": {"type": "object"}, "Health": {"type": "object"}}},
        });
        filter_prefix(&mut document, "/api");
        assert_eq!(path_keys(&document), vec!["/api/users"]);
        // The surviving path keeps its full key, and reference tracing is not
        // attempted, so both schemas remain even though `Health` is now unused.
        assert!(document["components"]["schemas"].get("User").is_some());
        assert!(document["components"]["schemas"].get("Health").is_some());
    }

    #[test]
    fn a_prefix_matching_nothing_leaves_an_empty_paths_object() {
        let mut document = json!({"paths": {"/health": {"get": {}}}});
        filter_prefix(&mut document, "/api");
        assert_eq!(operation_count(&document), 0);
        assert!(document["paths"].as_object().expect("paths").is_empty());
    }

    #[test]
    fn a_document_without_paths_is_left_untouched() {
        let mut document = json!({"openapi": "3.1.0", "info": {"title": "shop"}});
        let before = document.clone();
        filter_prefix(&mut document, "/api");
        assert_eq!(document, before);
    }

    // ── breaking-change classification ──────────────────────────────────

    fn reasons(committed: &Value, live: &Value) -> Vec<String> {
        breaking_changes(committed, live)
            .into_iter()
            .map(|change| change.reason)
            .collect()
    }

    /// A one-operation document whose `GET /r` 200 response body is `schema`.
    fn response_doc(schema: Value) -> Value {
        json!({"paths": {"/r": {"get": {"responses": {"200":
            {"content": {"application/json": {"schema": schema}}}}}}}})
    }

    /// A one-operation document whose `POST /r` request body is `schema`.
    fn request_doc(schema: Value) -> Value {
        json!({"paths": {"/r": {"post": {"requestBody":
            {"content": {"application/json": {"schema": schema}}}}}}})
    }

    #[test]
    fn a_removed_path_is_breaking() {
        let committed = json!({"paths": {"/users": {"get": {}}, "/legacy": {"get": {}}}});
        let live = json!({"paths": {"/users": {"get": {}}}});
        let found = breaking_changes(&committed, &live);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pointer, "/paths/~1legacy/get");
        assert!(found[0].reason.contains("removed"));
    }

    #[test]
    fn a_removed_operation_on_a_kept_path_is_breaking() {
        let committed = json!({"paths": {"/users": {"get": {}, "post": {}}}});
        let live = json!({"paths": {"/users": {"get": {}}}});
        let found = breaking_changes(&committed, &live);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pointer, "/paths/~1users/post");
    }

    #[test]
    fn an_added_operation_is_not_breaking() {
        let committed = json!({"paths": {"/users": {"get": {}}}});
        let live = json!({"paths": {"/users": {"get": {}, "post": {}}}});
        assert!(breaking_changes(&committed, &live).is_empty());
    }

    #[test]
    fn an_added_optional_response_field_is_not_breaking() {
        let committed = response_doc(json!({"type": "object",
            "properties": {"id": {"type": "string"}}}));
        let live = response_doc(json!({"type": "object",
            "properties": {"id": {"type": "string"}, "nickname": {"type": "string"}}}));
        assert!(breaking_changes(&committed, &live).is_empty());
    }

    #[test]
    fn a_removed_response_field_is_breaking() {
        let committed = response_doc(json!({"type": "object",
            "properties": {"id": {"type": "string"}, "email": {"type": "string"}}}));
        let live = response_doc(json!({"type": "object",
            "properties": {"id": {"type": "string"}}}));
        let found = reasons(&committed, &live);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("`email`"), "{found:?}");
    }

    #[test]
    fn a_removed_success_response_is_breaking_but_a_removed_error_is_not() {
        let committed = json!({"paths": {"/users": {"post": {"responses": {
            "201": {}, "409": {}}}}}});
        let live = json!({"paths": {"/users": {"post": {"responses": {}}}}});
        let found = breaking_changes(&committed, &live);
        assert_eq!(found.len(), 1);
        assert!(found[0].reason.contains("201"));
    }

    #[test]
    fn a_new_required_request_field_is_breaking() {
        let committed = request_doc(json!({"type": "object",
            "properties": {"name": {"type": "string"}}, "required": ["name"]}));
        let live = request_doc(json!({"type": "object",
            "properties": {"name": {"type": "string"}, "team": {"type": "string"}},
            "required": ["name", "team"]}));
        let found = reasons(&committed, &live);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("`team`"), "{found:?}");
    }

    #[test]
    fn a_new_optional_request_field_is_not_breaking() {
        let committed = request_doc(json!({"type": "object",
            "properties": {"name": {"type": "string"}}, "required": ["name"]}));
        let live = request_doc(json!({"type": "object",
            "properties": {"name": {"type": "string"}, "team": {"type": "string"}},
            "required": ["name"]}));
        assert!(breaking_changes(&committed, &live).is_empty());
    }

    #[test]
    fn a_request_field_becoming_required_is_breaking() {
        let committed = request_doc(json!({"type": "object",
            "properties": {"name": {"type": "string"}}, "required": []}));
        let live = request_doc(json!({"type": "object",
            "properties": {"name": {"type": "string"}}, "required": ["name"]}));
        let found = reasons(&committed, &live);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("now required"), "{found:?}");
    }

    #[test]
    fn a_narrowed_type_is_breaking() {
        let committed = request_doc(json!({"type": "object",
            "properties": {"count": {"type": "number"}}}));
        let live = request_doc(json!({"type": "object",
            "properties": {"count": {"type": "integer"}}}));
        let found = reasons(&committed, &live);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("narrowed"), "{found:?}");
    }

    #[test]
    fn a_dropped_enum_value_is_breaking() {
        let committed = request_doc(json!({"type": "object", "properties":
            {"role": {"type": "string", "enum": ["admin", "user", "guest"]}}}));
        let live = request_doc(json!({"type": "object", "properties":
            {"role": {"type": "string", "enum": ["admin", "user"]}}}));
        let found = reasons(&committed, &live);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("guest"), "{found:?}");
    }

    #[test]
    fn a_new_required_query_parameter_is_breaking() {
        let committed = json!({"paths": {"/users": {"get": {"parameters": []}}}});
        let live = json!({"paths": {"/users": {"get": {"parameters": [
            {"name": "since", "in": "query", "required": true}]}}}});
        let found = reasons(&committed, &live);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("since"), "{found:?}");
    }

    #[test]
    fn a_new_optional_query_parameter_is_not_breaking() {
        let committed = json!({"paths": {"/users": {"get": {"parameters": []}}}});
        let live = json!({"paths": {"/users": {"get": {"parameters": [
            {"name": "since", "in": "query", "required": false}]}}}});
        assert!(breaking_changes(&committed, &live).is_empty());
    }

    #[test]
    fn a_ref_to_a_narrowed_component_is_followed() {
        let make = |inner: &str| {
            json!({
                "paths": {"/r": {"post": {"requestBody": {"content": {"application/json":
                    {"schema": {"$ref": "#/components/schemas/Body"}}}}}}},
                "components": {"schemas": {"Body": {"type": "object",
                    "properties": {"count": {"type": inner}}}}}
            })
        };
        let found = reasons(&make("number"), &make("integer"));
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("narrowed"), "{found:?}");
    }

    #[test]
    fn a_self_referential_schema_terminates() {
        let document = json!({
            "paths": {"/r": {"post": {"requestBody": {"content": {"application/json":
                {"schema": {"$ref": "#/components/schemas/Node"}}}}}}},
            "components": {"schemas": {"Node": {"type": "object",
                "properties": {"next": {"$ref": "#/components/schemas/Node"}}}}}
        });
        // Comparing a cyclic document to itself must not loop forever.
        assert!(breaking_changes(&document, &document).is_empty());
    }

    #[test]
    fn is_narrowing_covers_the_documented_cases() {
        let number: BTreeSet<String> = ["number".to_owned()].into_iter().collect();
        let integer: BTreeSet<String> = ["integer".to_owned()].into_iter().collect();
        assert!(is_narrowing(&number, &integer));
        assert!(!is_narrowing(&integer, &number));

        let any = BTreeSet::new();
        assert!(is_narrowing(&any, &integer));
        assert!(!is_narrowing(&integer, &any));

        let both: BTreeSet<String> = ["string".to_owned(), "null".to_owned()]
            .into_iter()
            .collect();
        let just: BTreeSet<String> = ["string".to_owned()].into_iter().collect();
        assert!(is_narrowing(&both, &just));
        assert!(!is_narrowing(&just, &both));
    }

    #[test]
    fn a_breaking_change_renders_its_pointer_and_reason() {
        let change = Breaking {
            pointer: "/paths/~1legacy/get".to_owned(),
            reason: "operation `GET` removed".to_owned(),
        };
        assert!(change.to_string().contains("/paths/~1legacy/get"));
        assert_eq!(change.to_json()["reason"], json!("operation `GET` removed"));
    }
}
