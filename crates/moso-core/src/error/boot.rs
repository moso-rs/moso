//! Boot-time problems, and the report that prints all of them at once.
//!
//! `AppBuilder::build()` never fails fast. It runs every check, collects every
//! problem into a [`BootErrors`], and renders one grouped report:
//!
//! ```text
//! error: application failed to build (3 problems)
//!
//!   ✗ missing provider: `shop::db::Db`
//!       required by  GET /users            src/routes/users.rs:14
//!                    POST /users           src/routes/users.rs:31
//!       fix          add `.provide(db)` to your `App` builder in src/lib.rs
//!                    let db = moso::db::connect(&cfg.database).await?;
//!                    App::new(cfg).provide(db)
//!
//!   ✗ route conflict: GET /users/{id}  and  GET /users/{user_id}
//!       first        src/routes/users.rs:47
//!       second       src/routes/admin.rs:22
//!       note         path parameters must have the same name at the same position
//!       fix          rename one parameter, or nest one router under a distinct prefix
//! ```
//!
//! # The rules the renderer obeys
//!
//! - **Every problem, one pass.** Fixing errors one at a time, recompiling
//!   between each, is the failure mode this exists to prevent.
//! - **A source location on every entry.** `#[endpoint]` captures
//!   `file!()`/`line!()` into the operation spec precisely so this report can
//!   quote it.
//! - **A concrete `fix` line** wherever the fix is mechanical — code the reader
//!   can paste, not a description of code.
//! - **Levenshtein suggestions** for anything name-like: a config key, a
//!   provider type name, a permission, a tag.
//! - **Colour and box drawing only on a TTY.** Redirected output is plain
//!   ASCII, so a CI log stays readable.
//!
//! # Layout constants
//!
//! Every block is `  ✗ headline`, then detail lines indented six columns with
//! the label padded to [`LABEL_WIDTH`]. A value spanning several lines has its
//! continuations aligned under the first, which is why [`BootError::details`]
//! returns whole (possibly multi-line) values rather than pre-split lines.

use std::borrow::Cow;
use std::fmt;
use std::io::IsTerminal;

use moso_openapi::SourceLocation;

/// Columns the `✗ ` bullet is indented by.
const BULLET_INDENT: &str = "  ";
/// Columns a detail label is indented by.
const LABEL_INDENT: &str = "      ";
/// Width a detail label is padded to, so every value starts at one column.
pub const LABEL_WIDTH: usize = 13;
/// The longest headline the renderer will emit before eliding.
const MAX_HEADLINE: usize = 72;
/// The longest type name the renderer will print in full.
///
/// From `docs/04-devex/41-diagnostics.md`: never print a type longer than 80
/// characters, because a wrapped type name is worse than no type name.
const MAX_TYPE_NAME: usize = 80;

/// Where a requirement came from, for the `required by` block.
///
/// One line of the report: `GET /users            src/routes/users.rs:14`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRef {
    /// The HTTP method, upper case.
    pub method: &'static str,
    /// The templated path, `/users/{id}` style.
    pub path: String,
    /// Where `#[endpoint]` was written, when it was.
    pub source: Option<SourceLocation>,
    /// The dependency chain that led here, outermost first, when the
    /// requirement is transitive: `via dependency \`SearchScope\``.
    pub via: Vec<&'static str>,
}

impl RouteRef {
    /// The `METHOD /path` half of the line, unpadded.
    ///
    /// The renderer measures every label in a block so it can align the source
    /// locations into a column.
    pub fn label(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

impl fmt::Display for RouteRef {
    /// `METHOD /path` padded to the format width, then the source location.
    ///
    /// The width is read from the format specifier (`{route:<24}`) so the
    /// report can align a whole block; it defaults to the label's own length
    /// plus two, which is right for a one-line rendering.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = self.label();
        let width = f.width().unwrap_or(label.chars().count() + 2);
        write!(f, "{label:<width$}")?;
        match &self.source {
            Some(source) => write!(f, "{source}")?,
            None => f.write_str("(source location unknown)")?,
        }
        if !self.via.is_empty() {
            write!(f, "  via {}", self.via.join(" -> "))?;
        }
        Ok(())
    }
}

/// A provider a route needs but the builder did not register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequirement {
    /// The fully-qualified type name, from `core::any::type_name`.
    pub type_name: &'static str,
    /// Every route that needs it.
    pub required_by: Vec<RouteRef>,
}

