//! `expand-size` — how many lines each macro emits, against the budgets in
//! `docs/06-reference/62-macro-reference.md`.
//!
//! Rule A5 in `docs/04-devex/42-compile-times.md` puts a line budget on every
//! derive, and the reason is monomorphisation: at 200 endpoints, a derive that
//! quietly doubles in size is measured in seconds of every rebuild, and nobody
//! notices in review because the diff is three lines of `quote!`. So the
//! expansion is measured.
//!
//! | Macro | Budget |
//! | --- | --- |
//! | `#[endpoint]` | ≤ 60 lines per endpoint |
//! | `#[derive(Schema)]` | ≤ 25 lines per field, ≤ 300 lines per type |
//! | `#[derive(Config)]` | ≤ 20 lines per field |
//!
//! # How the expansion is obtained
//!
//! Not with `cargo expand`. `cargo expand` pipes the result through `rustfmt`,
//! so the line count would depend on whether the developer has it installed and
//! on which edition's formatting rules apply — a budget that moves when a tool
//! is upgraded is not a budget. This calls `rustc -Zunpretty=expanded` directly
//! (with `RUSTC_BOOTSTRAP=1`, as `cargo expand` itself does on a stable
//! toolchain), whose pretty-printer is part of the compiler and therefore
//! consistent for a given `rustc`. The version is printed with the numbers.
//!
//! One consequence, stated plainly: rustc's printer wraps at its own width, so
//! these counts are a little higher than the same code would be after
//! `rustfmt`. They are comparable with each other and with the committed
//! baseline, which is what a budget needs.
//!
//! # How lines are attributed to a macro
//!
//! The expansion is parsed into items — `path_refs`-style structural parsing,
//! not regular expressions — and each item is attributed by what it *is*:
//! anything naming `__moso_op_*` belongs to `#[endpoint]`, an
//! `impl ::moso::__private::Config for T` belongs to `#[derive(Config)]`, and so
//! on. Two attributions are heuristics and are labelled as such in the output:
//! the always-on assertion blocks (`const _: () = { … __moso_assert_* … }`) are
//! attributed to the item they follow, and the `#[middleware]` layer/service
//! pair is found by following the generated `Layer` impl.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bail;
use crate::util::{Cmd, Result, ui};

/// The per-endpoint budget for `#[endpoint]`.
///
/// ```
/// assert_eq!(xtask::expand::ENDPOINT_BUDGET, 60);
/// ```
pub const ENDPOINT_BUDGET: usize = 60;

/// The per-field budget for `#[derive(Schema)]`.
///
/// ```
/// assert_eq!(xtask::expand::SCHEMA_PER_FIELD_BUDGET, 25);
/// ```
pub const SCHEMA_PER_FIELD_BUDGET: usize = 25;

/// The per-type budget for `#[derive(Schema)]`.
///
/// ```
/// assert_eq!(xtask::expand::SCHEMA_TOTAL_BUDGET, 300);
/// ```
pub const SCHEMA_TOTAL_BUDGET: usize = 300;

/// The per-field budget for `#[derive(Config)]`.
///
/// ```
/// assert_eq!(xtask::expand::CONFIG_PER_FIELD_BUDGET, 20);
/// ```
pub const CONFIG_PER_FIELD_BUDGET: usize = 20;

/// Which macro a generated item came from.
///
/// ```
/// use xtask::expand::Origin;
///
/// assert_eq!(Origin::Endpoint.label(), "#[endpoint]");
/// assert!(Origin::Endpoint.is_moso());
/// assert!(!Origin::Handwritten.is_moso());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// `#[endpoint]`, including the companion type and its derives.
    Endpoint,
    /// `#[derive(Schema)]` — the `Schema`, `Validate` and `Describe` impls.
    Schema,
    /// `#[derive(Config)]`.
    Config,
    /// `#[derive(Dependency)]`.
    Dependency,
    /// `#[derive(Error)]` and `#[derive(Responder)]`.
    Error,
    /// `#[middleware]` — the layer and service pair.
    Middleware,
    /// `serde`'s own derives, measured separately because they are not ours.
    Serde,
    /// The standard library's derives: `Debug`, `Clone`, `PartialEq`, …
    StdDerive,
    /// Code the user wrote, echoed by the expansion.
    Handwritten,
}

impl Origin {
    /// How this origin is named in a report.
    ///
    /// ```
    /// use xtask::expand::Origin;
    ///
    /// assert_eq!(Origin::Schema.label(), "#[derive(Schema)]");
    /// assert_eq!(Origin::StdDerive.label(), "std derives");
    /// ```
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Endpoint => "#[endpoint]",
            Self::Schema => "#[derive(Schema)]",
            Self::Config => "#[derive(Config)]",
            Self::Dependency => "#[derive(Dependency)]",
            Self::Error => "#[derive(Error)]/#[derive(Responder)]",
            Self::Middleware => "#[middleware]",
            Self::Serde => "serde derives",
            Self::StdDerive => "std derives",
            Self::Handwritten => "hand-written",
        }
    }

    /// Whether this is a Moso macro, and therefore Moso's cost to control.
    ///
    /// ```
    /// use xtask::expand::Origin;
    ///
    /// assert!(Origin::Middleware.is_moso());
    /// assert!(!Origin::Serde.is_moso());
    /// assert!(!Origin::StdDerive.is_moso());
    /// ```
    #[must_use]
    pub fn is_moso(self) -> bool {
        matches!(
            self,
            Self::Endpoint
                | Self::Schema
                | Self::Config
                | Self::Dependency
                | Self::Error
                | Self::Middleware
        )
    }
}

