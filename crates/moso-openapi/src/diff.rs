//! Document diffing, the engine behind `moso openapi check`.
//!
//! The generated document is committed to the repository and CI fails on drift.
//! That is worth doing for its own sake, but the real payoff is that code review
//! gets a readable view of API changes:
//!
//! ```text
//! ✗ openapi.json is out of date
//!
//!   + POST /users/{id}/deactivate      (added in src/routes/users.rs:102)
//!   ~ GET /users                        parameter `limit` maximum 100 → 200
//!   - GET /legacy/users                 (removed)
//!
//!   run `moso openapi export` to update, and review the diff before committing
//! ```
//!
//! That block is [`ChangeReport`]'s [`Display`](core::fmt::Display); the change
//! lines on their own are [`format_changes`].
//!
//! # What counts as breaking
//!
//! A change is breaking when an existing, correct client can stop working
//! because of it. The rule set is deliberately in-repo and small enough to
//! argue with:
//!
//! | Breaking | Not breaking |
//! | --- | --- |
//! | an operation is removed | an operation is added |
//! | a required request field or parameter is added | an optional one is added |
//! | a request field becomes required | a required field becomes optional |
//! | a response field is removed | a response field is added |
//! | a type is narrowed (`number` → `integer`, a variant leaves a `oneOf`) | a type is widened |
//! | a constraint is tightened (`maxLength` down, `minimum` up, `enum` shrinks) | a constraint is relaxed |
//! | a security requirement is added or gains scopes | a requirement is removed |
//! | a success status is removed | an error status is added |
//!
//! Two of those rules read in opposite directions depending on where the schema
//! sits, and the implementation is position-aware about exactly those two:
//!
//! | | request body / parameter | response body / header |
//! | --- | --- | --- |
//! | a field is **added** | breaking only if required | never breaking |
//! | a field is **removed** | never breaking | breaking |
//! | a field **becomes required** | breaking | not breaking |
//! | a field **becomes optional** | not breaking | breaking |
//!
//! Everything else — narrowing, tightening — is classified the same way in both
//! positions, because that is what the table above promises and because a rule
//! set people cannot recite is a rule set people ignore.
//!
//! Descriptions, summaries, examples and `x-*` extensions are compared but are
//! never breaking, and [`DiffOptions`] can suppress them entirely so that a
//! doc-comment edit does not bury a real change. `x-source`, the source
//! location `#[endpoint]` records, is never diffed at all: it changes whenever
//! a file's line numbers shift, which is not an API change.
//!
//! # Determinism
//!
//! Changes come out in a walk order that does not depend on either document's
//! map ordering: `info`, servers, document-level security, tags, then paths
//! sorted lexicographically, each path's methods in [`HttpMethod::ALL`] order,
//! and within an operation a fixed member order. The output of `moso openapi
//! check` is therefore itself diffable.

use core::fmt;

use indexmap::IndexMap;
use serde_json::{Number, Value};

use crate::COMPONENTS_SCHEMAS_PREFIX;
use crate::document::{Document, response_key_rank};
use crate::path::{
    Header, HttpMethod, MediaType, Operation, Parameter, ParameterLocation, PathItem, RequestBody,
    Response,
};
use crate::security::{SecurityRequirement, SecurityScheme};
use moso_schema::json_schema::{AdditionalProperties, SchemaNode, TypeSet};

/// The extension `#[endpoint]` records a source location in.
///
/// Read to annotate an added operation, never diffed: it moves whenever a file
/// does.
const SOURCE_EXTENSION: &str = "x-source";

/// How deep into a schema tree the comparison walks before giving up.
///
/// Reached only by a pathologically nested inline schema; every recursive Moso
/// type goes through a `$ref`, which the cycle guard handles instead.
const MAX_SCHEMA_DEPTH: usize = 24;

/// How many `$ref` hops are followed before a chain is assumed to be a cycle.
const MAX_REF_HOPS: usize = 8;

/// How long a value may be before it is elided in a change detail.
const BRIEF_LEN: usize = 60;

/// What kind of change a [`Change`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// Present in the new document, absent from the old.
    Added,
    /// Present in the old document, absent from the new.
    Removed,
    /// Present in both, but different.
    Modified,
}

impl ChangeKind {
    /// The single character used to introduce this kind in CLI output:
    /// `+`, `-` or `~`.
    pub const fn symbol(self) -> char {
        match self {
            ChangeKind::Added => '+',
            ChangeKind::Removed => '-',
            ChangeKind::Modified => '~',
        }
    }
}

impl fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ChangeKind::Added => "added",
            ChangeKind::Removed => "removed",
            ChangeKind::Modified => "changed",
        })
    }
}

/// One difference between two documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Whether the subject was added, removed or modified.
    pub kind: ChangeKind,
    /// **Which route or component** the change is in, in a human-readable
    /// location language rather than a JSON pointer: `GET /users`,
    /// `webhook `order.paid` POST`, `components.schemas.UserOut`, `info`.
    ///
    /// Deliberately no finer than that. The sub-location — which parameter,
    /// which property, which status — lives at the front of
    /// [`detail`](Change::detail) instead, so that the CLI can align every
    /// change against one narrow column.
    pub path: String,
    /// What changed, phrased for a reviewer and prefixed with the sub-location:
    /// ``parameter `limit` maximum 100 → 200``, ``property `email` removed``.
    pub detail: String,
    /// Whether an existing correct client can break because of this change.
    pub breaking: bool,
}

impl Change {
    /// A non-breaking change.
    pub fn new(kind: ChangeKind, path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.into(),
            detail: detail.into(),
            breaking: false,
        }
    }

    /// Mark this change as breaking.
    pub fn breaking(mut self) -> Self {
        self.breaking = true;
        self
    }
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.kind.symbol(), self.path)?;
        if !self.detail.is_empty() {
            write!(f, "  {}", self.detail)?;
        }
        Ok(())
    }
}

/// Which categories of difference to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffOptions {
    /// Report changes to summaries and descriptions. Never breaking.
    pub include_descriptions: bool,
    /// Report changes to examples. Never breaking.
    pub include_examples: bool,
    /// Report changes to `x-*` specification extensions.
    pub include_extensions: bool,
    /// Report only the changes classified as breaking.
    pub breaking_only: bool,
    /// Follow `$ref`s into `components.schemas` rather than comparing the
    /// reference strings.
    ///
    /// On by default: a schema that changed shape without changing name is
    /// exactly the drift worth catching. With it off, `components.schemas` is
    /// diffed as a section of its own instead, so nothing goes unreported —
    /// but a change is then attributed to the schema rather than to every
    /// operation that uses it.
    pub resolve_refs: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            include_descriptions: true,
            include_examples: false,
            include_extensions: true,
            breaking_only: false,
            resolve_refs: true,
        }
    }
}

impl DiffOptions {
    /// Only structural differences: no prose, no examples, no extensions.
    pub fn structural() -> Self {
        Self {
            include_descriptions: false,
            include_examples: false,
            include_extensions: false,
            ..Self::default()
        }
    }
}

/// Compare two documents with the default options.
///
/// Changes are returned in a stable order — by path, then by method, then by
/// the member within the operation — so the CLI output is diffable itself.
pub fn diff(old: &Document, new: &Document) -> Vec<Change> {
    diff_with(old, new, &DiffOptions::default())
}

/// Compare two documents, choosing which categories to report.
pub fn diff_with(old: &Document, new: &Document, options: &DiffOptions) -> Vec<Change> {
    let mut differ = Differ {
        old,
        new,
        options,
        changes: Vec::new(),
    };
    differ.run();
    let mut changes = differ.changes;
    if options.breaking_only {
        changes.retain(|change| change.breaking);
    }
    changes
}

/// `true` when any change would break an existing correct client.
///
/// What `moso openapi check --breaking` gates CI on.
pub fn has_breaking(changes: &[Change]) -> bool {
    changes.iter().any(|change| change.breaking)
}

/// Render changes as the CLI prints them.
///
/// Breaking changes are listed first and marked; the output has no ANSI
/// escapes, so the caller decides about colour. Each line is indented two
/// spaces and the detail column is aligned across every change, so the result
/// is readable in a terminal and in a CI log.
///
/// For the full `moso openapi check` block — header, change lines, and the
/// line telling the reader what to run — use [`ChangeReport`].
pub fn format_changes(changes: &[Change]) -> String {
    let ordered = changes
        .iter()
        .filter(|change| change.breaking)
        .chain(changes.iter().filter(|change| !change.breaking));
    render_lines(ordered, changes, true)
}

/// The whole `moso openapi check` verdict, ready to print.
///
/// ```
/// use moso_openapi::diff::{Change, ChangeKind, ChangeReport};
///
/// let changes = [Change::new(ChangeKind::Removed, "GET /legacy", "(removed)").breaking()];
/// let report = ChangeReport::new(&changes);
/// assert!(report.to_string().starts_with("✗ openapi.json is out of date"));
/// assert!(report.has_breaking());
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ChangeReport<'a> {
    changes: &'a [Change],
    file: &'a str,
    command: &'a str,
}

impl<'a> ChangeReport<'a> {
    /// A report about `openapi.json`, fixed by `moso openapi export`.
    pub fn new(changes: &'a [Change]) -> Self {
        Self {
            changes,
            file: "openapi.json",
            command: "moso openapi export",
        }
    }

    /// Name the file the document was compared against.
    pub fn file(mut self, file: &'a str) -> Self {
        self.file = file;
        self
    }

    /// Name the command that would bring the file up to date.
    pub fn command(mut self, command: &'a str) -> Self {
        self.command = command;
        self
    }

    /// `true` when there is nothing to report.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// `true` when any reported change is breaking.
    pub fn has_breaking(&self) -> bool {
        has_breaking(self.changes)
    }

    /// The changes this report covers.
    pub fn changes(&self) -> &'a [Change] {
        self.changes
    }
}

impl fmt::Display for ChangeReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.changes.is_empty() {
            return write!(f, "✓ {} is up to date", self.file);
        }
        writeln!(f, "✗ {} is out of date", self.file)?;
        writeln!(f)?;
        // Grouped by kind rather than by severity: a reviewer reads "what is
        // new, what moved, what is gone", and `format_changes` is there for
        // when severity is the question.
        let ordered = [ChangeKind::Added, ChangeKind::Modified, ChangeKind::Removed]
            .into_iter()
            .flat_map(|kind| self.changes.iter().filter(move |c| c.kind == kind));
        f.write_str(&render_lines(ordered, self.changes, false))?;
        writeln!(f)?;
        write!(
            f,
            "  run `{}` to update, and review the diff before committing",
            self.command
        )
    }
}