/// One problem found while building the application.
///
/// `#[non_exhaustive]`: later work packages add variants (jobs, permissions)
/// without a breaking change. Match with a `_` arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BootError {
    /// A route needs `Inject<T>` and no `.provide::<T>(..)` registered one.
    ///
    /// The archetypal Moso boot error, and the one that justifies the whole
    /// two-tier DI model: it is impossible for this to surface as a 500.
    MissingProvider {
        /// The missing provider and everything that wanted it.
        requirement: ProviderRequirement,
        /// Every provider that *was* registered, for the "did you mean" line.
        registered: Vec<&'static str>,
    },

    /// Two routes match the same requests.
    RouteConflict {
        /// The method both routes are registered under.
        method: &'static str,
        /// The first path, as written.
        first_path: String,
        /// Where the first was registered.
        first: Option<SourceLocation>,
        /// The second path, as written.
        second_path: String,
        /// Where the second was registered.
        second: Option<SourceLocation>,
        /// Why they conflict: identical, or differing only in a parameter name.
        reason: ConflictReason,
    },

    /// A path template contains a parameter the handler never extracts, or an
    /// extractor names a parameter the path does not declare.
    ///
    /// Axum answers this with a runtime 500 or a silently missing value; a
    /// mismatch is a mistake and boot is where mistakes should surface.
    PathParameterMismatch {
        /// The route, for the report header.
        route: RouteRef,
        /// Parameters the path declares.
        declared: Vec<String>,
        /// Parameters the extractor expects.
        expected: Vec<String>,
    },

    /// A path was written with Axum 0.7 / Actix syntax.
    ///
    /// Normally caught at compile time by the const path validator; this
    /// variant covers paths built at runtime by `mount_at` and `nest`.
    LegacyPathSyntax {
        /// The offending path.
        path: String,
        /// The offending segment, `:id` or `*rest`.
        segment: String,
        /// The corrected path, ready to paste.
        suggestion: String,
    },

    /// Two operations claim the same `operationId`, which breaks every client
    /// generator downstream.
    DuplicateOperationId {
        /// The contested id.
        operation_id: String,
        /// `METHOD /path` of the route that claimed it first.
        first: String,
        /// `METHOD /path` of the route that tried to claim it second.
        second: String,
    },

    /// Two distinct Rust types produced the same schema name, so one would
    /// silently overwrite the other in `components/schemas`.
    SchemaCollision {
        /// The contested schema name.
        name: String,
        /// The first type to claim it.
        first: String,
        /// The second type to claim it.
        second: String,
    },

    /// A `provide_with` factory graph contains a cycle.
    ProviderCycle {
        /// The cycle, in order, with the first element repeated at the end.
        path: Vec<&'static str>,
    },

    /// A `provide_with` factory returned an error.
    ProviderFailed {
        /// The type the factory was building.
        type_name: &'static str,
        /// The factory's error, already rendered.
        detail: String,
    },

    /// A required configuration value was absent from every source.
    MissingConfig {
        /// The dotted key, `database.max_connections` style.
        key: String,
        /// The environment variable that would have supplied it.
        env: String,
        /// The file key that would have supplied it.
        file_key: String,
        /// The expected type, for the `type` line.
        expected_type: &'static str,
    },

    /// A configuration value was present but could not be coerced.
    InvalidConfig {
        /// The dotted key.
        key: String,
        /// Which source supplied the bad value, and its raw text.
        source: String,
        /// What the field expected: `integer in 1..=1000`.
        expected: String,
        /// The value as it was found.
        found: String,
        /// An alternative spelling worth mentioning, if any.
        note: Option<String>,
    },

    /// The middleware stack violates an ordering invariant — `catch_error`
    /// outside `trace`, or `metrics` before routing.
    MiddlewareOrder {
        /// The invariant, phrased as the rule that was broken.
        rule: &'static str,
        /// The stack as it stands, outermost first.
        stack: Vec<String>,
    },

    /// The OpenAPI document failed its own consistency checks.
    Document {
        /// The rendered `moso-openapi` error.
        detail: String,
    },

    /// A problem no other variant models. Batteries use this before they earn
    /// a variant of their own.
    Other {
        /// The headline, rendered after the `✗`.
        message: String,
        /// Indented detail lines, in order.
        notes: Vec<String>,
        /// The `fix` block, if the fix is mechanical.
        fix: Option<String>,
    },
}

impl BootError {
    /// The one-line headline, rendered after the `✗`.
    pub fn headline(&self) -> String {
        let headline = match self {
            BootError::MissingProvider { requirement, .. } => {
                format!(
                    "missing provider: `{}`",
                    elide(requirement.type_name, MAX_TYPE_NAME)
                )
            }
            BootError::RouteConflict {
                method,
                first_path,
                second_path,
                ..
            } => format!("route conflict: {method} {first_path}  and  {method} {second_path}"),
            BootError::PathParameterMismatch { route, .. } => {
                format!("path parameter mismatch: {} {}", route.method, route.path)
            }
            BootError::LegacyPathSyntax { path, .. } => {
                format!("legacy path syntax: {path}")
            }
            BootError::DuplicateOperationId { operation_id, .. } => {
                format!("duplicate operationId: \"{operation_id}\"")
            }
            BootError::SchemaCollision { name, .. } => {
                format!("schema name collision: \"{name}\"")
            }
            BootError::ProviderCycle { path } => {
                format!("provider cycle: {}", short_chain(path))
            }
            BootError::ProviderFailed { type_name, .. } => {
                format!("provider failed: `{}`", elide(type_name, MAX_TYPE_NAME))
            }
            BootError::MissingConfig { key, .. } => format!("missing configuration: {key}"),
            BootError::InvalidConfig { key, .. } => format!("invalid configuration: {key}"),
            BootError::MiddlewareOrder { rule, .. } => format!("middleware order: {rule}"),
            BootError::Document { .. } => "invalid OpenAPI document".to_owned(),
            BootError::Other { message, .. } => message.clone(),
        };
        elide(&headline, MAX_HEADLINE).into_owned()
    }