/// One item in the expanded source.
///
/// ```
/// use xtask::expand::{Item, Origin};
///
/// let item = Item { module: "routes::posts".into(), header: "pub struct __moso_op_list;".into(),
///     start: 10, end: 10, origin: Origin::Endpoint, subject: Some("__moso_op_list".into()) };
/// assert_eq!(item.lines(), 1);
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Item {
    /// The `::`-joined module path the item is in.
    pub module: String,
    /// The item's first line, trimmed — enough to recognise it.
    pub header: String,
    /// The first line of the item, including its attributes, 1-based.
    pub start: usize,
    /// The last line of the item, 1-based and inclusive.
    pub end: usize,
    /// Which macro produced it.
    pub origin: Origin,
    /// The type the item is about, when one can be named.
    pub subject: Option<String>,
}

impl Item {
    /// How many lines the item occupies.
    ///
    /// ```
    /// use xtask::expand::{Item, Origin};
    ///
    /// let item = Item { module: String::new(), header: "impl A {".into(), start: 4, end: 9,
    ///     origin: Origin::Handwritten, subject: None };
    /// assert_eq!(item.lines(), 6);
    /// ```
    #[must_use]
    pub fn lines(&self) -> usize {
        self.end.saturating_sub(self.start) + 1
    }
}

/// A type declared in the expansion, with its field count.
///
/// ```
/// use xtask::expand::TypeShape;
///
/// let shape = TypeShape { name: "CreatePost".into(), module: "models::post".into(), fields: 5 };
/// assert_eq!(shape.fields, 5);
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TypeShape {
    /// The type's name.
    pub name: String,
    /// The module it is declared in.
    pub module: String,
    /// How many fields or variants it has. Zero for a unit type.
    pub fields: usize,
}

/// What one macro cost, and whether that is within budget.
///
/// ```
/// use xtask::expand::{MacroCost, Origin};
///
/// let cost = MacroCost { origin: Origin::Endpoint, items: 7, lines: 400, units: 6,
///     unit_name: "endpoint".into(), per_unit: 66.7, budget: Some(60.0),
///     budget_name: "lines per endpoint".into(), within_budget: false, offenders: Vec::new() };
/// assert!(!cost.within_budget);
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MacroCost {
    /// Which macro.
    pub origin: Origin,
    /// How many items it generated.
    pub items: usize,
    /// How many lines those items occupy.
    pub lines: usize,
    /// How many things it was applied to — endpoints, or fields.
    pub units: usize,
    /// What a unit is.
    pub unit_name: String,
    /// Lines per unit.
    pub per_unit: f64,
    /// The budget, when the macro has one.
    pub budget: Option<f64>,
    /// What the budget is a budget on.
    pub budget_name: String,
    /// Whether the budget is met.
    pub within_budget: bool,
    /// The specific types or endpoints that are over, with their numbers.
    pub offenders: Vec<String>,
}

/// The whole measurement.
///
/// ```
/// use xtask::expand::Report;
///
/// let report = Report { package: "example-crud".into(), rustc: "rustc 1.97.1".into(),
///     total_lines: 5928, generated_lines: 3000, macros: Vec::new(), types: Vec::new(),
///     items: Vec::new() };
/// assert!(report.within_budget());
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Report {
    /// The package that was expanded.
    pub package: String,
    /// The compiler whose pretty-printer produced the lines.
    pub rustc: String,
    /// Lines in the whole expansion.
    pub total_lines: usize,
    /// Lines attributable to a Moso macro.
    pub generated_lines: usize,
    /// One entry per macro that appears.
    pub macros: Vec<MacroCost>,
    /// Every type the expansion declares, with its field count.
    pub types: Vec<TypeShape>,
    /// Every item, with the macro it was attributed to and its line span. This
    /// is what makes the report actionable: WP-25 needs to know *which* 80 lines
    /// to shorten, not only that there are 155 of them.
    pub items: Vec<Item>,
}

impl Report {
    /// Whether every budgeted macro is within budget.
    ///
    /// ```
    /// # use xtask::expand::Report;
    /// let report = Report { package: "p".into(), rustc: "r".into(), total_lines: 0,
    ///     generated_lines: 0, macros: Vec::new(), types: Vec::new(), items: Vec::new() };
    /// assert!(report.within_budget());
    /// ```
    #[must_use]
    pub fn within_budget(&self) -> bool {
        self.macros
            .iter()
            .all(|cost| cost.budget.is_none() || cost.within_budget)
    }
}

/// Options for one run.
///
/// ```
/// let options = xtask::expand::Options::default();
/// assert_eq!(options.package, "example-crud");
/// ```
#[derive(Clone, Debug)]
pub struct Options {
    /// The package whose library target is expanded.
    pub package: String,
    /// Write the machine-readable report here.
    pub json: Option<PathBuf>,
    /// Write the expanded source here, for reading.
    pub save: Option<PathBuf>,
    /// Build artefacts directory; `None` means the workspace's own `target/`.
    pub target_dir: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            package: "example-crud".to_owned(),
            json: None,
            save: None,
            target_dir: None,
        }
    }
}