/// Render one line per change with the detail column aligned.
fn render_lines<'c>(
    ordered: impl Iterator<Item = &'c Change>,
    all: &[Change],
    mark_breaking: bool,
) -> String {
    let width = all
        .iter()
        .map(|change| change.path.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for change in ordered {
        out.push_str("  ");
        out.push(change.kind.symbol());
        out.push(' ');
        out.push_str(&change.path);
        let tail = !change.detail.is_empty() || (mark_breaking && change.breaking);
        if tail {
            for _ in 0..width.saturating_sub(change.path.chars().count()) + 2 {
                out.push(' ');
            }
            out.push_str(&change.detail);
        }
        if mark_breaking && change.breaking {
            if !change.detail.is_empty() {
                out.push_str("  ");
            }
            out.push_str("(breaking)");
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------

/// Which side of the wire a schema is on.
///
/// Requests are contravariant and responses are covariant with respect to
/// *presence*, which is the only place the classification differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    Request,
    Response,
}

/// Which direction of a numeric bound is the strict one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tighten {
    /// A larger value is stricter: `minimum`, `minLength`, `minItems`.
    Up,
    /// A smaller value is stricter: `maximum`, `maxLength`, `maxItems`.
    Down,
}

struct Differ<'a> {
    old: &'a Document,
    new: &'a Document,
    options: &'a DiffOptions,
    changes: Vec<Change>,
}

impl<'a> Differ<'a> {
    fn record(&mut self, kind: ChangeKind, path: &str, detail: impl Into<String>, breaking: bool) {
        self.changes.push(Change {
            kind,
            path: path.to_owned(),
            detail: detail.into(),
            breaking,
        });
    }

    fn note_string(&mut self, path: &str, label: &str, old: Option<&str>, new: Option<&str>) {
        if old == new {
            return;
        }
        let kind = match (old, new) {
            (None, Some(_)) => ChangeKind::Added,
            (Some(_), None) => ChangeKind::Removed,
            _ => ChangeKind::Modified,
        };
        let detail = format!("{label} {} → {}", quoted_opt(old), quoted_opt(new));
        self.record(kind, path, detail, false);
    }

    fn run(&mut self) {
        self.info();
        self.servers();
        let (old, new) = (self.old, self.new);
        self.security_of("", &old.security, &new.security);
        self.tags();
        self.path_map(&old.paths, &new.paths, "");
        self.path_map(&old.webhooks, &new.webhooks, "webhook ");
        self.components();
    }

    // ── document level ──────────────────────────────────────────────────

    fn info(&mut self) {
        let (old, new) = (&self.old.info, &self.new.info);
        if old.title != new.title {
            let detail = format!("title `{}` → `{}`", old.title, new.title);
            self.record(ChangeKind::Modified, "info", detail, false);
        }
        if old.version != new.version {
            let detail = format!("version `{}` → `{}`", old.version, new.version);
            self.record(ChangeKind::Modified, "info", detail, false);
        }
        if self.options.include_descriptions {
            let (summary_old, summary_new) = (old.summary.clone(), new.summary.clone());
            self.note_string(
                "info",
                "summary",
                summary_old.as_deref(),
                summary_new.as_deref(),
            );
            let (old, new) = (
                self.old.info.description.clone(),
                self.new.info.description.clone(),
            );
            self.note_string(
                "info",
                "description",
                old.as_deref().map(brief_ref).as_deref(),
                new.as_deref().map(brief_ref).as_deref(),
            );
        }
        let (old, new) = (self.old, self.new);
        self.extensions("info", "", &old.info.extensions, &new.info.extensions);
        self.extensions("", "", &old.extensions, &new.extensions);
    }

    fn servers(&mut self) {
        let (old, new) = (self.old, self.new);
        for server in &old.servers {
            if !new.servers.iter().any(|other| other.url == server.url) {
                let detail = format!("`{}` removed", server.url);
                self.record(ChangeKind::Removed, "servers", detail, false);
            }
        }
        for server in &new.servers {
            if !old.servers.iter().any(|other| other.url == server.url) {
                let detail = format!("`{}` added", server.url);
                self.record(ChangeKind::Added, "servers", detail, false);
            }
        }
    }

    fn tags(&mut self) {
        if !self.options.include_descriptions {
            return;
        }
        let (old, new) = (self.old, self.new);
        for tag in &old.tags {
            match new.tags.iter().find(|other| other.name == tag.name) {
                None => {
                    let detail = format!("`{}` removed", tag.name);
                    self.record(ChangeKind::Removed, "tags", detail, false);
                }
                Some(other) if other.description != tag.description => {
                    let detail = format!(
                        "`{}` description {} → {}",
                        tag.name,
                        quoted_opt(tag.description.as_deref().map(brief_ref).as_deref()),
                        quoted_opt(other.description.as_deref().map(brief_ref).as_deref())
                    );
                    self.record(ChangeKind::Modified, "tags", detail, false);
                }
                Some(_) => {}
            }
        }
        for tag in &new.tags {
            if !old.tags.iter().any(|other| other.name == tag.name) {
                let detail = format!("`{}` added", tag.name);
                self.record(ChangeKind::Added, "tags", detail, false);
            }
        }
    }

    // ── paths ───────────────────────────────────────────────────────────

    fn path_map(
        &mut self,
        old: &'a IndexMap<String, PathItem>,
        new: &'a IndexMap<String, PathItem>,
        prefix: &str,
    ) {
        for path in union_keys(old, new) {
            match (old.get(path), new.get(path)) {
                (Some(old_item), Some(new_item)) => {
                    self.path_item(prefix, path, old_item, new_item);
                }
                (Some(old_item), None) => {
                    for (method, _) in old_item.operations() {
                        let base = format!("{prefix}{method} {path}");
                        self.record(ChangeKind::Removed, &base, "(removed)", true);
                    }
                }
                (None, Some(new_item)) => {
                    for (method, operation) in new_item.operations() {
                        let base = format!("{prefix}{method} {path}");
                        self.record(ChangeKind::Added, &base, added_detail(operation), false);
                    }
                }
                (None, None) => {}
            }
        }
    }

    fn path_item(&mut self, prefix: &str, path: &str, old: &'a PathItem, new: &'a PathItem) {
        for method in HttpMethod::ALL {
            let base = format!("{prefix}{method} {path}");
            match (old.operation(method), new.operation(method)) {
                (Some(old_op), Some(new_op)) => {
                    self.operation(&base, old, new, old_op, new_op);
                }
                (Some(_), None) => self.record(ChangeKind::Removed, &base, "(removed)", true),
                (None, Some(operation)) => {
                    self.record(ChangeKind::Added, &base, added_detail(operation), false);
                }
                (None, None) => {}
            }
        }
    }

    fn operation(
        &mut self,
        base: &str,
        old_item: &'a PathItem,
        new_item: &'a PathItem,
        old: &'a Operation,
        new: &'a Operation,
    ) {
        if self.options.include_descriptions {
            let (old_summary, new_summary) = (old.summary.clone(), new.summary.clone());
            self.note_string(
                base,
                "summary",
                old_summary.as_deref(),
                new_summary.as_deref(),
            );
            let (old_text, new_text) = (
                old.description.as_deref().map(brief_ref),
                new.description.as_deref().map(brief_ref),
            );
            self.note_string(
                base,
                "description",
                old_text.as_deref(),
                new_text.as_deref(),
            );
        }
        let (old_id, new_id) = (old.operation_id.clone(), new.operation_id.clone());
        self.note_string(base, "operationId", old_id.as_deref(), new_id.as_deref());

        if old.deprecated != new.deprecated {
            let detail = if new.deprecated {
                "now deprecated"
            } else {
                "no longer deprecated"
            };
            self.record(ChangeKind::Modified, base, detail, false);
        }

        for tag in &old.tags {
            if !new.tags.contains(tag) {
                let detail = format!("tag `{tag}` removed");
                self.record(ChangeKind::Removed, base, detail, false);
            }
        }
        for tag in &new.tags {
            if !old.tags.contains(tag) {
                let detail = format!("tag `{tag}` added");
                self.record(ChangeKind::Added, base, detail, false);
            }
        }

        self.parameters(
            base,
            &effective_parameters(old_item, old),
            &effective_parameters(new_item, new),
        );
        self.request_body(base, old.request_body.as_ref(), new.request_body.as_ref());
        self.responses(base, &old.responses, &new.responses);

        let old_security = old.security.as_deref().unwrap_or(&self.old.security);
        let new_security = new.security.as_deref().unwrap_or(&self.new.security);
        self.security_of(base, old_security, new_security);

        self.extensions(base, "", &old.extensions, &new.extensions);
    }

    // ── parameters ──────────────────────────────────────────────────────

    fn parameters(&mut self, base: &str, old: &[&'a Parameter], new: &[&'a Parameter]) {
        let mut keys: Vec<(ParameterLocation, &str)> = old
            .iter()
            .chain(new.iter())
            .map(|p| (p.location, p.name.as_str()))
            .collect();
        keys.sort_unstable();
        keys.dedup();

        for (location, name) in keys {
            let find = |set: &[&'a Parameter]| {
                set.iter()
                    .copied()
                    .find(|p| p.location == location && p.name == name)
            };
            match (find(old), find(new)) {
                (None, Some(parameter)) => {
                    let detail = format!("parameter `{name}` ({location}) added");
                    self.record(ChangeKind::Added, base, detail, parameter.required);
                }
                (Some(_), None) => {
                    let detail = format!("parameter `{name}` ({location}) removed");
                    self.record(ChangeKind::Removed, base, detail, false);
                }
                (Some(old_param), Some(new_param)) => {
                    self.parameter(base, old_param, new_param);
                }
                (None, None) => {}
            }
        }
    }

    fn parameter(&mut self, base: &str, old: &'a Parameter, new: &'a Parameter) {
        let what = format!("parameter `{}`", old.name);

        if old.required != new.required {
            let detail = if new.required {
                format!("{what} is now required")
            } else {
                format!("{what} is now optional")
            };
            self.record(ChangeKind::Modified, base, detail, new.required);
        }
        if old.deprecated != new.deprecated {
            let detail = if new.deprecated {
                format!("{what} is now deprecated")
            } else {
                format!("{what} is no longer deprecated")
            };
            self.record(ChangeKind::Modified, base, detail, false);
        }
        if self.options.include_descriptions {
            let (old_text, new_text) = (
                old.description.as_deref().map(brief_ref),
                new.description.as_deref().map(brief_ref),
            );
            self.note_string(
                base,
                &format!("{what} description"),
                old_text.as_deref(),
                new_text.as_deref(),
            );
        }
        if old.style != new.style {
            let detail = format!(
                "{what} style {} → {}",
                quoted_opt(old.style.map(|s| s.as_str())),
                quoted_opt(new.style.map(|s| s.as_str()))
            );
            self.record(ChangeKind::Modified, base, detail, false);
        }
        if old.explode != new.explode {
            let detail = format!(
                "{what} explode {} → {}",
                show_opt_bool(old.explode),
                show_opt_bool(new.explode)
            );
            self.record(ChangeKind::Modified, base, detail, false);
        }
        if old.allow_reserved != new.allow_reserved {
            let detail = format!(
                "{what} allowReserved {} → {}",
                old.allow_reserved, new.allow_reserved
            );
            self.record(ChangeKind::Modified, base, detail, false);
        }
        if old.allow_empty_value != new.allow_empty_value {
            let detail = format!(
                "{what} allowEmptyValue {} → {}",
                old.allow_empty_value, new.allow_empty_value
            );
            self.record(ChangeKind::Modified, base, detail, false);
        }
        if self.options.include_examples && old.example != new.example {
            let detail = format!(
                "{what} example {} → {}",
                show_opt_value(old.example.as_ref()),
                show_opt_value(new.example.as_ref())
            );
            self.record(ChangeKind::Modified, base, detail, false);
        }

        match (&old.schema, &new.schema) {
            (Some(old_schema), Some(new_schema)) => {
                let mut seen = Vec::new();
                self.schema(
                    base,
                    &what,
                    old_schema,
                    new_schema,
                    Position::Request,
                    &mut seen,
                    0,
                );
            }
            (None, Some(_)) => {
                let detail = format!("{what} is now constrained by a schema");
                self.record(ChangeKind::Modified, base, detail, true);
            }
            (Some(_), None) => {
                let detail = format!("{what} is no longer constrained by a schema");
                self.record(ChangeKind::Modified, base, detail, false);
            }
            (None, None) => {}
        }

        self.extensions(base, &what, &old.extensions, &new.extensions);
    }

    // ── bodies and responses ────────────────────────────────────────────

    fn request_body(
        &mut self,
        base: &str,
        old: Option<&'a RequestBody>,
        new: Option<&'a RequestBody>,
    ) {
        match (old, new) {
            (None, Some(body)) => {
                self.record(ChangeKind::Added, base, "request body added", body.required);
            }
            (Some(_), None) => {
                self.record(ChangeKind::Removed, base, "request body removed", false);
            }
            (Some(old_body), Some(new_body)) => {
                if old_body.required != new_body.required {
                    let detail = if new_body.required {
                        "request body is now required"
                    } else {
                        "request body is now optional"
                    };
                    self.record(ChangeKind::Modified, base, detail, new_body.required);
                }
                if self.options.include_descriptions {
                    let (old_text, new_text) = (
                        old_body.description.as_deref().map(brief_ref),
                        new_body.description.as_deref().map(brief_ref),
                    );
                    self.note_string(
                        base,
                        "request body description",
                        old_text.as_deref(),
                        new_text.as_deref(),
                    );
                }
                self.content(
                    base,
                    "request body",
                    &old_body.content,
                    &new_body.content,
                    Position::Request,
                );
            }
            (None, None) => {}
        }
    }

    fn responses(
        &mut self,
        base: &str,
        old: &'a IndexMap<String, Response>,
        new: &'a IndexMap<String, Response>,
    ) {
        let mut keys: Vec<&str> = union_keys(old, new);
        keys.sort_by(|left, right| {
            response_key_rank(left)
                .cmp(&response_key_rank(right))
                .then_with(|| left.cmp(right))
        });

        for key in keys {
            match (old.get(key), new.get(key)) {
                (Some(_), None) => {
                    let detail = format!("response {key} removed");
                    self.record(ChangeKind::Removed, base, detail, is_success_key(key));
                }
                (None, Some(_)) => {
                    let detail = format!("response {key} added");
                    self.record(ChangeKind::Added, base, detail, false);
                }
                (Some(old_response), Some(new_response)) => {
                    self.response(base, key, old_response, new_response);
                }
                (None, None) => {}
            }
        }
    }

    fn response(&mut self, base: &str, key: &str, old: &'a Response, new: &'a Response) {
        let what = format!("response {key}");

        if old.reference != new.reference {
            let detail = format!(
                "{what} {} → {}",
                quoted_opt(old.reference.as_deref()),
                quoted_opt(new.reference.as_deref())
            );
            self.record(ChangeKind::Modified, base, detail, false);
        }
        if self.options.include_descriptions {
            let (old_text, new_text) = (
                old.description.as_deref().map(brief_ref),
                new.description.as_deref().map(brief_ref),
            );
            self.note_string(
                base,
                &format!("{what} description"),
                old_text.as_deref(),
                new_text.as_deref(),
            );
        }

        for name in union_keys(&old.headers, &new.headers) {
            match (old.headers.get(name), new.headers.get(name)) {
                (Some(_), None) => {
                    let detail = format!("{what} header `{name}` removed");
                    self.record(ChangeKind::Removed, base, detail, true);
                }
                (None, Some(_)) => {
                    let detail = format!("{what} header `{name}` added");
                    self.record(ChangeKind::Added, base, detail, false);
                }
                (Some(old_header), Some(new_header)) => {
                    self.header(
                        base,
                        &format!("{what} header `{name}`"),
                        old_header,
                        new_header,
                    );
                }
                (None, None) => {}
            }
        }

        self.content(base, &what, &old.content, &new.content, Position::Response);

        for name in union_keys(&old.links, &new.links) {
            match (old.links.get(name), new.links.get(name)) {
                (Some(_), None) => {
                    let detail = format!("{what} link `{name}` removed");
                    self.record(ChangeKind::Removed, base, detail, false);
                }
                (None, Some(_)) => {
                    let detail = format!("{what} link `{name}` added");
                    self.record(ChangeKind::Added, base, detail, false);
                }
                (Some(old_link), Some(new_link)) if old_link != new_link => {
                    let detail = format!("{what} link `{name}` changed");
                    self.record(ChangeKind::Modified, base, detail, false);
                }
                _ => {}
            }
        }

        self.extensions(base, &what, &old.extensions, &new.extensions);
    }

    fn header(&mut self, base: &str, what: &str, old: &'a Header, new: &'a Header) {
        if old.required != new.required {
            let detail = if new.required {
                format!("{what} is now always sent")
            } else {
                format!("{what} is now optional")
            };
            self.record(ChangeKind::Modified, base, detail, !new.required);
        }
        if let (Some(old_schema), Some(new_schema)) = (&old.schema, &new.schema) {
            let mut seen = Vec::new();
            self.schema(
                base,
                what,
                old_schema,
                new_schema,
                Position::Response,
                &mut seen,
                0,
            );
        }
    }

    fn content(
        &mut self,
        base: &str,
        what: &str,
        old: &'a IndexMap<String, MediaType>,
        new: &'a IndexMap<String, MediaType>,
        position: Position,
    ) {
        for content_type in union_keys(old, new) {
            let label = media_label(what, content_type);
            match (old.get(content_type), new.get(content_type)) {
                (Some(_), None) => {
                    let detail = format!("{label} is no longer offered");
                    self.record(ChangeKind::Removed, base, detail, true);
                }
                (None, Some(_)) => {
                    let detail = format!("{label} is now offered");
                    self.record(ChangeKind::Added, base, detail, false);
                }
                (Some(old_media), Some(new_media)) => {
                    match (&old_media.schema, &new_media.schema) {
                        (Some(old_schema), Some(new_schema)) => {
                            let mut seen = Vec::new();
                            self.schema(
                                base, &label, old_schema, new_schema, position, &mut seen, 0,
                            );
                        }
                        (None, Some(_)) => {
                            let detail = format!("{label} gained a schema");
                            self.record(
                                ChangeKind::Modified,
                                base,
                                detail,
                                position == Position::Request,
                            );
                        }
                        (Some(_), None) => {
                            let detail = format!("{label} lost its schema");
                            self.record(
                                ChangeKind::Modified,
                                base,
                                detail,
                                position == Position::Response,
                            );
                        }
                        (None, None) => {}
                    }
                    if self.options.include_examples && old_media.example != new_media.example {
                        let detail = format!(
                            "{label} example {} → {}",
                            show_opt_value(old_media.example.as_ref()),
                            show_opt_value(new_media.example.as_ref())
                        );
                        self.record(ChangeKind::Modified, base, detail, false);
                    }
                }
                (None, None) => {}
            }
        }
    }

    // ── security ────────────────────────────────────────────────────────

    fn security_of(
        &mut self,
        base: &str,
        old: &'a [SecurityRequirement],
        new: &'a [SecurityRequirement],
    ) {
        let path = if base.is_empty() { "security" } else { base };
        for requirement in new {
            match old.iter().find(|other| same_schemes(other, requirement)) {
                None => {
                    let detail = format!("security requirement `{requirement}` added");
                    self.record(ChangeKind::Added, path, detail, true);
                }
                Some(previous) => self.scopes(path, previous, requirement),
            }
        }
        for requirement in old {
            if !new.iter().any(|other| same_schemes(other, requirement)) {
                let detail = format!("security requirement `{requirement}` removed");
                self.record(ChangeKind::Removed, path, detail, false);
            }
        }
    }

    fn scopes(&mut self, path: &str, old: &SecurityRequirement, new: &SecurityRequirement) {
        for (name, new_scopes) in new.schemes() {
            let old_scopes = old
                .schemes()
                .find(|(other, _)| *other == name)
                .map(|(_, scopes)| scopes)
                .unwrap_or(&[]);
            let gained: Vec<&str> = new_scopes
                .iter()
                .filter(|scope| !old_scopes.contains(scope))
                .map(String::as_str)
                .collect();
            let lost: Vec<&str> = old_scopes
                .iter()
                .filter(|scope| !new_scopes.contains(scope))
                .map(String::as_str)
                .collect();
            if !gained.is_empty() {
                let detail = format!("security `{name}` now requires {}", quoted_list(&gained));
                self.record(ChangeKind::Modified, path, detail, true);
            }
            if !lost.is_empty() {
                let detail = format!(
                    "security `{name}` no longer requires {}",
                    quoted_list(&lost)
                );
                self.record(ChangeKind::Modified, path, detail, false);
            }
        }
    }

    // ── components ──────────────────────────────────────────────────────

    fn components(&mut self) {
        let (old, new) = (self.old, self.new);

        if !self.options.resolve_refs {
            for name in union_keys(&old.components.schemas, &new.components.schemas) {
                let base = format!("components.schemas.{name}");
                match (
                    old.components.schemas.get(name),
                    new.components.schemas.get(name),
                ) {
                    (Some(_), None) => self.record(ChangeKind::Removed, &base, "removed", true),
                    (None, Some(_)) => self.record(ChangeKind::Added, &base, "added", false),
                    (Some(old_schema), Some(new_schema)) => {
                        let mut seen = Vec::new();
                        self.schema(
                            &base,
                            "",
                            old_schema,
                            new_schema,
                            Position::Request,
                            &mut seen,
                            0,
                        );
                    }
                    (None, None) => {}
                }
            }
        }

        for name in union_keys(&old.components.responses, &new.components.responses) {
            let base = format!("components.responses.{name}");
            match (
                old.components.responses.get(name),
                new.components.responses.get(name),
            ) {
                (Some(_), None) => self.record(ChangeKind::Removed, &base, "removed", true),
                (None, Some(_)) => self.record(ChangeKind::Added, &base, "added", false),
                (Some(old_response), Some(new_response)) => {
                    self.content(
                        &base,
                        "body",
                        &old_response.content,
                        &new_response.content,
                        Position::Response,
                    );
                }
                (None, None) => {}
            }
        }

        for name in union_keys(
            &old.components.security_schemes,
            &new.components.security_schemes,
        ) {
            let base = format!("components.securitySchemes.{name}");
            match (
                old.components.security_schemes.get(name),
                new.components.security_schemes.get(name),
            ) {
                (Some(_), None) => self.record(ChangeKind::Removed, &base, "removed", false),
                (None, Some(_)) => self.record(ChangeKind::Added, &base, "added", false),
                (Some(old_scheme), Some(new_scheme)) if old_scheme != new_scheme => {
                    let retyped = old_scheme.kind() != new_scheme.kind();
                    let detail = if retyped {
                        format!("type `{}` → `{}`", old_scheme.kind(), new_scheme.kind())
                    } else {
                        scheme_detail(old_scheme, new_scheme)
                    };
                    self.record(ChangeKind::Modified, &base, detail, retyped);
                }
                _ => {}
            }
        }
    }

    // ── schemas ─────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn schema(
        &mut self,
        base: &str,
        what: &str,
        old: &'a SchemaNode,
        new: &'a SchemaNode,
        position: Position,
        seen: &mut Vec<(String, String)>,
        depth: usize,
    ) {
        if depth > MAX_SCHEMA_DEPTH {
            return;
        }

        if !self.options.resolve_refs {
            if old.reference != new.reference {
                let detail = labelled(
                    what,
                    &format!(
                        "schema {} → {}",
                        quoted_opt(old.reference.as_deref()),
                        quoted_opt(new.reference.as_deref())
                    ),
                );
                self.record(ChangeKind::Modified, base, detail, false);
            }
            if old.reference.is_some() || new.reference.is_some() {
                return;
            }
            self.compare_schema(base, what, old, new, position, seen, depth);
            return;
        }

        let key = (
            old.reference.clone().unwrap_or_default(),
            new.reference.clone().unwrap_or_default(),
        );
        let guarded = !key.0.is_empty() || !key.1.is_empty();
        if guarded {
            if seen.contains(&key) {
                return;
            }
            seen.push(key);
        }
        let old_resolved = resolve_schema(old, self.old);
        let new_resolved = resolve_schema(new, self.new);
        self.compare_schema(
            base,
            what,
            old_resolved,
            new_resolved,
            position,
            seen,
            depth,
        );
        if guarded {
            seen.pop();
        }
    }

    // A JSON Schema node has a lot of independent keywords and each needs its own
    // before/after pair; grouping them into a struct would only move the arity.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "one arm per JSON Schema keyword; splitting it would hide the \
                  exhaustiveness that makes a missed keyword visible in review"
    )]
    fn compare_schema(
        &mut self,
        base: &str,
        what: &str,
        old: &'a SchemaNode,
        new: &'a SchemaNode,
        position: Position,
        seen: &mut Vec<(String, String)>,
        depth: usize,
    ) {
        if old.types != new.types {
            let detail = labelled(
                what,
                &format!("type {} → {}", type_list(&old.types), type_list(&new.types)),
            );
            let breaking = is_narrowing(&old.types, &new.types);
            self.record(ChangeKind::Modified, base, detail, breaking);
        }

        if old.format != new.format {
            let detail = labelled(
                what,
                &format!(
                    "format {} → {}",
                    quoted_opt(old.format.as_deref()),
                    quoted_opt(new.format.as_deref())
                ),
            );
            self.record(ChangeKind::Modified, base, detail, new.format.is_some());
        }

        if old.constant != new.constant {
            let detail = labelled(
                what,
                &format!(
                    "const {} → {}",
                    show_opt_value(old.constant.as_ref()),
                    show_opt_value(new.constant.as_ref())
                ),
            );
            self.record(ChangeKind::Modified, base, detail, new.constant.is_some());
        }

        self.enumeration(base, what, &old.enumeration, &new.enumeration);

        // string
        self.bound_u64(
            base,
            what,
            "minLength",
            old.min_length,
            new.min_length,
            Tighten::Up,
        );
        self.bound_u64(
            base,
            what,
            "maxLength",
            old.max_length,
            new.max_length,
            Tighten::Down,
        );
        if old.pattern != new.pattern {
            let detail = labelled(
                what,
                &format!(
                    "pattern {} → {}",
                    quoted_opt(old.pattern.as_deref()),
                    quoted_opt(new.pattern.as_deref())
                ),
            );
            self.record(ChangeKind::Modified, base, detail, new.pattern.is_some());
        }

        // numeric
        self.bound_number(
            base,
            what,
            "minimum",
            &old.minimum,
            &new.minimum,
            Tighten::Up,
        );
        self.bound_number(
            base,
            what,
            "maximum",
            &old.maximum,
            &new.maximum,
            Tighten::Down,
        );
        self.bound_number(
            base,
            what,
            "exclusiveMinimum",
            &old.exclusive_minimum,
            &new.exclusive_minimum,
            Tighten::Up,
        );
        self.bound_number(
            base,
            what,
            "exclusiveMaximum",
            &old.exclusive_maximum,
            &new.exclusive_maximum,
            Tighten::Down,
        );
        if old.multiple_of != new.multiple_of {
            let detail = labelled(
                what,
                &format!(
                    "multipleOf {} → {}",
                    show_opt_number(old.multiple_of.as_ref()),
                    show_opt_number(new.multiple_of.as_ref())
                ),
            );
            self.record(
                ChangeKind::Modified,
                base,
                detail,
                new.multiple_of.is_some(),
            );
        }

        // array
        self.bound_u64(
            base,
            what,
            "minItems",
            old.min_items,
            new.min_items,
            Tighten::Up,
        );
        self.bound_u64(
            base,
            what,
            "maxItems",
            old.max_items,
            new.max_items,
            Tighten::Down,
        );
        if old.unique_items != new.unique_items {
            let detail = labelled(
                what,
                &format!("uniqueItems {} → {}", old.unique_items, new.unique_items),
            );
            self.record(ChangeKind::Modified, base, detail, new.unique_items);
        }
        match (&old.items, &new.items) {
            (Some(old_items), Some(new_items)) => {
                self.schema(
                    base,
                    &extend(what, "items"),
                    old_items,
                    new_items,
                    position,
                    seen,
                    depth + 1,
                );
            }
            (None, Some(_)) => {
                let detail = labelled(what, "items are now constrained");
                self.record(ChangeKind::Modified, base, detail, true);
            }
            (Some(_), None) => {
                let detail = labelled(what, "items are no longer constrained");
                self.record(ChangeKind::Modified, base, detail, false);
            }
            (None, None) => {}
        }

        // object
        self.properties(base, what, old, new, position, seen, depth);
        self.bound_u64(
            base,
            what,
            "minProperties",
            old.min_properties,
            new.min_properties,
            Tighten::Up,
        );
        self.bound_u64(
            base,
            what,
            "maxProperties",
            old.max_properties,
            new.max_properties,
            Tighten::Down,
        );
        if old.additional_properties != new.additional_properties {
            let detail = labelled(
                what,
                &format!(
                    "additionalProperties {} → {}",
                    show_additional(old.additional_properties.as_ref()),
                    show_additional(new.additional_properties.as_ref())
                ),
            );
            let closed = matches!(
                new.additional_properties,
                Some(AdditionalProperties::Any(false))
            );
            self.record(
                ChangeKind::Modified,
                base,
                detail,
                closed && position == Position::Request,
            );
        }

        // composition
        self.variants(
            base,
            what,
            "oneOf",
            &old.one_of,
            &new.one_of,
            position,
            seen,
            depth,
        );
        self.variants(
            base,
            what,
            "anyOf",
            &old.any_of,
            &new.any_of,
            position,
            seen,
            depth,
        );
        self.variants(
            base,
            what,
            "allOf",
            &old.all_of,
            &new.all_of,
            position,
            seen,
            depth,
        );
        if old.discriminator != new.discriminator {
            let detail = labelled(what, "discriminator changed");
            self.record(ChangeKind::Modified, base, detail, true);
        }

        // annotations
        if old.default != new.default {
            let detail = labelled(
                what,
                &format!(
                    "default {} → {}",
                    show_opt_value(old.default.as_ref()),
                    show_opt_value(new.default.as_ref())
                ),
            );
            self.record(ChangeKind::Modified, base, detail, false);
        }
        if old.deprecated != new.deprecated {
            let detail = labelled(
                what,
                if new.deprecated {
                    "is now deprecated"
                } else {
                    "is no longer deprecated"
                },
            );
            self.record(ChangeKind::Modified, base, detail, false);
        }
        if old.read_only != new.read_only {
            let detail = labelled(
                what,
                &format!("readOnly {} → {}", old.read_only, new.read_only),
            );
            let breaking = new.read_only && position == Position::Request;
            self.record(ChangeKind::Modified, base, detail, breaking);
        }
        if old.write_only != new.write_only {
            let detail = labelled(
                what,
                &format!("writeOnly {} → {}", old.write_only, new.write_only),
            );
            let breaking = new.write_only && position == Position::Response;
            self.record(ChangeKind::Modified, base, detail, breaking);
        }
        if self.options.include_descriptions {
            let (old_title, new_title) = (old.title.as_deref(), new.title.as_deref());
            self.note_string(base, &extend(what, "title"), old_title, new_title);
            let (old_text, new_text) = (
                old.description.as_deref().map(brief_ref),
                new.description.as_deref().map(brief_ref),
            );
            self.note_string(
                base,
                &extend(what, "description"),
                old_text.as_deref(),
                new_text.as_deref(),
            );
        }
        if self.options.include_examples && old.examples != new.examples {
            let detail = labelled(what, "examples changed");
            self.record(ChangeKind::Modified, base, detail, false);
        }

        self.extensions(base, what, &old.extensions, &new.extensions);
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the position in the document (base, what) and the old/new pair are all \
                  needed to render a change the reader can locate"
    )]
    fn properties(
        &mut self,
        base: &str,
        what: &str,
        old: &'a SchemaNode,
        new: &'a SchemaNode,
        position: Position,
        seen: &mut Vec<(String, String)>,
        depth: usize,
    ) {
        for name in union_keys(&old.properties, &new.properties) {
            let child = extend(what, &format!("property `{name}`"));
            let was_required = old.required.iter().any(|entry| entry == name);
            let is_required = new.required.iter().any(|entry| entry == name);

            match (old.properties.get(name), new.properties.get(name)) {
                (None, Some(_)) => {
                    let breaking = position == Position::Request && is_required;
                    self.record(ChangeKind::Added, base, format!("{child} added"), breaking);
                }
                (Some(_), None) => {
                    let breaking = position == Position::Response;
                    self.record(
                        ChangeKind::Removed,
                        base,
                        format!("{child} removed"),
                        breaking,
                    );
                }
                (Some(old_property), Some(new_property)) => {
                    self.requiredness(base, &child, position, was_required, is_required);
                    self.schema(
                        base,
                        &child,
                        old_property,
                        new_property,
                        position,
                        seen,
                        depth + 1,
                    );
                }
                (None, None) => {}
            }
        }

        // A composed schema can require a name it does not itself declare; its
        // requiredness still has to be diffed.
        for name in old.required.iter().chain(new.required.iter()) {
            if old.properties.contains_key(name) || new.properties.contains_key(name) {
                continue;
            }
            let was_required = old.required.iter().any(|entry| entry == name);
            let is_required = new.required.iter().any(|entry| entry == name);
            if was_required == is_required {
                continue;
            }
            let child = extend(what, &format!("property `{name}`"));
            self.requiredness(base, &child, position, was_required, is_required);
        }
    }

    fn requiredness(
        &mut self,
        base: &str,
        child: &str,
        position: Position,
        was_required: bool,
        is_required: bool,
    ) {
        if was_required == is_required {
            return;
        }
        let breaking = match position {
            Position::Request => is_required,
            Position::Response => was_required,
        };
        let detail = if is_required {
            format!("{child} is now required")
        } else {
            format!("{child} is now optional")
        };
        self.record(ChangeKind::Modified, base, detail, breaking);
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "as `properties`: the document position and the old/new pair are what a \
                  located change needs"
    )]
    fn variants(
        &mut self,
        base: &str,
        what: &str,
        keyword: &str,
        old: &'a [SchemaNode],
        new: &'a [SchemaNode],
        position: Position,
        seen: &mut Vec<(String, String)>,
        depth: usize,
    ) {
        if old.is_empty() && new.is_empty() {
            return;
        }
        // Losing a `oneOf`/`anyOf` alternative narrows the type; losing an
        // `allOf` part relaxes it. The two read in opposite directions.
        let composed = keyword == "allOf";
        let old_ids: Vec<String> = old.iter().map(variant_id).collect();
        let new_ids: Vec<String> = new.iter().map(variant_id).collect();

        for (index, id) in old_ids.iter().enumerate() {
            match new_ids.iter().position(|other| other == id) {
                Some(other) => self.schema(
                    base,
                    &extend(what, &format!("{keyword} {}", brief(id))),
                    &old[index],
                    &new[other],
                    position,
                    seen,
                    depth + 1,
                ),
                None => {
                    let detail =
                        format!("{} lost the variant {}", labelled(what, keyword), brief(id));
                    self.record(ChangeKind::Removed, base, detail, !composed);
                }
            }
        }
        for id in &new_ids {
            if !old_ids.contains(id) {
                let detail = format!(
                    "{} gained the variant {}",
                    labelled(what, keyword),
                    brief(id)
                );
                self.record(ChangeKind::Added, base, detail, composed);
            }
        }
    }

    fn enumeration(&mut self, base: &str, what: &str, old: &[Value], new: &[Value]) {
        if old == new {
            return;
        }
        let lost: Vec<String> = old
            .iter()
            .filter(|value| !new.contains(value))
            .map(compact)
            .collect();
        let gained: Vec<String> = new
            .iter()
            .filter(|value| !old.contains(value))
            .map(compact)
            .collect();
        if !lost.is_empty() {
            let detail = labelled(what, &format!("enum lost {}", list(&lost)));
            self.record(ChangeKind::Removed, base, detail, true);
        }
        if !gained.is_empty() {
            let detail = labelled(what, &format!("enum gained {}", list(&gained)));
            self.record(ChangeKind::Added, base, detail, false);
        }
    }

    fn bound_u64(
        &mut self,
        base: &str,
        what: &str,
        keyword: &str,
        old: Option<u64>,
        new: Option<u64>,
        direction: Tighten,
    ) {
        if old == new {
            return;
        }
        let breaking = match (old, new) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(before), Some(after)) => match direction {
                Tighten::Up => after > before,
                Tighten::Down => after < before,
            },
        };
        let detail = labelled(
            what,
            &format!("{keyword} {} → {}", show_opt(old), show_opt(new)),
        );
        self.record(
            kind_for(old.is_some(), new.is_some()),
            base,
            detail,
            breaking,
        );
    }

    fn bound_number(
        &mut self,
        base: &str,
        what: &str,
        keyword: &str,
        old: &Option<Number>,
        new: &Option<Number>,
        direction: Tighten,
    ) {
        if old == new {
            return;
        }
        let breaking = match (old, new) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(before), Some(after)) => {
                let (before, after) = (as_f64(before), as_f64(after));
                match direction {
                    Tighten::Up => after > before,
                    Tighten::Down => after < before,
                }
            }
        };
        let detail = labelled(
            what,
            &format!(
                "{keyword} {} → {}",
                show_opt_number(old.as_ref()),
                show_opt_number(new.as_ref())
            ),
        );
        self.record(
            kind_for(old.is_some(), new.is_some()),
            base,
            detail,
            breaking,
        );
    }

    fn extensions(
        &mut self,
        base: &str,
        what: &str,
        old: &IndexMap<String, Value>,
        new: &IndexMap<String, Value>,
    ) {
        if !self.options.include_extensions {
            return;
        }
        let path = if base.is_empty() { "document" } else { base };
        for key in union_keys(old, new) {
            if key == SOURCE_EXTENSION {
                continue;
            }
            match (old.get(key), new.get(key)) {
                (None, Some(_)) => {
                    let detail = labelled(what, &format!("`{key}` added"));
                    self.record(ChangeKind::Added, path, detail, false);
                }
                (Some(_), None) => {
                    let detail = labelled(what, &format!("`{key}` removed"));
                    self.record(ChangeKind::Removed, path, detail, false);
                }
                (Some(before), Some(after)) if before != after => {
                    let detail = labelled(
                        what,
                        &format!(
                            "`{key}` {} → {}",
                            brief(&compact(before)),
                            brief(&compact(after))
                        ),
                    );
                    self.record(ChangeKind::Modified, path, detail, false);
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The union of two maps' keys, lexicographically sorted so that neither
/// document's insertion order can influence the output.
fn union_keys<'k, V, W>(
    old: &'k IndexMap<String, V>,
    new: &'k IndexMap<String, W>,
) -> Vec<&'k str> {
    let mut keys: Vec<&str> = old
        .keys()
        .chain(new.keys())
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    keys.sort_unstable();
    keys
}

/// Path-item parameters merged under the operation's own, which win.
fn effective_parameters<'p>(item: &'p PathItem, operation: &'p Operation) -> Vec<&'p Parameter> {
    let mut out: Vec<&Parameter> = operation.parameters.iter().collect();
    for parameter in &item.parameters {
        let shadowed = out
            .iter()
            .any(|other| other.location == parameter.location && other.name == parameter.name);
        if !shadowed {
            out.push(parameter);
        }
    }
    out
}

/// Follow `$ref`s into `components.schemas` until a concrete node is reached.
///
/// Stops at [`MAX_REF_HOPS`], at a reference this document does not define, and
/// at any reference that is not a `#/components/schemas/` one.
fn resolve_schema<'d>(node: &'d SchemaNode, document: &'d Document) -> &'d SchemaNode {
    let mut current = node;
    for _ in 0..MAX_REF_HOPS {
        let Some(reference) = current.reference.as_deref() else {
            return current;
        };
        let Some(name) = reference.strip_prefix(COMPONENTS_SCHEMAS_PREFIX) else {
            return current;
        };
        let Some(target) = document.components.schemas.get(name) else {
            return current;
        };
        current = target;
    }
    current
}

fn added_detail(operation: &Operation) -> String {
    match operation
        .extensions
        .get(SOURCE_EXTENSION)
        .and_then(Value::as_str)
    {
        Some(source) => format!("(added in {source})"),
        None => "(added)".to_owned(),
    }
}

/// Two requirements describe the same alternative when they name the same set
/// of schemes; the scopes are then diffed within it.
fn same_schemes(left: &SecurityRequirement, right: &SecurityRequirement) -> bool {
    let names = |requirement: &SecurityRequirement| {
        requirement
            .schemes()
            .map(|(name, _)| name.to_owned())
            .collect::<std::collections::BTreeSet<_>>()
    };
    names(left) == names(right)
}

fn scheme_detail(old: &SecurityScheme, new: &SecurityScheme) -> String {
    match (old, new) {
        (
            SecurityScheme::ApiKey {
                name: old_name,
                location: old_location,
                ..
            },
            SecurityScheme::ApiKey {
                name: new_name,
                location: new_location,
                ..
            },
        ) if old_name != new_name || old_location != new_location => format!(
            "`{old_name}` in {} → `{new_name}` in {}",
            old_location.as_str(),
            new_location.as_str()
        ),
        (
            SecurityScheme::Http {
                scheme: old_scheme, ..
            },
            SecurityScheme::Http {
                scheme: new_scheme, ..
            },
        ) if old_scheme != new_scheme => format!("scheme `{old_scheme}` → `{new_scheme}`"),
        _ => "definition changed".to_owned(),
    }
}

/// A variant's identity within a `oneOf`/`anyOf`/`allOf`.
///
/// A bare `$ref` is identified by its target so that following the reference
/// still pairs the right variants up; anything else by its own serialisation.
fn variant_id(node: &SchemaNode) -> String {
    match &node.reference {
        Some(reference) => reference.clone(),
        None => serde_json::to_string(node).unwrap_or_else(|_| "<unprintable>".to_owned()),
    }
}

/// Whether the new type set is a strict subset of the old one.
fn is_narrowing(old: &TypeSet, new: &TypeSet) -> bool {
    if old.is_empty() || new.is_empty() {
        // `{}` accepts anything; going from unconstrained to constrained is a
        // narrowing, the reverse is a widening.
        return old.is_empty() && !new.is_empty();
    }
    new.iter().all(|ty| old.contains(*ty)) && new.len() < old.len()
        || (old.contains(moso_schema::json_schema::JsonType::Number)
            && new.contains(moso_schema::json_schema::JsonType::Integer)
            && !new.contains(moso_schema::json_schema::JsonType::Number))
}

fn type_list(types: &TypeSet) -> String {
    if types.is_empty() {
        return "any".to_owned();
    }
    let names: Vec<&str> = types.iter().map(|ty| ty.as_str()).collect();
    names.join("|")
}

fn is_success_key(key: &str) -> bool {
    key.starts_with('2') && (key.parse::<u16>().is_ok() || key.eq_ignore_ascii_case("2xx"))
}

fn media_label(what: &str, content_type: &str) -> String {
    if content_type == "application/json" {
        what.to_owned()
    } else {
        format!("{what} ({content_type})")
    }
}

/// `what` and `rest` joined with a space, tolerating an empty `what`.
fn labelled(what: &str, rest: &str) -> String {
    if what.is_empty() {
        rest.to_owned()
    } else {
        format!("{what} {rest}")
    }
}

/// The sub-location one level deeper.
fn extend(what: &str, part: &str) -> String {
    labelled(what, part)
}

fn kind_for(had: bool, has: bool) -> ChangeKind {
    match (had, has) {
        (false, true) => ChangeKind::Added,
        (true, false) => ChangeKind::Removed,
        _ => ChangeKind::Modified,
    }
}

fn quoted_opt(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("`{value}`"),
        None => "none".to_owned(),
    }
}

fn quoted_list(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("`{item}`")).collect();
    quoted.join(", ")
}