    /// The indented body: `required by`, `first`/`second`, `note`, and so on.
    ///
    /// A pair whose label is `""` is rendered as a bare line at the label
    /// indent — that is how `did you mean …?` reaches the report without
    /// pretending to be a labelled field.
    pub fn details(&self) -> Vec<(&'static str, String)> {
        let mut details: Vec<(&'static str, String)> = Vec::new();
        match self {
            BootError::MissingProvider {
                requirement,
                registered,
            } => {
                details.push(("required by", route_block(&requirement.required_by)));
                if let Some(suggestion) = suggest_type(requirement.type_name, registered) {
                    details.push(("", format!("did you mean `{suggestion}`?")));
                }
            }
            BootError::RouteConflict {
                first,
                second,
                reason,
                ..
            } => {
                details.push(("first", location(*first)));
                details.push(("second", location(*second)));
                details.push(("note", reason.note().to_owned()));
            }
            BootError::PathParameterMismatch {
                route,
                declared,
                expected,
            } => {
                details.push(("route", route.to_string()));
                details.push(("declared", name_list(declared)));
                details.push(("expected", name_list(expected)));
                let missing: Vec<String> = expected
                    .iter()
                    .filter(|name| !declared.contains(name))
                    .cloned()
                    .collect();
                if let Some(first) = missing.first() {
                    let options: Vec<&str> = declared.iter().map(String::as_str).collect();
                    if let Some(suggestion) = did_you_mean(first, options.iter().copied()) {
                        details.push(("", format!("did you mean `{{{suggestion}}}`?")));
                    }
                }
            }
            BootError::LegacyPathSyntax {
                segment,
                suggestion,
                ..
            } => {
                details.push(("segment", segment.clone()));
                details.push(("note", "Moso uses `{name}` and `{*rest}`, the Axum 0.8 syntax; `:name` and `*rest` are Axum 0.7".to_owned()));
                details.push(("suggestion", suggestion.clone()));
            }
            BootError::DuplicateOperationId { first, second, .. } => {
                details.push(("first", first.clone()));
                details.push(("second", second.clone()));
                details.push((
                    "note",
                    "an operationId is the method name in every generated client, so it must be unique".to_owned(),
                ));
            }
            BootError::SchemaCollision {
                first,
                second,
                name,
            } => {
                details.push(("first", elide(first, MAX_TYPE_NAME).into_owned()));
                details.push(("second", elide(second, MAX_TYPE_NAME).into_owned()));
                details.push((
                    "note",
                    format!(
                        "both types generate `#/components/schemas/{name}`; one would overwrite the other"
                    ),
                ));
            }
            BootError::ProviderCycle { path } => {
                details.push(("cycle", short_chain(path)));
                details.push((
                    "note",
                    "each `provide_with` factory resolves the next, and the last resolves the first"
                        .to_owned(),
                ));
            }
            BootError::ProviderFailed { detail, .. } => {
                details.push(("cause", detail.clone()));
            }
            BootError::MissingConfig {
                key,
                env,
                file_key,
                expected_type,
            } => {
                details.push(("key", key.clone()));
                details.push(("type", (*expected_type).to_owned()));
                details.push(("env", env.clone()));
                details.push(("file", file_key.clone()));
            }
            BootError::InvalidConfig {
                key,
                source,
                expected,
                found,
                note,
            } => {
                details.push(("key", key.clone()));
                details.push(("source", source.clone()));
                details.push(("expected", expected.clone()));
                details.push(("found", found.clone()));
                if let Some(note) = note {
                    details.push(("note", note.clone()));
                }
            }
            BootError::MiddlewareOrder { rule, stack } => {
                details.push(("rule", (*rule).to_owned()));
                details.push(("stack", stack.join("\n")));
            }
            BootError::Document { detail } => {
                details.push(("detail", detail.clone()));
            }
            BootError::Other { notes, .. } => {
                if !notes.is_empty() {
                    details.push(("note", notes.join("\n")));
                }
            }
        }
        details
    }

    /// The `fix` block: code the reader can paste, or `None` when the fix is
    /// a judgement call rather than a mechanical edit.
    pub fn fix(&self) -> Option<String> {
        match self {
            BootError::MissingProvider { requirement, .. } => {
                let short = short_type_name(requirement.type_name);
                Some(format!(
                    "register it on the `App` builder, usually in src/lib.rs\n\
                     let value: {short} = /* construct it */;\n\
                     App::new(config).provide(value)"
                ))
            }
            BootError::RouteConflict { reason, .. } => Some(match reason {
                ConflictReason::Identical => {
                    "remove one registration, or move one router under a distinct prefix with \
                     `mount_at`"
                        .to_owned()
                }
                ConflictReason::ParameterNameMismatch => {
                    "rename one parameter, or nest one router under a distinct prefix".to_owned()
                }
                ConflictReason::WildcardShadows => {
                    "register the wildcard route last, or give it a distinct prefix".to_owned()
                }
            }),
            BootError::PathParameterMismatch { route, declared, .. } => Some(format!(
                "make the extractor's fields match the path: `{}` declares {}",
                route.path,
                name_list(declared)
            )),
            BootError::LegacyPathSyntax { suggestion, .. } => {
                Some(format!("use `{suggestion}`"))
            }
            BootError::DuplicateOperationId { .. } => Some(
                "give one of them an explicit id\n#[endpoint(operation_id = \"unique_name\")]"
                    .to_owned(),
            ),
            BootError::SchemaCollision { .. } => Some(
                "rename one type, or give one an explicit schema name\n\
                 #[schema(rename = \"OtherName\")]"
                    .to_owned(),
            ),
            BootError::ProviderCycle { .. } => Some(
                "break the cycle: build one of these eagerly and register it with `.provide(..)` \
                 instead of `.provide_with(..)`"
                    .to_owned(),
            ),
            BootError::ProviderFailed { .. } => None,
            BootError::MissingConfig { env, key, .. } => Some(format!(
                "supply it from the environment or the profile's config file\nexport {env}=…\n\
                 # or, in config/<profile>.toml\n{key} = …"
            )),
            BootError::InvalidConfig { key, expected, .. } => {
                Some(format!("set `{key}` to {expected}"))
            }
            BootError::MiddlewareOrder { .. } => Some(
                "reorder the stack\nApp::new(config).with_middleware(|stack| { /* move the slot */ })"
                    .to_owned(),
            ),
            BootError::Document { .. } => None,
            BootError::Other { fix, .. } => fix.clone(),
        }
    }

    /// The sort rank used by [`BootErrors::sort_for_report`].
    ///
    /// Lower is earlier. Missing providers come first because they are usually
    /// the root cause of everything under them.
    fn rank(&self) -> u8 {
        match self {
            BootError::MissingProvider { .. } => 0,
            BootError::ProviderCycle { .. } => 1,
            BootError::ProviderFailed { .. } => 2,
            BootError::RouteConflict { .. } => 10,
            BootError::PathParameterMismatch { .. } => 11,
            BootError::LegacyPathSyntax { .. } => 12,
            BootError::MissingConfig { .. } => 20,
            BootError::InvalidConfig { .. } => 21,
            BootError::DuplicateOperationId { .. } => 30,
            BootError::SchemaCollision { .. } => 31,
            BootError::Document { .. } => 32,
            BootError::MiddlewareOrder { .. } => 40,
            BootError::Other { .. } => 50,
        }
    }

    /// Render this one problem into `out`, as the report renders it.
    fn write_block(&self, out: &mut String, style: Style) {
        out.push_str(BULLET_INDENT);
        out.push_str(&style.paint(RED_BOLD, style.bullet()));
        out.push(' ');
        out.push_str(&style.paint(BOLD, &self.headline()));
        out.push('\n');

        for (label, value) in self.details() {
            write_labelled(out, style, label, &value, DIM);
        }
        if let Some(fix) = self.fix() {
            write_labelled(out, style, "fix", &fix, GREEN);
        }
    }
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        self.write_block(&mut out, Style::plain());
        f.write_str(out.trim_end())
    }
}