/// Expands the package, attributes the lines, and enforces the budgets.
///
/// ```no_run
/// let within = xtask::expand::run(&xtask::expand::Options::default())?;
/// assert!(within);
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn run(options: &Options) -> Result<bool> {
    let root = crate::util::workspace_root()?;
    ui::headline("expand-size");

    let expanded = expand(&root, &options.package, options.target_dir.as_deref())?;
    if let Some(path) = &options.save {
        std::fs::write(root.join(path), &expanded)?;
        ui::note(&format!("expansion written to {}", path.display()));
    }
    let rustc = Cmd::new("rustc")
        .arg("--version")
        .capture()
        .map(|output| output.stdout.trim().to_owned())
        .unwrap_or_else(|_| "unknown".to_owned());

    let report = measure(&options.package, &rustc, &expanded);

    println!(
        "  {} expands to {} lines, {} of them generated by a Moso macro ({})",
        report.package, report.total_lines, report.generated_lines, report.rustc
    );
    for cost in &report.macros {
        let line = format!(
            "{:<38} {:>5} lines in {:>3} items, {:>6.1} {}",
            cost.origin.label(),
            cost.lines,
            cost.items,
            cost.per_unit,
            cost.unit_name
        );
        match cost.budget {
            None => ui::note(&line),
            Some(budget) if cost.within_budget => {
                ui::ok(&format!(
                    "{line}  (budget {budget:.0} {})",
                    cost.budget_name
                ));
            }
            Some(budget) => {
                ui::fail(&format!(
                    "{line}  (budget {budget:.0} {})",
                    cost.budget_name
                ));
                for offender in &cost.offenders {
                    ui::note(offender);
                }
            }
        }
    }

    if let Some(path) = &options.json {
        let text = serde_json::to_string_pretty(&report)?;
        std::fs::write(root.join(path), text + "\n")?;
        ui::note(&format!("report written to {}", path.display()));
    }

    Ok(report.within_budget())
}

/// Runs the compiler's own pretty-printer over a package's library target.
///
/// ```no_run
/// let root = xtask::util::workspace_root()?;
/// let expanded = xtask::expand::expand(&root, "example-crud", None)?;
/// assert!(expanded.contains("__moso_op_"));
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn expand(root: &Path, package: &str, target_dir: Option<&Path>) -> Result<String> {
    let mut cmd = Cmd::cargo().cwd(root).env("RUSTC_BOOTSTRAP", "1").args([
        "rustc",
        "--package",
        package,
        "--lib",
        "--profile",
        "check",
    ]);
    if let Some(dir) = target_dir {
        cmd = cmd.args(["--target-dir", &dir.display().to_string()]);
    }
    let cmd = cmd.args(["--", "-Zunpretty=expanded"]);
    let output = cmd.capture()?;
    if !output.ok() {
        bail!(
            "cannot expand {package}\n{}\n    the command was: {}",
            crate::util::indent(&output.stderr_tail(15)),
            cmd.rendered()
        );
    }
    if output.stdout.trim().is_empty() {
        bail!(
            "expanding {package} produced no output; -Zunpretty=expanded may have been rejected \
             by this toolchain (`{}`)",
            cmd.rendered()
        );
    }
    Ok(output.stdout)
}