fn list(items: &[String]) -> String {
    items.join(", ")
}

fn show_opt(value: Option<u64>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "none".to_owned(),
    }
}

fn show_opt_bool(value: Option<bool>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "none".to_owned(),
    }
}

fn show_opt_number(value: Option<&Number>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "none".to_owned(),
    }
}

fn show_opt_value(value: Option<&Value>) -> String {
    match value {
        Some(value) => brief(&compact(value)),
        None => "none".to_owned(),
    }
}

fn show_additional(value: Option<&AdditionalProperties>) -> String {
    match value {
        None => "unset".to_owned(),
        Some(AdditionalProperties::Any(allowed)) => allowed.to_string(),
        Some(AdditionalProperties::Schema(_)) => "a schema".to_owned(),
    }
}

fn as_f64(number: &Number) -> f64 {
    number.as_f64().unwrap_or(f64::NAN)
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unprintable>".to_owned())
}

/// Truncate on a character boundary so that a long description or a large
/// inline schema cannot swamp a change list.
fn brief(value: &str) -> String {
    if value.chars().count() <= BRIEF_LEN {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(BRIEF_LEN - 1).collect();
    out.push('…');
    out
}

fn brief_ref(value: &str) -> String {
    brief(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Components, Document, Info, Server};
    use crate::path::{Operation, Parameter, ParameterLocation, PathItem, Response};
    use crate::security::SecurityRequirement;
    use moso_schema::json_schema::JsonType;
    use serde_json::json;

    // ── fixtures ────────────────────────────────────────────────────────

    fn user_schema() -> SchemaNode {
        let mut node = SchemaNode::of_type(JsonType::Object);
        node.properties
            .insert("id".to_owned(), SchemaNode::of_type(JsonType::String));
        node.properties
            .insert("email".to_owned(), SchemaNode::of_type(JsonType::String));
        node.required = vec!["id".to_owned()];
        node
    }

    fn document_with(path: &str, method: HttpMethod, operation: Operation) -> Document {
        let mut document = Document::new(Info::new("Shop API", "1.0.0"));
        let mut item = PathItem::default();
        item.set_operation(method, operation);
        document.paths.insert(path.to_owned(), item);
        document
    }

    fn listing() -> Operation {
        let mut operation = Operation::default();
        let mut limit = Parameter::new("limit", ParameterLocation::Query);
        let mut schema = SchemaNode::of_type(JsonType::Integer);
        schema.maximum = Some(Number::from(100));
        limit.schema = Some(schema);
        operation.parameters.push(limit);
        operation
            .responses
            .insert("200".to_owned(), Response::new("the users"));
        operation
    }

    /// The single change the diff produced. Returned by value so that call
    /// sites can write `only(&diff(&old, &new))` without a `let` dance.
    fn only(changes: &[Change]) -> Change {
        assert_eq!(changes.len(), 1, "{changes:#?}");
        changes[0].clone()
    }

    fn find(changes: &[Change], needle: &str) -> Change {
        changes
            .iter()
            .find(|change| change.detail.contains(needle))
            .unwrap_or_else(|| panic!("no change mentioning `{needle}` in {changes:#?}"))
            .clone()
    }

    // ── the documented invariants ───────────────────────────────────────

    #[test]
    fn change_kind_symbols_match_the_cli_legend() {
        assert_eq!(ChangeKind::Added.symbol(), '+');
        assert_eq!(ChangeKind::Removed.symbol(), '-');
        assert_eq!(ChangeKind::Modified.symbol(), '~');
    }

    #[test]
    fn has_breaking_is_an_any() {
        let benign = Change::new(ChangeKind::Added, "GET /users", "");
        let bad = Change::new(ChangeKind::Removed, "GET /legacy", "").breaking();
        assert!(!has_breaking(std::slice::from_ref(&benign)));
        assert!(has_breaking(&[benign, bad]));
    }

    #[test]
    fn structural_options_drop_prose() {
        let options = DiffOptions::structural();
        assert!(!options.include_descriptions);
        assert!(!options.include_extensions);
        assert!(options.resolve_refs);
    }

    #[test]
    fn an_identical_document_has_no_changes() {
        let document = document_with("/users", HttpMethod::Get, listing());
        assert!(diff(&document, &document).is_empty());
    }

    #[test]
    fn changes_do_not_depend_on_map_ordering() {
        let mut first = Document::new(Info::new("T", "1"));
        let mut second = Document::new(Info::new("T", "1"));
        for path in ["/a", "/b", "/c"] {
            first.paths.insert(path.to_owned(), PathItem::default());
        }
        for path in ["/c", "/a", "/b"] {
            second.paths.insert(path.to_owned(), PathItem::default());
        }
        let mut third = second.clone();
        third.paths.shift_remove("/b");
        third
            .paths
            .insert("/d".to_owned(), path_item_with(Operation::default()));

        let one = diff(&first, &third);
        let other = diff(&second, &third);
        assert_eq!(one, other);
    }

    fn path_item_with(operation: Operation) -> PathItem {
        let mut item = PathItem::default();
        item.set_operation(HttpMethod::Get, operation);
        item
    }

    // ── operations ──────────────────────────────────────────────────────

    #[test]
    fn a_removed_operation_is_breaking_and_an_added_one_is_not() {
        let old = document_with("/legacy/users", HttpMethod::Get, listing());
        let new = document_with("/users", HttpMethod::Get, listing());
        let changes = diff(&old, &new);

        let removed = find(&changes, "(removed)");
        assert_eq!(removed.path, "GET /legacy/users");
        assert!(removed.breaking);
        assert_eq!(removed.kind, ChangeKind::Removed);

        let added = find(&changes, "(added)");
        assert_eq!(added.path, "GET /users");
        assert!(!added.breaking);
        assert_eq!(added.kind, ChangeKind::Added);
    }

    #[test]
    fn an_added_operation_reports_where_it_came_from() {
        let old = Document::new(Info::new("Shop API", "1.0.0"));
        let mut operation = listing();
        operation.extensions.insert(
            SOURCE_EXTENSION.to_owned(),
            json!("src/routes/users.rs:102"),
        );
        let new = document_with("/users/{id}/deactivate", HttpMethod::Post, operation);
        let changes = diff(&old, &new);
        assert_eq!(only(&changes).detail, "(added in src/routes/users.rs:102)");
    }

    #[test]
    fn a_removed_method_on_a_kept_path_is_breaking() {
        let mut old = document_with("/users", HttpMethod::Get, listing());
        old.paths["/users"].set_operation(HttpMethod::Post, Operation::default());
        let new = document_with("/users", HttpMethod::Get, listing());
        let changes = diff(&old, &new);
        let change = only(&changes);
        assert_eq!(change.path, "POST /users");
        assert!(change.breaking);
    }

    // ── parameters ──────────────────────────────────────────────────────

    #[test]
    fn a_relaxed_parameter_constraint_is_not_breaking() {
        let old = document_with("/users", HttpMethod::Get, listing());
        let mut new = document_with("/users", HttpMethod::Get, listing());
        new.paths["/users"].get.as_mut().unwrap().parameters[0]
            .schema
            .as_mut()
            .unwrap()
            .maximum = Some(Number::from(200));

        let changes = diff(&old, &new);
        let change = only(&changes);
        assert_eq!(change.path, "GET /users");
        assert_eq!(change.detail, "parameter `limit` maximum 100 → 200");
        assert!(!change.breaking);
    }

    #[test]
    fn a_tightened_parameter_constraint_is_breaking() {
        let old = document_with("/users", HttpMethod::Get, listing());
        let mut new = document_with("/users", HttpMethod::Get, listing());
        new.paths["/users"].get.as_mut().unwrap().parameters[0]
            .schema
            .as_mut()
            .unwrap()
            .maximum = Some(Number::from(50));
        assert!(only(&diff(&old, &new)).breaking);
    }

    #[test]
    fn an_added_required_parameter_is_breaking_and_an_optional_one_is_not() {
        let old = document_with("/users", HttpMethod::Get, listing());

        let mut new = document_with("/users", HttpMethod::Get, listing());
        let mut required = Parameter::new("tenant", ParameterLocation::Query);
        required.required = true;
        new.paths["/users"]
            .get
            .as_mut()
            .unwrap()
            .parameters
            .push(required);
        assert!(only(&diff(&old, &new)).breaking);

        let mut new = document_with("/users", HttpMethod::Get, listing());
        new.paths["/users"]
            .get
            .as_mut()
            .unwrap()
            .parameters
            .push(Parameter::new("tenant", ParameterLocation::Query));
        assert!(!only(&diff(&old, &new)).breaking);
    }

    #[test]
    fn a_parameter_becoming_required_is_breaking_and_the_reverse_is_not() {
        let old = document_with("/users", HttpMethod::Get, listing());
        let mut new = document_with("/users", HttpMethod::Get, listing());
        new.paths["/users"].get.as_mut().unwrap().parameters[0].required = true;
        let forward = diff(&old, &new);
        assert!(only(&forward).breaking);
        assert!(!only(&diff(&new, &old)).breaking);
    }

    #[test]
    fn a_removed_parameter_is_reported_but_not_breaking() {
        let old = document_with("/users", HttpMethod::Get, listing());
        let mut new = document_with("/users", HttpMethod::Get, listing());
        new.paths["/users"].get.as_mut().unwrap().parameters.clear();
        let change = only(&diff(&old, &new));
        assert_eq!(change.detail, "parameter `limit` (query) removed");
        assert!(!change.breaking);
    }

    #[test]
    fn a_path_item_parameter_is_the_same_as_an_operation_one() {
        let mut old = document_with("/users", HttpMethod::Get, listing());
        let item = old.paths.get_mut("/users").unwrap();
        let parameter = item.get.as_mut().unwrap().parameters.remove(0);
        item.parameters.push(parameter);

        let new = document_with("/users", HttpMethod::Get, listing());
        assert!(diff(&old, &new).is_empty());
    }

    // ── request bodies ──────────────────────────────────────────────────

    fn creating(required: bool, schema: SchemaNode) -> Operation {
        let mut body = RequestBody {
            required,
            ..RequestBody::default()
        };
        body.content
            .insert("application/json".to_owned(), MediaType::new(schema));
        let mut operation = Operation {
            request_body: Some(body),
            ..Operation::default()
        };
        operation
            .responses
            .insert("201".to_owned(), Response::new("created"));
        operation
    }

    #[test]
    fn an_added_required_request_body_is_breaking() {
        let old = document_with("/users", HttpMethod::Post, Operation::default());
        let new = document_with("/users", HttpMethod::Post, creating(true, user_schema()));
        assert!(find(&diff(&old, &new), "request body added").breaking);

        let new = document_with("/users", HttpMethod::Post, creating(false, user_schema()));
        assert!(!find(&diff(&old, &new), "request body added").breaking);
    }

    #[test]
    fn an_added_required_request_field_is_breaking_and_an_optional_one_is_not() {
        let old = document_with("/users", HttpMethod::Post, creating(true, user_schema()));

        let mut required = user_schema();
        required
            .properties
            .insert("tenant".to_owned(), SchemaNode::of_type(JsonType::String));
        required.required.push("tenant".to_owned());
        let new = document_with("/users", HttpMethod::Post, creating(true, required));
        let change = find(&diff(&old, &new), "property `tenant` added");
        assert!(change.breaking);
        assert_eq!(change.detail, "request body property `tenant` added");

        let mut optional = user_schema();
        optional
            .properties
            .insert("tenant".to_owned(), SchemaNode::of_type(JsonType::String));
        let new = document_with("/users", HttpMethod::Post, creating(true, optional));
        assert!(!find(&diff(&old, &new), "property `tenant` added").breaking);
    }

    #[test]
    fn a_request_field_becoming_required_is_breaking_and_the_reverse_is_not() {
        let old = document_with("/users", HttpMethod::Post, creating(true, user_schema()));
        let mut tightened = user_schema();
        tightened.required.push("email".to_owned());
        let new = document_with("/users", HttpMethod::Post, creating(true, tightened));

        let change = find(&diff(&old, &new), "is now required");
        assert!(change.breaking);
        assert!(!find(&diff(&new, &old), "is now optional").breaking);
    }

    #[test]
    fn a_removed_request_content_type_is_breaking() {
        let mut old_body = RequestBody::default();
        old_body
            .content
            .insert("application/json".to_owned(), MediaType::opaque());
        old_body.content.insert(
            "application/x-www-form-urlencoded".to_owned(),
            MediaType::opaque(),
        );
        let mut old_op = Operation {
            request_body: Some(old_body),
            ..Operation::default()
        };
        old_op
            .responses
            .insert("201".to_owned(), Response::new("created"));

        let mut new_body = RequestBody::default();
        new_body
            .content
            .insert("application/json".to_owned(), MediaType::opaque());
        let mut new_op = Operation {
            request_body: Some(new_body),
            ..Operation::default()
        };
        new_op
            .responses
            .insert("201".to_owned(), Response::new("created"));

        let old = document_with("/users", HttpMethod::Post, old_op);
        let new = document_with("/users", HttpMethod::Post, new_op);
        let change = only(&diff(&old, &new));
        assert!(change.breaking);
        assert!(change.detail.contains("x-www-form-urlencoded"));
    }

    // ── responses ───────────────────────────────────────────────────────

    fn responding(status: &str, schema: SchemaNode) -> Operation {
        let mut response = Response::new("ok");
        response
            .content
            .insert("application/json".to_owned(), MediaType::new(schema));
        let mut operation = Operation::default();
        operation.responses.insert(status.to_owned(), response);
        operation
    }

    #[test]
    fn a_removed_response_field_is_breaking_and_an_added_one_is_not() {
        let old = document_with(
            "/users/1",
            HttpMethod::Get,
            responding("200", user_schema()),
        );

        let mut shrunk = user_schema();
        shrunk.properties.shift_remove("email");
        let new = document_with("/users/1", HttpMethod::Get, responding("200", shrunk));
        let change = find(&diff(&old, &new), "property `email` removed");
        assert!(change.breaking);
        assert_eq!(change.detail, "response 200 property `email` removed");

        let mut grown = user_schema();
        grown
            .properties
            .insert("name".to_owned(), SchemaNode::of_type(JsonType::String));
        grown.required.push("name".to_owned());
        let new = document_with("/users/1", HttpMethod::Get, responding("200", grown));
        assert!(!find(&diff(&old, &new), "property `name` added").breaking);
    }

    #[test]
    fn a_response_field_becoming_optional_is_breaking() {
        let old = document_with(
            "/users/1",
            HttpMethod::Get,
            responding("200", user_schema()),
        );
        let mut relaxed = user_schema();
        relaxed.required.clear();
        let new = document_with("/users/1", HttpMethod::Get, responding("200", relaxed));
        assert!(find(&diff(&old, &new), "is now optional").breaking);
        assert!(!find(&diff(&new, &old), "is now required").breaking);
    }

    #[test]
    fn a_removed_success_status_is_breaking_but_a_removed_error_status_is_not() {
        let mut old_op = responding("200", user_schema());
        old_op
            .responses
            .insert("201".to_owned(), Response::new("created"));
        old_op
            .responses
            .insert("404".to_owned(), Response::new("gone"));
        let old = document_with("/users", HttpMethod::Get, old_op);

        let new = document_with("/users", HttpMethod::Get, responding("200", user_schema()));
        let changes = diff(&old, &new);
        assert!(find(&changes, "response 201 removed").breaking);
        assert!(!find(&changes, "response 404 removed").breaking);
    }

    #[test]
    fn an_added_error_status_is_not_breaking() {
        let old = document_with("/users", HttpMethod::Get, responding("200", user_schema()));
        let mut new_op = responding("200", user_schema());
        new_op
            .responses
            .insert("429".to_owned(), Response::new("slow down"));
        let new = document_with("/users", HttpMethod::Get, new_op);
        let change = only(&diff(&old, &new));
        assert_eq!(change.detail, "response 429 added");
        assert!(!change.breaking);
    }

    #[test]
    fn responses_are_walked_in_status_order() {
        let old = document_with("/users", HttpMethod::Get, Operation::default());
        let mut new_op = Operation::default();
        for key in ["default", "500", "200", "404"] {
            new_op.responses.insert(key.to_owned(), Response::new("x"));
        }
        let new = document_with("/users", HttpMethod::Get, new_op);
        let details: Vec<&str> = diff(&old, &new)
            .iter()
            .map(|change| change.detail.as_str())
            .map(|detail| {
                detail
                    .trim_start_matches("response ")
                    .trim_end_matches(" added")
            })
            .collect::<Vec<_>>()
            .iter()
            .map(|s| Box::leak(s.to_string().into_boxed_str()) as &str)
            .collect();
        assert_eq!(details, ["200", "404", "500", "default"]);
    }

    // ── types, enums and composition ────────────────────────────────────

    #[test]
    fn narrowing_a_type_is_breaking_and_widening_is_not() {
        let number = SchemaNode::of_type(JsonType::Number);
        let integer = SchemaNode::of_type(JsonType::Integer);
        let old = document_with("/n", HttpMethod::Get, responding("200", number.clone()));
        let new = document_with("/n", HttpMethod::Get, responding("200", integer.clone()));
        let change = only(&diff(&old, &new));
        assert_eq!(change.detail, "response 200 type number → integer");
        assert!(change.breaking);
        assert!(!only(&diff(&new, &old)).breaking);
    }

    #[test]
    fn dropping_null_from_a_type_set_is_narrowing() {
        let nullable = SchemaNode::of_type(JsonType::String).nullable();
        let plain = SchemaNode::of_type(JsonType::String);
        let old = document_with("/n", HttpMethod::Get, responding("200", nullable));
        let new = document_with("/n", HttpMethod::Get, responding("200", plain));
        assert!(only(&diff(&old, &new)).breaking);
    }

    #[test]
    fn shrinking_an_enum_is_breaking_and_growing_it_is_not() {
        let wide = SchemaNode::enumeration(vec![json!("a"), json!("b"), json!("c")]);
        let narrow = SchemaNode::enumeration(vec![json!("a"), json!("b")]);
        let old = document_with("/e", HttpMethod::Get, responding("200", wide));
        let new = document_with("/e", HttpMethod::Get, responding("200", narrow));
        let change = only(&diff(&old, &new));
        assert_eq!(change.detail, r#"response 200 enum lost "c""#);
        assert!(change.breaking);
        assert!(!only(&diff(&new, &old)).breaking);
    }

    #[test]
    fn losing_a_one_of_variant_is_breaking_and_gaining_one_is_not() {
        let wide = SchemaNode::one_of(vec![
            SchemaNode::reference("#/components/schemas/Cat"),
            SchemaNode::reference("#/components/schemas/Dog"),
        ]);
        let narrow = SchemaNode::one_of(vec![SchemaNode::reference("#/components/schemas/Cat")]);
        let old = document_with("/p", HttpMethod::Get, responding("200", wide));
        let new = document_with("/p", HttpMethod::Get, responding("200", narrow));
        let change = only(&diff(&old, &new));
        assert!(change.breaking);
        assert!(change.detail.contains("lost the variant"));
        assert!(!only(&diff(&new, &old)).breaking);
    }

    #[test]
    fn gaining_an_all_of_part_is_breaking_because_it_constrains() {
        let loose = SchemaNode::all_of(vec![SchemaNode::reference("#/components/schemas/Base")]);
        let tight = SchemaNode::all_of(vec![
            SchemaNode::reference("#/components/schemas/Base"),
            SchemaNode::reference("#/components/schemas/Extra"),
        ]);
        let old = document_with("/p", HttpMethod::Post, creating(true, loose));
        let new = document_with("/p", HttpMethod::Post, creating(true, tight));
        assert!(only(&diff(&old, &new)).breaking);
        assert!(!only(&diff(&new, &old)).breaking);
    }

    #[test]
    fn closing_a_request_object_is_breaking() {
        let open = user_schema();
        let mut closed = user_schema();
        closed.additional_properties = Some(AdditionalProperties::Any(false));
        let old = document_with("/p", HttpMethod::Post, creating(true, open.clone()));
        let new = document_with("/p", HttpMethod::Post, creating(true, closed.clone()));
        assert!(only(&diff(&old, &new)).breaking);

        let old = document_with("/p", HttpMethod::Get, responding("200", open));
        let new = document_with("/p", HttpMethod::Get, responding("200", closed));
        assert!(!only(&diff(&old, &new)).breaking);
    }

    // ── $ref resolution ─────────────────────────────────────────────────

    fn referring(components: Components) -> Document {
        let mut document = document_with(
            "/users/1",
            HttpMethod::Get,
            responding("200", SchemaNode::reference("#/components/schemas/User")),
        );
        document.components = components;
        document
    }

    #[test]
    fn refs_are_followed_by_default() {
        let mut old_components = Components::default();
        old_components
            .schemas
            .insert("User".to_owned(), user_schema());
        let mut new_components = Components::default();
        let mut shrunk = user_schema();
        shrunk.properties.shift_remove("email");
        new_components.schemas.insert("User".to_owned(), shrunk);

        let changes = diff(&referring(old_components), &referring(new_components));
        let change = only(&changes);
        assert_eq!(change.path, "GET /users/1");
        assert_eq!(change.detail, "response 200 property `email` removed");
        assert!(change.breaking);
    }

    #[test]
    fn without_resolution_the_change_is_attributed_to_the_component() {
        let mut old_components = Components::default();
        old_components
            .schemas
            .insert("User".to_owned(), user_schema());
        let mut new_components = Components::default();
        let mut shrunk = user_schema();
        shrunk.properties.shift_remove("email");
        new_components.schemas.insert("User".to_owned(), shrunk);

        let options = DiffOptions {
            resolve_refs: false,
            ..DiffOptions::default()
        };
        let changes = diff_with(
            &referring(old_components),
            &referring(new_components),
            &options,
        );
        let change = only(&changes);
        assert_eq!(change.path, "components.schemas.User");
        assert_eq!(change.detail, "property `email` removed");
    }

    #[test]
    fn a_self_referential_schema_terminates() {
        let mut node = user_schema();
        node.properties.insert(
            "manager".to_owned(),
            SchemaNode::reference("#/components/schemas/User"),
        );
        let mut components = Components::default();
        components.schemas.insert("User".to_owned(), node.clone());
        let old = referring(components);

        let mut changed = node;
        changed
            .properties
            .insert("name".to_owned(), SchemaNode::of_type(JsonType::String));
        let mut components = Components::default();
        components.schemas.insert("User".to_owned(), changed);
        let new = referring(components);

        let changes = diff(&old, &new);
        assert!(!changes.is_empty());
        assert!(changes.iter().any(|c| c.detail.contains("`name` added")));
    }

    // ── security ────────────────────────────────────────────────────────

    #[test]
    fn an_added_security_requirement_is_breaking_and_a_removed_one_is_not() {
        let old = document_with("/users", HttpMethod::Get, listing());
        let mut new = document_with("/users", HttpMethod::Get, listing());
        new.paths["/users"].get.as_mut().unwrap().security =
            Some(vec![SecurityRequirement::scheme("bearer")]);

        let change = only(&diff(&old, &new));
        assert_eq!(change.path, "GET /users");
        assert_eq!(change.detail, "security requirement `bearer` added");
        assert!(change.breaking);
        assert!(!only(&diff(&new, &old)).breaking);
    }

    #[test]
    fn gaining_a_scope_is_breaking_and_losing_one_is_not() {
        let mut old = document_with("/users", HttpMethod::Get, listing());
        old.paths["/users"].get.as_mut().unwrap().security =
            Some(vec![SecurityRequirement::scopes("oauth", ["read"])]);
        let mut new = document_with("/users", HttpMethod::Get, listing());
        new.paths["/users"].get.as_mut().unwrap().security =
            Some(vec![SecurityRequirement::scopes(
                "oauth",
                ["read", "write"],
            )]);

        let change = only(&diff(&old, &new));
        assert_eq!(change.detail, "security `oauth` now requires `write`");
        assert!(change.breaking);
        assert!(!only(&diff(&new, &old)).breaking);
    }

    #[test]
    fn an_operation_inherits_the_document_level_security() {
        let mut old = document_with("/users", HttpMethod::Get, listing());
        old.security.push(SecurityRequirement::scheme("session"));
        let mut new = document_with("/users", HttpMethod::Get, listing());
        new.security.push(SecurityRequirement::scheme("session"));
        new.paths["/users"].get.as_mut().unwrap().security = Some(Vec::new());

        // The endpoint became public: the requirement it inherited is gone.
        let change = only(&diff(&old, &new));
        assert_eq!(change.path, "GET /users");
        assert_eq!(change.kind, ChangeKind::Removed);
        assert!(!change.breaking);
    }

    // ── prose, examples and extensions ──────────────────────────────────

    #[test]
    fn descriptions_are_reported_but_suppressible() {
        let old = document_with("/users", HttpMethod::Get, listing());
        let mut new = document_with("/users", HttpMethod::Get, listing());
        new.paths["/users"].get.as_mut().unwrap().summary = Some("List users".to_owned());

        let change = only(&diff(&old, &new));
        assert_eq!(change.detail, "summary none → `List users`");
        assert!(!change.breaking);
        assert!(diff_with(&old, &new, &DiffOptions::structural()).is_empty());
    }

    #[test]
    fn long_prose_is_elided() {
        let old = document_with("/users", HttpMethod::Get, listing());
        let mut new = document_with("/users", HttpMethod::Get, listing());
        new.paths["/users"].get.as_mut().unwrap().description = Some("x".repeat(500));
        let change = only(&diff(&old, &new));
        assert!(change.detail.contains('…'));
        assert!(change.detail.chars().count() < 120, "{}", change.detail);
    }

    #[test]
    fn extensions_are_reported_except_for_the_source_location() {
        let old = document_with("/users", HttpMethod::Get, listing());
        let mut new = document_with("/users", HttpMethod::Get, listing());
        {
            let operation = new.paths["/users"].get.as_mut().unwrap();
            operation
                .extensions
                .insert(SOURCE_EXTENSION.to_owned(), json!("src/a.rs:9"));
            operation
                .extensions
                .insert("x-internal".to_owned(), json!(true));
        }
        let changes = diff(&old, &new);
        let change = only(&changes);
        assert_eq!(change.detail, "`x-internal` added");

        let options = DiffOptions {
            include_extensions: false,
            ..DiffOptions::default()
        };
        assert!(diff_with(&old, &new, &options).is_empty());
    }

    #[test]
    fn info_servers_and_tags_are_compared() {
        let old = Document::new(Info::new("Shop API", "1.0.0"));
        let mut new = Document::new(Info::new("Shop API", "1.1.0"));
        new.servers
            .push(Server::new("https://api.shop.example", "production"));
        new.tags.push(crate::document::Tag::new("users"));

        let changes = diff(&old, &new);
        assert_eq!(find(&changes, "version").path, "info");
        assert_eq!(find(&changes, "api.shop.example").path, "servers");
        assert_eq!(find(&changes, "`users` added").path, "tags");
        assert!(!has_breaking(&changes));
    }

    #[test]
    fn breaking_only_filters_the_output() {
        let mut old = document_with("/users", HttpMethod::Get, listing());
        old.paths
            .insert("/legacy".to_owned(), path_item_with(Operation::default()));
        let mut new = document_with("/users", HttpMethod::Get, listing());
        new.paths["/users"].get.as_mut().unwrap().summary = Some("List".to_owned());

        let options = DiffOptions {
            breaking_only: true,
            ..DiffOptions::default()
        };
        let changes = diff_with(&old, &new, &options);
        assert!(changes.iter().all(|change| change.breaking));
        assert_eq!(only(&changes).path, "GET /legacy");
    }

    // ── rendering ───────────────────────────────────────────────────────

    #[test]
    fn format_changes_puts_breaking_first_and_marks_it() {
        let changes = [
            Change::new(ChangeKind::Added, "POST /users", "(added)"),
            Change::new(ChangeKind::Removed, "GET /legacy/users", "(removed)").breaking(),
        ];
        let rendered = format_changes(&changes);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("  - GET /legacy/users"));
        assert!(lines[0].ends_with("(breaking)"));
        assert!(lines[1].starts_with("  + POST /users"));
        assert!(!lines[1].contains("breaking"));
    }

    #[test]
    fn format_changes_aligns_the_detail_column() {
        let changes = [
            Change::new(ChangeKind::Added, "POST /a", "one"),
            Change::new(ChangeKind::Modified, "GET /a/much/longer/path", "two"),
        ];
        let rendered = format_changes(&changes);
        // The last run of two or more spaces on a line is the padding, so the
        // column the detail starts in is where that run ends.
        let columns: Vec<usize> = rendered
            .lines()
            .map(|line| line.rfind("  ").expect("a padded line") + 2)
            .collect();
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0], columns[1], "{rendered}");
    }

    #[test]
    fn format_changes_of_nothing_is_nothing() {
        assert_eq!(format_changes(&[]), "");
    }

    #[test]
    fn the_report_renders_the_documented_cli_block() {
        let changes = [
            Change::new(
                ChangeKind::Added,
                "POST /users/{id}/deactivate",
                "(added in src/routes/users.rs:102)",
            ),
            Change::new(
                ChangeKind::Modified,
                "GET /users",
                "parameter `limit` maximum 100 → 200",
            ),
            Change::new(ChangeKind::Removed, "GET /legacy/users", "(removed)").breaking(),
        ];
        let rendered = ChangeReport::new(&changes).to_string();
        let expected = "\
✗ openapi.json is out of date

  + POST /users/{id}/deactivate  (added in src/routes/users.rs:102)
  ~ GET /users                   parameter `limit` maximum 100 → 200
  - GET /legacy/users            (removed)

  run `moso openapi export` to update, and review the diff before committing";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn an_empty_report_says_so() {
        let report = ChangeReport::new(&[]).file("openapi.v1.json");
        assert!(report.is_empty());
        assert!(!report.has_breaking());
        assert_eq!(report.to_string(), "✓ openapi.v1.json is up to date");
    }

    #[test]
    fn the_report_can_name_another_file_and_command() {
        let changes = [Change::new(ChangeKind::Added, "GET /x", "(added)")];
        let rendered = ChangeReport::new(&changes)
            .file("openapi.v1.json")
            .command("moso openapi export --prefix /api/v1")
            .to_string();
        assert!(rendered.starts_with("✗ openapi.v1.json is out of date"));
        assert!(rendered.ends_with("review the diff before committing"));
        assert!(rendered.contains("run `moso openapi export --prefix /api/v1` to update"));
        assert_eq!(ChangeReport::new(&changes).changes().len(), 1);
    }

    // ── helper units ────────────────────────────────────────────────────

    #[test]
    fn success_keys_are_recognised() {
        assert!(is_success_key("200"));
        assert!(is_success_key("204"));
        assert!(is_success_key("2XX"));
        assert!(!is_success_key("404"));
        assert!(!is_success_key("default"));
    }

    #[test]
    fn narrowing_detection_covers_the_documented_cases() {
        let number = TypeSet::of(JsonType::Number);
        let integer = TypeSet::of(JsonType::Integer);
        let nullable = TypeSet::nullable(JsonType::String);
        let plain = TypeSet::of(JsonType::String);
        assert!(is_narrowing(&number, &integer));
        assert!(!is_narrowing(&integer, &number));
        assert!(is_narrowing(&nullable, &plain));
        assert!(!is_narrowing(&plain, &nullable));
        assert!(is_narrowing(&TypeSet::new(), &plain));
        assert!(!is_narrowing(&plain, &TypeSet::new()));
    }

    #[test]
    fn briefing_respects_character_boundaries() {
        assert_eq!(brief("short"), "short");
        let long = "é".repeat(200);
        let briefed = brief(&long);
        assert_eq!(briefed.chars().count(), BRIEF_LEN);
        assert!(briefed.ends_with('…'));
    }
}