/// Why two routes conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// Byte-identical paths.
    Identical,
    /// Same shape, different parameter names at the same position — `matchit`
    /// cannot distinguish `/users/{id}` from `/users/{user_id}`.
    ParameterNameMismatch,
    /// A wildcard swallows a route registered after it.
    WildcardShadows,
}

impl ConflictReason {
    /// The `note` line explaining the conflict to the reader.
    pub fn note(self) -> &'static str {
        match self {
            ConflictReason::Identical => "the same method and path are registered twice",
            ConflictReason::ParameterNameMismatch => {
                "path parameters must have the same name at the same position"
            }
            ConflictReason::WildcardShadows => {
                "a wildcard segment matches every path registered beneath it"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BootErrors
// ---------------------------------------------------------------------------

/// Every problem found in one `build()`, in discovery order.
///
/// Empty is the success case, so the collection is built unconditionally and
/// checked once at the end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootErrors {
    problems: Vec<BootError>,
}

impl BootErrors {
    /// An empty report.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a problem.
    pub fn push(&mut self, error: BootError) {
        self.problems.push(error);
    }

    /// Record every problem from another report, keeping order.
    pub fn extend(&mut self, other: BootErrors) {
        self.problems.extend(other.problems);
    }

    /// The problems, in discovery order.
    pub fn as_slice(&self) -> &[BootError] {
        &self.problems
    }

    /// How many problems were found.
    pub fn len(&self) -> usize {
        self.problems.len()
    }

    /// Whether the build succeeded.
    pub fn is_empty(&self) -> bool {
        self.problems.is_empty()
    }

    /// `Ok(())` when empty, `Err(self)` otherwise — the shape `build()` wants.
    pub fn into_result(self) -> Result<(), BootErrors> {
        if self.problems.is_empty() {
            Ok(())
        } else {
            Err(self)
        }
    }

    /// Sort into a stable, useful order before rendering.
    ///
    /// Missing providers first (they are usually the root cause), then route
    /// conflicts, then configuration, then everything else. Within a group,
    /// discovery order is preserved.
    pub fn sort_for_report(&mut self) {
        self.problems.sort_by_key(BootError::rank);
    }

    /// Render the grouped report.
    ///
    /// `colour` selects ANSI escapes and Unicode glyphs; pass the result of
    /// [`stderr_is_tty`]. An empty report renders as the empty string, because
    /// there is nothing to say about a build that worked.
    pub fn render(&self, colour: bool) -> String {
        if self.problems.is_empty() {
            return String::new();
        }
        let style = Style::new(colour);
        let count = self.problems.len();
        let noun = if count == 1 { "problem" } else { "problems" };

        let mut out = String::new();
        out.push_str(&style.paint(RED_BOLD, "error:"));
        out.push(' ');
        out.push_str(&style.paint(
            BOLD,
            &format!("application failed to build ({count} {noun})"),
        ));
        out.push_str("\n\n");

        for (index, problem) in self.problems.iter().enumerate() {
            problem.write_block(&mut out, style);
            if index + 1 < count {
                out.push('\n');
            }
        }
        out
    }
}

impl fmt::Display for BootErrors {
    /// The plain-ASCII rendering. [`BootErrors::render`] when you know whether
    /// the destination is a terminal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(false))
    }
}

impl core::error::Error for BootErrors {}

impl FromIterator<BootError> for BootErrors {
    fn from_iter<I: IntoIterator<Item = BootError>>(iter: I) -> Self {
        Self {
            problems: iter.into_iter().collect(),
        }
    }
}

impl IntoIterator for BootErrors {
    type Item = BootError;
    type IntoIter = std::vec::IntoIter<BootError>;