/// Attributes an expansion's lines and applies the budgets.
///
/// Separated from [`run`] so the whole measurement is testable against a
/// hand-written expansion.
///
/// ```
/// use xtask::expand::{Origin, measure};
///
/// let expanded = "\
/// pub struct __moso_op_list;
/// impl ::moso::__private::Endpoint for __moso_op_list {
///     fn method() {}
/// }
/// ";
/// let report = measure("demo", "rustc 1.97.1", expanded);
/// let endpoint = report.macros.iter().find(|m| m.origin == Origin::Endpoint).expect("found");
/// assert_eq!(endpoint.units, 1, "one endpoint");
/// assert_eq!(endpoint.lines, 4);
/// assert!(endpoint.within_budget);
/// ```
#[must_use]
pub fn measure(package: &str, rustc: &str, expanded: &str) -> Report {
    let items = scan(expanded);
    let types = declared_types(expanded, &items);
    let total_lines = expanded.lines().count();
    let generated_lines: usize = items
        .iter()
        .filter(|item| item.origin.is_moso())
        .map(Item::lines)
        .sum();

    let mut by_origin: BTreeMap<Origin, Vec<&Item>> = BTreeMap::new();
    for item in &items {
        by_origin.entry(item.origin).or_default().push(item);
    }

    let field_count = |name: &str| -> Option<usize> {
        types
            .iter()
            .find(|shape| shape.name == name)
            .map(|shape| shape.fields)
    };

    let mut macros = Vec::new();
    for (origin, group) in by_origin {
        if origin == Origin::Handwritten {
            continue;
        }
        let lines: usize = group.iter().copied().map(Item::lines).sum();
        let subjects: BTreeSet<&str> = group
            .iter()
            .filter_map(|item| item.subject.as_deref())
            .collect();

        // Lines per subject, so an individual offender can be named.
        let mut per_subject: BTreeMap<&str, usize> = BTreeMap::new();
        for item in &group {
            if let Some(subject) = item.subject.as_deref() {
                *per_subject.entry(subject).or_default() += item.lines();
            }
        }

        let (units, unit_name, per_unit, budget, budget_name, within, offenders) = match origin {
            Origin::Endpoint => {
                let units = subjects.len().max(1);
                let offenders: Vec<String> = per_subject
                    .iter()
                    .filter(|(_, lines)| **lines > ENDPOINT_BUDGET)
                    .map(|(subject, lines)| {
                        format!(
                            "{subject} expands to {lines} lines, over the {ENDPOINT_BUDGET}-line \
                             budget for one endpoint"
                        )
                    })
                    .collect();
                (
                    units,
                    "lines/endpoint".to_owned(),
                    lines as f64 / units as f64,
                    Some(ENDPOINT_BUDGET as f64),
                    "lines per endpoint".to_owned(),
                    offenders.is_empty(),
                    offenders,
                )
            }
            Origin::Schema | Origin::Config => {
                let per_field_budget = if origin == Origin::Schema {
                    SCHEMA_PER_FIELD_BUDGET
                } else {
                    CONFIG_PER_FIELD_BUDGET
                };
                let mut fields = 0_usize;
                let mut offenders = Vec::new();
                for (subject, subject_lines) in &per_subject {
                    let subject_fields = field_count(subject).unwrap_or(0);
                    fields += subject_fields;
                    if subject_fields > 0 {
                        let ratio = *subject_lines as f64 / subject_fields as f64;
                        if ratio > per_field_budget as f64 {
                            offenders.push(format!(
                                "{subject}: {subject_lines} lines over {subject_fields} fields = \
                                 {ratio:.1} lines/field, budget {per_field_budget}"
                            ));
                        }
                    }
                    if origin == Origin::Schema && *subject_lines > SCHEMA_TOTAL_BUDGET {
                        offenders.push(format!(
                            "{subject}: {subject_lines} lines in total, budget \
                             {SCHEMA_TOTAL_BUDGET}"
                        ));
                    }
                }
                let units = fields.max(1);
                (
                    fields,
                    "lines/field".to_owned(),
                    lines as f64 / units as f64,
                    Some(per_field_budget as f64),
                    "lines per field".to_owned(),
                    offenders.is_empty(),
                    offenders,
                )
            }
            _ => {
                let units = subjects.len().max(1);
                (
                    subjects.len(),
                    "lines/type".to_owned(),
                    lines as f64 / units as f64,
                    None,
                    String::new(),
                    true,
                    Vec::new(),
                )
            }
        };

        macros.push(MacroCost {
            origin,
            items: group.len(),
            lines,
            units,
            unit_name,
            per_unit,
            budget,
            budget_name,
            within_budget: within,
            offenders,
        });
    }

    Report {
        package: package.to_owned(),
        rustc: rustc.to_owned(),
        total_lines,
        generated_lines,
        macros,
        types,
        items,
    }
}

/// Splits an expansion into top-level items and attributes each one.
///
/// ```
/// use xtask::expand::{Origin, scan};
///
/// let items = scan("\
/// pub mod models {
///     pub struct Post {
///         pub id: u32,
///     }
///     impl ::moso::__private::Schema for Post {
///         fn schema_name() {}
///     }
/// }
/// ");
/// assert_eq!(items.len(), 2, "the module's closing brace is not an item");
/// assert_eq!(items[0].module, "models");
/// assert_eq!(items[0].origin, Origin::Handwritten);
/// assert_eq!(items[1].origin, Origin::Schema);
/// assert_eq!(items[1].subject.as_deref(), Some("Post"));
/// ```
#[must_use]
pub fn scan(expanded: &str) -> Vec<Item> {
    let lines: Vec<&str> = expanded.lines().collect();
    let mut items: Vec<Item> = Vec::new();
    let mut modules: Vec<(String, i64)> = Vec::new();
    let mut depth: i64 = 0;
    let mut pending_start: Option<usize> = None;
    let mut index = 0_usize;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();

        while let Some((_, module_depth)) = modules.last() {
            if depth < *module_depth {
                modules.pop();
            } else {
                break;
            }
        }
        let item_depth = modules.last().map_or(0, |(_, d)| *d);

        if trimmed.is_empty() {
            depth += brace_delta(line);
            index += 1;
            continue;
        }
        if depth != item_depth {
            depth += brace_delta(line);
            index += 1;
            continue;
        }
        if trimmed.starts_with('}') || trimmed.starts_with(')') {
            // The closing brace of the enclosing module, which is not an item.
            depth += brace_delta(line);
            index += 1;
            continue;
        }
        if trimmed.starts_with("#[") || trimmed.starts_with("#![") || trimmed.starts_with("//") {
            if pending_start.is_none() {
                pending_start = Some(index);
            }
            depth += brace_delta(line);
            index += 1;
            continue;
        }

        if let Some(name) = module_name(trimmed) {
            let path = match modules.last() {
                Some((parent, _)) => format!("{parent}::{name}"),
                None => name,
            };
            depth += brace_delta(line);
            modules.push((path, depth));
            pending_start = None;
            index += 1;
            continue;
        }

        // An item: consume lines until the brace depth returns to this level and
        // the line looks like the end of an item.
        let start = pending_start.take().unwrap_or(index);
        let mut end = index;
        loop {
            depth += brace_delta(lines[end]);
            let closed = depth <= item_depth && ends_item(lines[end].trim_end());
            if closed || end + 1 >= lines.len() {
                break;
            }
            end += 1;
        }
        let module = modules
            .last()
            .map(|(path, _)| path.clone())
            .unwrap_or_default();
        let body = lines[start..=end].join("\n");
        // Everything from the item's first line up to the opening brace: the
        // whole declaration even when rustc's printer wrapped it, and never
        // anything from inside the body. Attributes and doc comments are
        // excluded, so `decl` always starts with the item's own first token.
        let signature = lines[index..=end].join("\n");
        let decl = signature
            .split('{')
            .next()
            .unwrap_or(&signature)
            .trim()
            .to_owned();
        let decl = if decl.is_empty() {
            trimmed.to_owned()
        } else {
            decl
        };
        let (origin, subject) = attribute(&decl, &body);
        items.push(Item {
            module,
            header: trimmed.to_owned(),
            start: start + 1,
            end: end + 1,
            origin,
            subject,
        });
        index = end + 1;
    }

    resolve_heuristics(&mut items);
    items
}