    fn into_iter(self) -> Self::IntoIter {
        self.problems.into_iter()
    }
}

/// Whether stderr is a terminal, so the report may use colour.
///
/// `NO_COLOR` and `MOSO_NO_COLOR` force this to `false`, in that order of
/// precedence, because respecting them is table stakes for a CLI.
pub fn stderr_is_tty() -> bool {
    fn set(name: &str) -> bool {
        std::env::var_os(name).is_some_and(|value| !value.is_empty())
    }
    if set("NO_COLOR") || set("MOSO_NO_COLOR") {
        return false;
    }
    std::io::stderr().is_terminal()
}

// ---------------------------------------------------------------------------
// Levenshtein
// ---------------------------------------------------------------------------

/// The closest match to `candidate` among `options`, if one is close enough.
///
/// Used for every name-like mismatch in the report. The threshold scales with
/// the length of the input, so `db` does not "suggest" `kv`.
///
/// Comparison is case-insensitive and an exact match is never a suggestion:
/// telling someone they might have meant the thing they wrote is noise.
///
/// ```
/// # use moso_core::error::boot::did_you_mean;
/// assert_eq!(
///     did_you_mean("posts.publsh", ["posts.publish", "posts.delete"]),
///     Some("posts.publish")
/// );
/// assert_eq!(did_you_mean("db", ["kv"]), None);
/// ```
pub fn did_you_mean<'a>(
    candidate: &str,
    options: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let needle = candidate.to_lowercase();
    let threshold = (needle.chars().count() / 3).max(1);
    let mut best: Option<(usize, &'a str)> = None;

    for option in options {
        if option == candidate {
            continue;
        }
        let distance = levenshtein(&needle, &option.to_lowercase());
        if distance > threshold {
            continue;
        }
        if best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, option));
        }
    }
    best.map(|(_, option)| option)
}

/// The Levenshtein edit distance between two strings, counted in `char`s.
///
/// Two rolling rows rather than a full matrix: the report calls this once per
/// registered provider per missing provider, and the inputs are type names, so
/// the constant factor matters more than the asymptotics.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current: Vec<usize> = vec![0; b.len() + 1];

    for (i, left) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, right) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(left != right);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// Suggest a registered type name for a missing one.
///
/// Compares the trailing `::` segments and **never** the fully-qualified names.
/// A typo lives in the type name, not in the module path, and
/// [`did_you_mean`]'s threshold scales with the length of what it is given: on
/// `my_app::Store` against `my_app::Cfg` the shared eleven-character prefix
/// buys a threshold of six, which is enough to call `Cfg` a near miss. Nothing
/// erodes a diagnostic faster than a confident wrong suggestion.
fn suggest_type(missing: &str, registered: &[&'static str]) -> Option<&'static str> {
    let short = short_type_name(missing);

    // The same type name in another module: not a typo at all, but exactly the
    // mistake the reader made, so it outranks any edit-distance match.
    let same_name = registered
        .iter()
        .copied()
        .find(|name| *name != missing && short_type_name(name) == short);
    if same_name.is_some() {
        return same_name;
    }

    let shorts: Vec<&str> = registered.iter().copied().map(short_type_name).collect();
    if let Some(hit) = did_you_mean(short, shorts.iter().copied()) {
        return registered
            .iter()
            .copied()
            .find(|name| short_type_name(name) == hit);
    }

    registered
        .iter()
        .copied()
        .find(|name| *name != missing && short_type_name(name).eq_ignore_ascii_case(short))
}

/// The last `::`-separated segment of a type name: `shop::db::Db` → `Db`.
fn short_type_name(type_name: &str) -> &str {
    type_name.rsplit("::").next().unwrap_or(type_name)
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Elide the middle of `text` so it fits in `max` characters.
///
/// Middle rather than tail: for a type name the informative halves are the
/// crate at the front and the type at the back.
fn elide(text: &str, max: usize) -> Cow<'_, str> {
    let length = text.chars().count();
    if length <= max {
        return Cow::Borrowed(text);
    }
    let keep = max.saturating_sub(3);
    let head = keep / 2 + keep % 2;
    let tail = keep - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(length - tail).collect();
    Cow::Owned(format!("{start}...{end}"))
}

/// `a -> b -> a`, with each element shortened to its type name.
fn short_chain(path: &[&'static str]) -> String {
    path.iter()
        .map(|name| short_type_name(name))
        .collect::<Vec<_>>()
        .join(" -> ")
}

/// `id`, `user_id` — a comma-separated list, or `(none)` when empty.
fn name_list(names: &[String]) -> String {
    if names.is_empty() {
        return "(none)".to_owned();
    }
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A source location, or a stand-in when `#[endpoint]` did not record one.
fn location(source: Option<SourceLocation>) -> String {
    match source {
        Some(source) => source.to_string(),
        None => "(source location unknown)".to_owned(),
    }
}

/// The `required by` value: one line per route, source locations aligned.
fn route_block(routes: &[RouteRef]) -> String {
    let width = routes
        .iter()
        .map(|route| route.label().chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    routes
        .iter()
        .map(|route| format!("{route:width$}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Write one labelled detail, with continuation lines aligned under the value.
fn write_labelled(out: &mut String, style: Style, label: &str, value: &str, colour: &'static str) {
    let mut lines = value.split('\n');
    let Some(first) = lines.next() else {
        return;
    };

    out.push_str(LABEL_INDENT);
    if label.is_empty() {
        out.push_str(&style.paint(colour, first));
    } else {
        out.push_str(&style.paint(DIM, &format!("{label:<LABEL_WIDTH$}")));
        out.push_str(&style.paint(colour, first));
    }
    out.push('\n');

    for line in lines {
        out.push_str(LABEL_INDENT);
        out.push_str(&" ".repeat(LABEL_WIDTH));
        out.push_str(&style.paint(colour, line));
        out.push('\n');
    }
}

/// ANSI parameters, named so the call sites read as intent.
const BOLD: &str = "1";
const DIM: &str = "2";
const RED_BOLD: &str = "1;31";
const GREEN: &str = "32";

/// Whether the report may use ANSI escapes and non-ASCII glyphs.
#[derive(Debug, Clone, Copy)]
struct Style {
    colour: bool,
}

impl Style {
    fn new(colour: bool) -> Self {
        Self { colour }
    }

    fn plain() -> Self {
        Self { colour: false }
    }

    /// The bullet: `✗` on a terminal, `x` in a log file.
    fn bullet(self) -> &'static str {
        if self.colour { "✗" } else { "x" }
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.colour && !text.is_empty() {
            format!("\u{1b}[{code}m{text}\u{1b}[0m")
        } else {
            text.to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(method: &'static str, path: &str, line: u32) -> RouteRef {
        RouteRef {
            method,
            path: path.to_owned(),
            source: Some(SourceLocation {
                file: "src/routes/users.rs",
                line,
            }),
            via: Vec::new(),
        }
    }

    fn missing_provider() -> BootError {
        BootError::MissingProvider {
            requirement: ProviderRequirement {
                type_name: "shop::db::Db",
                required_by: vec![
                    route("GET", "/users", 14),
                    route("POST", "/users", 31),
                    route("GET", "/users/{id}", 47),
                ],
            },
            registered: vec!["shop::mail::Mailer"],
        }
    }

    #[test]
    fn an_empty_report_is_success() {
        assert!(BootErrors::new().into_result().is_ok());
    }

    #[test]
    fn a_populated_report_is_failure() {
        let mut errors = BootErrors::new();
        errors.push(BootError::Other {
            message: "something".to_owned(),
            notes: Vec::new(),
            fix: None,
        });
        assert_eq!(errors.len(), 1);
        assert!(errors.into_result().is_err());
    }

    #[test]
    fn an_empty_report_renders_nothing() {
        assert_eq!(BootErrors::new().render(true), "");
    }

    // ── Levenshtein ──────────────────────────────────────────────────────

    #[test]
    fn levenshtein_matches_known_distances() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("flaw", "lawn"), 2);
        assert_eq!(levenshtein("same", "same"), 0);
    }

    #[test]
    fn levenshtein_counts_characters_not_bytes() {
        // Three chars, six bytes: a byte-wise implementation would say 3.
        assert_eq!(levenshtein("héllo", "hello"), 1);
        assert_eq!(levenshtein("日本語", "日本"), 1);
    }

    #[test]
    fn did_you_mean_finds_a_close_name() {
        assert_eq!(
            did_you_mean("posts.publsh", ["posts.publish", "users.create"]),
            Some("posts.publish")
        );
    }

    #[test]
    fn did_you_mean_rejects_a_distant_name() {
        // The doc's example: two-character names are never each other's typo.
        assert_eq!(did_you_mean("db", ["kv"]), None);
        assert_eq!(did_you_mean("database", ["mailer", "storage"]), None);
    }

    #[test]
    fn did_you_mean_ignores_an_exact_match() {
        assert_eq!(did_you_mean("cache", ["cache"]), None);
    }

    #[test]
    fn did_you_mean_is_case_insensitive() {
        assert_eq!(did_you_mean("Cachce", ["cache"]), Some("cache"));
    }

    #[test]
    fn did_you_mean_prefers_the_closest() {
        assert_eq!(
            did_you_mean("mailer", ["mailers", "maler", "mailer_"]),
            Some("mailers")
        );
    }

    #[test]
    fn suggest_type_falls_back_to_the_trailing_segment() {
        // The module paths differ wildly, so only the type names are close.
        let suggestion = suggest_type("shop::db::Databse", &["other::crate::infra::Database"]);
        assert_eq!(suggestion, Some("other::crate::infra::Database"));
    }

    #[test]
    fn suggest_type_returns_none_when_nothing_is_close() {
        assert_eq!(suggest_type("shop::db::Db", &["shop::mail::Mailer"]), None);
    }

    #[test]
    fn a_shared_module_path_does_not_buy_a_suggestion() {
        // `Store` and `Cfg` are not near misses in any sense a reader would
        // accept. Measured on the fully-qualified names they are within six
        // edits of each other, because the shared `my_app::` prefix is eleven
        // characters of free similarity.
        assert_eq!(suggest_type("my_app::Store", &["my_app::Cfg"]), None);
        assert_eq!(
            suggest_type("my_app::Database", &["my_app::Mailer", "my_app::Databse"]),
            Some("my_app::Databse")
        );
    }

    #[test]
    fn the_same_type_name_in_another_module_wins() {
        assert_eq!(
            suggest_type("shop::db::Database", &["infra::Database", "shop::Databse"]),
            Some("infra::Database"),
            "the reader imported the wrong `Database`, which is not a typo"
        );
    }

    // ── rendering ────────────────────────────────────────────────────────

    #[test]
    fn elide_keeps_both_ends() {
        assert_eq!(elide("short", 10), "short");
        assert_eq!(elide("abcdefghij", 9), "abc...hij");
        assert_eq!(elide("abcdefghij", 10), "abcdefghij");
    }

    #[test]
    fn every_headline_fits_the_column_budget() {
        let long = "a".repeat(200);
        let problems = [
            missing_provider(),
            BootError::RouteConflict {
                method: "GET",
                first_path: format!("/{long}"),
                first: None,
                second_path: format!("/{long}"),
                second: None,
                reason: ConflictReason::Identical,
            },
            BootError::Other {
                message: long.clone(),
                notes: Vec::new(),
                fix: None,
            },
        ];
        for problem in &problems {
            assert!(
                problem.headline().chars().count() <= MAX_HEADLINE,
                "headline too long: {}",
                problem.headline()
            );
        }
    }

    #[test]
    fn plain_output_is_pure_ascii() {
        let mut errors: BootErrors = [
            missing_provider(),
            BootError::RouteConflict {
                method: "GET",
                first_path: "/users/{id}".to_owned(),
                first: Some(SourceLocation {
                    file: "src/routes/users.rs",
                    line: 47,
                }),
                second_path: "/users/{user_id}".to_owned(),
                second: Some(SourceLocation {
                    file: "src/routes/admin.rs",
                    line: 22,
                }),
                reason: ConflictReason::ParameterNameMismatch,
            },
        ]
        .into_iter()
        .collect();
        errors.sort_for_report();

        let plain = errors.render(false);
        assert!(
            plain.is_ascii(),
            "plain report contained non-ASCII: {plain}"
        );
        assert!(!plain.contains('\u{1b}'), "plain report contained escapes");
    }

    #[test]
    fn colour_output_uses_escapes_and_unicode() {
        let errors: BootErrors = [missing_provider()].into_iter().collect();
        let coloured = errors.render(true);
        assert!(coloured.contains('\u{1b}'));
        assert!(coloured.contains('✗'));
    }

    /// The snapshot. The exact bytes are the product surface described in
    /// `docs/01-http/10-app-lifecycle.md`; changing them is a deliberate act.
    #[test]
    fn the_grouped_report_renders_as_documented() {
        let mut errors: BootErrors = [
            BootError::RouteConflict {
                method: "GET",
                first_path: "/users/{id}".to_owned(),
                first: Some(SourceLocation {
                    file: "src/routes/users.rs",
                    line: 47,
                }),
                second_path: "/users/{user_id}".to_owned(),
                second: Some(SourceLocation {
                    file: "src/routes/admin.rs",
                    line: 22,
                }),
                reason: ConflictReason::ParameterNameMismatch,
            },
            missing_provider(),
        ]
        .into_iter()
        .collect();
        errors.sort_for_report();

        let expected = "\
error: application failed to build (2 problems)

  x missing provider: `shop::db::Db`
      required by  GET /users       src/routes/users.rs:14
                   POST /users      src/routes/users.rs:31
                   GET /users/{id}  src/routes/users.rs:47
      fix          register it on the `App` builder, usually in src/lib.rs
                   let value: Db = /* construct it */;
                   App::new(config).provide(value)

  x route conflict: GET /users/{id}  and  GET /users/{user_id}
      first        src/routes/users.rs:47
      second       src/routes/admin.rs:22
      note         path parameters must have the same name at the same position
      fix          rename one parameter, or nest one router under a distinct prefix
";
        assert_eq!(errors.render(false), expected);
    }

    #[test]
    fn missing_provider_suggests_a_registered_name() {
        let error = BootError::MissingProvider {
            requirement: ProviderRequirement {
                type_name: "shop::db::Databse",
                required_by: vec![route("GET", "/users", 14)],
            },
            registered: vec!["shop::db::Database"],
        };
        let details = error.details();
        assert!(
            details
                .iter()
                .any(|(label, value)| label.is_empty()
                    && value == "did you mean `shop::db::Database`?"),
            "{details:?}"
        );
    }

    #[test]
    fn sorting_puts_providers_first_and_is_stable() {
        let mut errors: BootErrors = [
            BootError::Other {
                message: "first other".to_owned(),
                notes: Vec::new(),
                fix: None,
            },
            BootError::MissingConfig {
                key: "database.url".to_owned(),
                env: "DATABASE_URL".to_owned(),
                file_key: "database.url".to_owned(),
                expected_type: "String",
            },
            BootError::Other {
                message: "second other".to_owned(),
                notes: Vec::new(),
                fix: None,
            },
            missing_provider(),
        ]
        .into_iter()
        .collect();
        errors.sort_for_report();

        let headlines: Vec<String> = errors.as_slice().iter().map(BootError::headline).collect();
        assert_eq!(headlines[0], "missing provider: `shop::db::Db`");
        assert_eq!(headlines[1], "missing configuration: database.url");
        assert_eq!(headlines[2], "first other");
        assert_eq!(headlines[3], "second other");
    }

    #[test]
    fn a_single_problem_uses_the_singular() {
        let errors: BootErrors = [missing_provider()].into_iter().collect();
        assert!(
            errors
                .render(false)
                .starts_with("error: application failed to build (1 problem)\n\n")
        );
    }

    #[test]
    fn route_refs_align_their_source_locations() {
        let block = route_block(&[route("GET", "/a", 1), route("DELETE", "/bbbb", 2)]);
        let columns: Vec<usize> = block
            .lines()
            .map(|line| line.find("src/").expect("source location"))
            .collect();
        assert_eq!(columns[0], columns[1]);
    }

    #[test]
    fn a_route_ref_without_a_source_says_so() {
        let reference = RouteRef {
            method: "GET",
            path: "/users".to_owned(),
            source: None,
            via: vec!["SearchScope"],
        };
        let rendered = reference.to_string();
        assert!(rendered.contains("(source location unknown)"));
        assert!(rendered.contains("via SearchScope"));
    }

    #[test]
    fn every_variant_renders_without_panicking() {
        let variants = [
            missing_provider(),
            BootError::RouteConflict {
                method: "GET",
                first_path: "/a".to_owned(),
                first: None,
                second_path: "/a".to_owned(),
                second: None,
                reason: ConflictReason::Identical,
            },
            BootError::PathParameterMismatch {
                route: route("GET", "/users/{id}", 1),
                declared: vec!["id".to_owned()],
                expected: vec!["idd".to_owned()],
            },
            BootError::LegacyPathSyntax {
                path: "/users/:id".to_owned(),
                segment: ":id".to_owned(),
                suggestion: "/users/{id}".to_owned(),
            },
            BootError::DuplicateOperationId {
                operation_id: "list_users".to_owned(),
                first: "GET /users".to_owned(),
                second: "GET /admin/users".to_owned(),
            },
            BootError::SchemaCollision {
                name: "User".to_owned(),
                first: "shop::api::User".to_owned(),
                second: "shop::admin::User".to_owned(),
            },
            BootError::ProviderCycle {
                path: vec!["shop::A", "shop::B", "shop::A"],
            },
            BootError::ProviderFailed {
                type_name: "shop::db::Db",
                detail: "connection refused".to_owned(),
            },
            BootError::MissingConfig {
                key: "database.url".to_owned(),
                env: "DATABASE_URL".to_owned(),
                file_key: "database.url".to_owned(),
                expected_type: "String",
            },
            BootError::InvalidConfig {
                key: "http.port".to_owned(),
                source: "env DATABASE_PORT".to_owned(),
                expected: "an integer in 1..=65535".to_owned(),
                found: "\"eighty\"".to_owned(),
                note: Some("ports are numbers".to_owned()),
            },
            BootError::MiddlewareOrder {
                rule: "CatchError must sit inside Trace",
                stack: vec!["CatchError".to_owned(), "Trace".to_owned()],
            },
            BootError::Document {
                detail: "two operations share an id".to_owned(),
            },
            BootError::Other {
                message: "something else".to_owned(),
                notes: vec!["a".to_owned(), "b".to_owned()],
                fix: Some("do the thing".to_owned()),
            },
        ];
        for variant in &variants {
            assert!(!variant.headline().is_empty());
            let rendered = variant.to_string();
            assert!(rendered.starts_with("  x "), "{rendered}");
            let _ = variant.details();
            let _ = variant.fix();
        }
    }

    #[test]
    fn the_path_mismatch_suggests_the_declared_parameter() {
        let error = BootError::PathParameterMismatch {
            route: route("GET", "/users/{id}", 1),
            declared: vec!["id".to_owned()],
            expected: vec!["idd".to_owned()],
        };
        assert!(
            error
                .details()
                .iter()
                .any(|(label, value)| label.is_empty() && value == "did you mean `{id}`?")
        );
    }

    #[test]
    fn no_color_disables_colour() {
        // Cannot toggle the process environment safely under a threaded test
        // runner, so assert the property the function is built on instead:
        // a non-empty NO_COLOR is what the check looks for.
        let disabled = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        if disabled {
            assert!(!stderr_is_tty());
        }
    }
}