/// Whether a line can be the last line of an item.
///
/// ```
/// use xtask::expand::ends_item;
///
/// assert!(ends_item("pub const A: u8 = 1;"));
/// assert!(ends_item("}"));
/// assert!(ends_item("impl Copy for X { }"));
/// assert!(!ends_item("impl<T> Layer<T> for"));
/// assert!(!ends_item("pub const A: u8 ="));
/// ```
#[must_use]
pub fn ends_item(line: &str) -> bool {
    let trimmed = line.trim_end();
    trimmed.ends_with(';') || trimmed.ends_with('}') || trimmed.ends_with("};")
}

/// The name of the module a line opens, if it opens one.
///
/// ```
/// use xtask::expand::module_name;
///
/// assert_eq!(module_name("pub mod routes {").as_deref(), Some("routes"));
/// assert_eq!(module_name("mod tests {").as_deref(), Some("tests"));
/// assert_eq!(module_name("pub mod empty;"), None, "a file module has no body here");
/// assert_eq!(module_name("pub struct Mod {"), None);
/// ```
#[must_use]
pub fn module_name(trimmed: &str) -> Option<String> {
    let rest = trimmed
        .strip_prefix("pub mod ")
        .or_else(|| trimmed.strip_prefix("mod "))?;
    if !trimmed.ends_with('{') {
        return None;
    }
    let name = rest.trim_end_matches(['{', ' ']).trim();
    if name.is_empty() || name.contains(' ') {
        return None;
    }
    Some(name.to_owned())
}

/// Attributes one item to the macro that produced it.
///
/// ```
/// use xtask::expand::{Origin, attribute};
///
/// assert_eq!(attribute("pub struct __moso_op_list;", "").0, Origin::Endpoint);
/// assert_eq!(attribute("impl ::moso::__private::Config for AppConfig {", "").0, Origin::Config);
/// assert_eq!(attribute("impl ::core::fmt::Debug for Post {", "").0, Origin::StdDerive);
/// assert_eq!(attribute("pub struct Post {", "").0, Origin::Handwritten);
/// assert_eq!(
///     attribute("impl ::moso::__private::Schema for Post {", "").1.as_deref(),
///     Some("Post"),
/// );
/// ```
#[must_use]
pub fn attribute(header: &str, body: &str) -> (Origin, Option<String>) {
    if header.contains("__moso_op_") {
        return (Origin::Endpoint, moso_op_name(header));
    }
    for (marker, origin) in [
        ("::moso::__private::Schema for", Origin::Schema),
        ("::moso::__private::Validate for", Origin::Schema),
        ("::moso::__private::Config for", Origin::Config),
        ("::moso::__private::Dependency for", Origin::Dependency),
        // `Describe` is emitted by `#[derive(Schema)]` *and* by
        // `#[derive(Error)]`/`#[derive(Responder)]`. Attributed to `Schema`
        // here and moved in `resolve_heuristics` when the type has no `Schema`
        // impl to belong to.
        ("::moso::__private::Describe for", Origin::Schema),
    ] {
        if header.contains(marker) {
            return (origin, Some(impl_subject(header)));
        }
    }
    if header.contains("for ::moso::__private::Error") {
        let subject = header
            .split("From<")
            .nth(1)
            .and_then(|rest| rest.split('>').next())
            .map(str::to_owned);
        return (Origin::Error, subject);
    }
    if header.contains("::moso::__private::tower::Layer<") {
        return (Origin::Middleware, Some(impl_subject(header)));
    }
    if header.starts_with("const _: ()") {
        let origin = if body.contains("_serde") {
            Origin::Serde
        } else if body.contains("__moso_assert") {
            // Resolved against the previous item in `resolve_heuristics`.
            Origin::Endpoint
        } else {
            Origin::Handwritten
        };
        return (origin, None);
    }
    if header.contains("impl ::core::") || header.contains("impl ::alloc::") {
        return (Origin::StdDerive, Some(impl_subject(header)));
    }
    if header.starts_with("unsafe impl ::core::") {
        return (Origin::StdDerive, Some(impl_subject(header)));
    }
    (Origin::Handwritten, None)
}

/// The assertion blocks and the `#[middleware]` pair need context, which only
/// exists once every item is known.
fn resolve_heuristics(items: &mut [Item]) {
    // 0. A `Describe` impl for a type with no `Schema` impl came from
    //    `#[derive(Error)]` or `#[derive(Responder)]`, not from
    //    `#[derive(Schema)]`.
    let schema_types: BTreeSet<String> = items
        .iter()
        .filter(|item| item.header.contains("::moso::__private::Schema for"))
        .filter_map(|item| item.subject.clone())
        .collect();
    for item in items.iter_mut() {
        if item.header.contains("::moso::__private::Describe for")
            && item
                .subject
                .as_ref()
                .is_none_or(|subject| !schema_types.contains(subject))
        {
            item.origin = Origin::Error;
        }
    }

    // 1. An assertion block belongs to whatever the item before it belonged to.
    let mut previous: Option<(Origin, Option<String>)> = None;
    for item in items.iter_mut() {
        let is_assertion = item.header.starts_with("const _: ()")
            && item.origin == Origin::Endpoint
            && item.subject.is_none();
        if is_assertion {
            if let Some((origin, subject)) = previous.clone()
                && origin.is_moso()
            {
                item.origin = origin;
                item.subject = subject;
            }
        } else {
            previous = Some((item.origin, item.subject.clone()));
        }
    }

    // 2. `#[middleware]` generates `XLayer`, `XService` and their derives. The
    //    `Layer` impl names the layer type; everything about that type and its
    //    service is the middleware's cost, not the user's.
    let layers: BTreeSet<String> = items
        .iter()
        .filter(|item| item.origin == Origin::Middleware)
        .filter_map(|item| item.subject.clone())
        .collect();
    if layers.is_empty() {
        return;
    }
    let services: BTreeSet<String> = layers
        .iter()
        .map(|layer| format!("{}Service", layer.trim_end_matches("Layer")))
        .collect();
    for item in items.iter_mut() {
        if item.origin == Origin::Middleware {
            continue;
        }
        let names = layers.iter().chain(services.iter());
        for name in names {
            if item.header.contains(name) {
                item.origin = Origin::Middleware;
                item.subject = Some(name.clone());
                break;
            }
        }
    }
}

fn moso_op_name(header: &str) -> Option<String> {
    let start = header.find("__moso_op_")?;
    let rest = &header[start..];
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

/// The type an `impl … for …` declaration is about, tolerating the line breaks
/// rustc's printer inserts.
///
/// ```
/// use xtask::expand::impl_subject;
///
/// assert_eq!(impl_subject("impl Schema for Post {"), "Post");
/// assert_eq!(impl_subject("impl<I> Layer<I> for\n    ObserveLayer {"), "ObserveLayer");
/// assert_eq!(impl_subject("impl Debug for Page<T> {"), "Page");
/// ```
#[must_use]
pub fn impl_subject(decl: &str) -> String {
    // Split on the `for` keyword rather than on the substring, so `Formatter`
    // and `information` cannot match.
    let after_for = decl
        .split_whitespace()
        .skip_while(|token| *token != "for")
        .nth(1)
        .unwrap_or("");
    after_for
        .trim()
        .trim_end_matches(['{', ' ', ','])
        .split('<')
        .next()
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// Finds every `struct` and `enum` the expansion declares and counts its fields.
///
/// Field counts come from the expansion rather than from the source because the
/// expansion is what the budget is about, and because a `#[cfg]`-ed-out field
/// costs nothing and should not count.
///
/// ```
/// use xtask::expand::{declared_types, scan};
///
/// let expanded = "\
/// pub struct Post {
///     pub id: u32,
///     pub title: String,
/// }
/// pub enum Kind {
///     Draft,
///     Live,
/// }
/// ";
/// let items = scan(expanded);
/// let types = declared_types(expanded, &items);
/// assert_eq!(types.len(), 2);
/// assert_eq!(types[0].fields, 2);
/// assert_eq!(types[1].fields, 2, "variants count as fields");
/// ```
#[must_use]
pub fn declared_types(expanded: &str, items: &[Item]) -> Vec<TypeShape> {
    let lines: Vec<&str> = expanded.lines().collect();
    let mut shapes = Vec::new();
    for item in items {
        let Some(name) = declared_type_name(&item.header) else {
            continue;
        };
        let body = &lines[item.start - 1..item.end];
        let fields = if item.header.contains('(') && !item.header.contains('{') {
            // A tuple struct: `pub struct Editor(pub Actor);`
            count_tuple_members(&item.header)
        } else {
            count_members(body)
        };
        shapes.push(TypeShape {
            name,
            module: item.module.clone(),
            fields,
        });
    }
    shapes
}

fn declared_type_name(header: &str) -> Option<String> {
    let rest = header
        .strip_prefix("pub struct ")
        .or_else(|| header.strip_prefix("struct "))
        .or_else(|| header.strip_prefix("pub enum "))
        .or_else(|| header.strip_prefix("enum "))?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

fn count_members(body: &[&str]) -> usize {
    let mut count = 0;
    let mut depth = 0_i64;
    for line in body {
        let before = depth;
        depth += brace_delta(line);
        if before != 1 {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("#[")
            || trimmed.starts_with('}')
            || trimmed.starts_with(')')
        {
            continue;
        }
        // A named field (`id: u32,`), a struct variant's opening line
        // (`Missing {`) or a unit variant (`Draft,`).
        if trimmed.ends_with(',') || trimmed.ends_with('{') || trimmed.ends_with('(') {
            count += 1;
        }
    }
    count
}

/// Counts the members of a tuple struct or newtype from its declaration.
///
/// ```
/// use xtask::expand::count_tuple_members;
///
/// assert_eq!(count_tuple_members("pub struct Editor(pub Actor);"), 1);
/// assert_eq!(count_tuple_members("pub struct Pair(u8, u8);"), 2);
/// assert_eq!(count_tuple_members("pub struct Empty();"), 0);
/// assert_eq!(count_tuple_members("pub struct Nested(Vec<(u8, u8)>);"), 1);
/// ```
#[must_use]
pub fn count_tuple_members(decl: &str) -> usize {
    let Some(open) = decl.find('(') else { return 0 };
    let inner = &decl[open + 1..];
    let mut depth = 0_i64;
    let mut members = 0_usize;
    let mut saw_content = false;
    for character in inner.chars() {
        match character {
            '(' | '<' | '[' => depth += 1,
            ')' if depth == 0 => break,
            ')' | '>' | ']' => depth -= 1,
            ',' if depth == 0 => members += 1,
            character if !character.is_whitespace() => saw_content = true,
            _ => {}
        }
    }
    if saw_content { members + 1 } else { 0 }
}

/// The net change in brace depth a line causes, ignoring braces inside string
/// and character literals and after a line comment.
///
/// ```
/// use xtask::expand::brace_delta;
///
/// assert_eq!(brace_delta("impl A {"), 1);
/// assert_eq!(brace_delta("}"), -1);
/// assert_eq!(brace_delta("let s = \"{{\";"), 0, "braces in a string do not count");
/// assert_eq!(brace_delta("// a comment with {"), 0);
/// assert_eq!(brace_delta("let c = '{';"), 0);
/// assert_eq!(brace_delta("fn f<'a>(x: &'a str) {"), 1, "a lifetime is not a char literal");
/// assert_eq!(brace_delta("let r = r#\"a { b\"#;"), 0, "raw strings too");
/// ```
#[must_use]
pub fn brace_delta(line: &str) -> i64 {
    let bytes: Vec<char> = line.chars().collect();
    let mut delta = 0_i64;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            '/' if index + 1 < bytes.len() && bytes[index + 1] == '/' => break,
            '{' => delta += 1,
            '}' => delta -= 1,
            'r' if index + 1 < bytes.len()
                && (bytes[index + 1] == '"' || bytes[index + 1] == '#') =>
            {
                if let Some(next) = skip_raw_string(&bytes, index) {
                    index = next;
                    continue;
                }
            }
            '"' => {
                index = skip_string(&bytes, index);
                continue;
            }
            '\'' => {
                index = skip_char_or_lifetime(&bytes, index);
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    delta
}

fn skip_string(chars: &[char], start: usize) -> usize {
    let mut index = start + 1;
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 2,
            '"' => return index + 1,
            _ => index += 1,
        }
    }
    index
}

fn skip_raw_string(chars: &[char], start: usize) -> Option<usize> {
    // `r` `#`* `"` … `"` `#`*
    let mut index = start + 1;
    let mut hashes = 0;
    while index < chars.len() && chars[index] == '#' {
        hashes += 1;
        index += 1;
    }
    if index >= chars.len() || chars[index] != '"' {
        return None;
    }
    index += 1;
    while index < chars.len() {
        if chars[index] == '"' {
            let mut closing = 0;
            while index + 1 + closing < chars.len() && chars[index + 1 + closing] == '#' {
                closing += 1;
            }
            if closing >= hashes {
                return Some(index + 1 + hashes);
            }
        }
        index += 1;
    }
    Some(index)
}

fn skip_char_or_lifetime(chars: &[char], start: usize) -> usize {
    // `'a` is a lifetime; `'x'` is a character. The difference is whether the
    // quote closes.
    let next = chars.get(start + 1).copied();
    let after = chars.get(start + 2).copied();
    match (next, after) {
        (Some('\\'), _) => {
            let mut index = start + 2;
            while index < chars.len() && chars[index] != '\'' {
                index += 1;
            }
            index + 1
        }
        (Some(_), Some('\'')) => start + 3,
        _ => start + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPANSION: &str = r#"#![feature(prelude_import)]
pub mod models {
    /// A post.
    pub struct CreatePost {
        pub title: String,
        pub body: String,
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for CreatePost {
        fn fmt(&self) {}
    }
    impl ::moso::__private::Validate for CreatePost {
        fn validate(&self) {}
    }
    impl ::moso::__private::Schema for CreatePost {
        fn schema_name() {}
        fn json_schema() {}
    }
}
pub mod routes {
    pub struct __moso_op_create;
    impl ::moso::__private::Endpoint for __moso_op_create {
        fn method() {}
    }
    const _: () =
        {
            fn __moso_assert_extract<T>() {}
        };
    pub async fn create() -> Result<()> { Ok(()) }
}
"#;

    #[test]
    fn modules_are_tracked_through_nesting() {
        let items = scan(EXPANSION);
        let modules: BTreeSet<&str> = items.iter().map(|item| item.module.as_str()).collect();
        assert!(modules.contains("models"), "{modules:?}");
        assert!(modules.contains("routes"), "{modules:?}");
    }

    #[test]
    fn a_doc_comment_belongs_to_the_item_it_precedes() {
        let items = scan(EXPANSION);
        let post = items
            .iter()
            .find(|item| item.header.starts_with("pub struct CreatePost"))
            .expect("the struct");
        assert_eq!(
            post.lines(),
            5,
            "the doc comment, the header, two fields and the brace"
        );
    }

    #[test]
    fn schema_validate_and_describe_all_count_as_the_schema_derive() {
        let items = scan(EXPANSION);
        let schema: Vec<&Item> = items
            .iter()
            .filter(|item| item.origin == Origin::Schema)
            .collect();
        assert_eq!(schema.len(), 2, "Validate and Schema");
        assert!(
            schema
                .iter()
                .all(|item| item.subject.as_deref() == Some("CreatePost"))
        );
    }

    #[test]
    fn an_assertion_block_is_attributed_to_the_item_before_it() {
        let items = scan(EXPANSION);
        let assertion = items
            .iter()
            .find(|item| item.header.starts_with("const _: ()"))
            .expect("the assertion block");
        assert_eq!(assertion.origin, Origin::Endpoint);
        assert_eq!(assertion.subject.as_deref(), Some("__moso_op_create"));
    }

    #[test]
    fn a_compiler_derive_is_not_charged_to_a_moso_macro() {
        let items = scan(EXPANSION);
        let debug = items
            .iter()
            .find(|item| item.header.contains("::core::fmt::Debug"))
            .expect("the Debug impl");
        assert_eq!(debug.origin, Origin::StdDerive);
        assert!(!debug.origin.is_moso());
    }

    #[test]
    fn field_counts_come_from_the_expansion() {
        let items = scan(EXPANSION);
        let types = declared_types(EXPANSION, &items);
        let post = types
            .iter()
            .find(|shape| shape.name == "CreatePost")
            .expect("the struct");
        assert_eq!(post.fields, 2);
        assert_eq!(post.module, "models");
    }

    #[test]
    fn the_schema_budget_is_measured_per_field() {
        let report = measure("demo", "rustc-test", EXPANSION);
        let schema = report
            .macros
            .iter()
            .find(|cost| cost.origin == Origin::Schema)
            .expect("a Schema cost");
        assert_eq!(schema.units, 2, "two fields");
        assert_eq!(schema.lines, 7, "3 lines of Validate plus 4 of Schema");
        assert!((schema.per_unit - 3.5).abs() < 1e-9);
        assert!(schema.within_budget);
    }

    #[test]
    fn a_derive_over_budget_is_named_with_its_numbers() {
        // Twenty-six lines of `Schema` for one field is over the 25-line budget.
        let mut expanded = String::from("pub struct One {\n    pub a: u8,\n}\n");
        expanded.push_str("impl ::moso::__private::Schema for One {\n");
        for index in 0..25 {
            expanded.push_str(&format!("    fn f{index}() {{}}\n"));
        }
        expanded.push_str("}\n");
        let report = measure("demo", "rustc-test", &expanded);
        let schema = report
            .macros
            .iter()
            .find(|cost| cost.origin == Origin::Schema)
            .expect("a Schema cost");
        assert!(!schema.within_budget, "{schema:?}");
        assert_eq!(schema.offenders.len(), 1);
        assert!(
            schema.offenders[0].contains("One"),
            "{:?}",
            schema.offenders
        );
        assert!(!report.within_budget());
    }

    #[test]
    fn braces_inside_a_diagnostic_message_do_not_confuse_the_scanner() {
        let expanded = "\
pub mod a {
    #[diagnostic::on_unimplemented(message = \"`{Self}` is not a handler {\")]
    pub trait Handler {
        fn call(&self);
    }
    pub struct After;
}
";
        let items = scan(expanded);
        let headers: Vec<&str> = items.iter().map(|item| item.header.as_str()).collect();
        assert!(headers.contains(&"pub trait Handler {"), "{headers:?}");
        assert!(headers.contains(&"pub struct After;"), "{headers:?}");
    }

    #[test]
    fn endpoint_lines_are_charged_per_endpoint() {
        let report = measure("demo", "rustc-test", EXPANSION);
        let endpoint = report
            .macros
            .iter()
            .find(|cost| cost.origin == Origin::Endpoint)
            .expect("an endpoint cost");
        assert_eq!(endpoint.units, 1);
        assert!(endpoint.within_budget, "{endpoint:?}");
        assert_eq!(endpoint.budget, Some(60.0));
    }

    #[test]
    fn the_middleware_pair_is_charged_to_the_middleware() {
        let expanded = "\
pub mod middleware {
    pub struct ObserveLayer;
    impl<I> ::moso::__private::tower::Layer<I> for ObserveLayer {
        fn layer(&self) {}
    }
    pub struct ObserveService<I> {
        inner: I,
    }
}
";
        let items = scan(expanded);
        let charged: Vec<&str> = items
            .iter()
            .filter(|item| item.origin == Origin::Middleware)
            .map(|item| item.header.as_str())
            .collect();
        assert_eq!(charged.len(), 3, "{charged:?}");
    }

    #[test]
    fn an_unterminated_item_at_the_end_of_the_file_does_not_loop() {
        let items = scan("pub struct Truncated {\n    pub a: u8,\n");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].end, 2);
    }
}
