//! `check-sealed` — the gate that makes [ADR-0005] true.
//!
//! ADR-0005 says the query engine under `moso-sql` can be replaced in a patch
//! release. That is only true if no `sea-query` type — no foreign type at all,
//! beyond a short reviewed list — is reachable from the public API of
//! `moso-sql` or `moso-orm`. A promise like that decays the moment one `pub fn`
//! returns a foreign builder, and it decays invisibly, because the crate still
//! compiles and the tests still pass. So it is checked, per commit, from
//! rustdoc's own view of the public API.
//!
//! # What counts as a leak
//!
//! A foreign path in any of these positions:
//!
//! | Position | Why it is a leak |
//! | --- | --- |
//! | a parameter or return type | callers must name the type to call the function |
//! | a public field type | callers can read the value out |
//! | a type alias target | the alias *is* the foreign type |
//! | a supertrait or a bound | implementors must depend on the foreign crate |
//! | an associated type's value | the projection has a foreign type |
//! | a re-export | `pub use foreign::Thing` is the foreign type under our name |
//! | a generic argument of an implemented trait | `impl From<foreign::T>` is callable |
//!
//! # What does not count
//!
//! Implementing a foreign trait for one of our types (`impl Serialize for Sql`)
//! is not a leak, and neither are the method signatures inside such an impl:
//! they are dictated by the trait, not chosen by us. Nor are blanket and
//! synthetic impls that rustdoc attaches from elsewhere. Getting these
//! exclusions right is the difference between a gate people keep and a gate
//! people delete.
//!
//! [ADR-0005]: ../../docs/adr/0005-sealed-sql-facade.md

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bail;
use crate::meta::Workspace;
use crate::rustdoc::{Doc, ImplOwner, path_refs, span_file, span_line};
use crate::util::{Error, Result, ui};

/// The crates whose public API is sealed, in the order they are checked.
///
/// ```
/// assert_eq!(xtask::sealed::SEALED_CRATES, ["moso-sql", "moso-orm"]);
/// ```
pub const SEALED_CRATES: [&str; 2] = ["moso-sql", "moso-orm"];

/// Crate names that may appear in any public signature, in any crate.
///
/// `moso_*` is handled separately, by prefix, so that a crate added later is
/// covered without editing this list.
///
/// ```
/// assert!(xtask::sealed::ALWAYS_ALLOWED.contains(&"core"));
/// ```
pub const ALWAYS_ALLOWED: [&str; 4] = ["std", "core", "alloc", "proc_macro"];

/// One allowlist entry. The reason is mandatory: an allowlist without reasons
/// becomes a list nobody can shorten.
///
/// ```
/// use xtask::sealed::AllowEntry;
///
/// let toml = r#"name = "serde"
/// reason = "`Serialize` bounds are part of the model contract""#;
/// let entry: AllowEntry = toml::from_str(toml)?;
/// assert_eq!(entry.name, "serde");
/// # Ok::<(), toml::de::Error>(())
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AllowEntry {
    /// The crate name (underscored, as rustdoc spells it) or the exact path.
    pub name: String,
    /// Why this exception exists. Checked to be non-empty.
    pub reason: String,
}

/// The per-crate half of the allowlist.
///
/// ```
/// use xtask::sealed::CrateAllow;
///
/// let toml = r#"
/// crates = [{ name = "sqlx", reason = "execution is not construction" }]
/// "#;
/// let allow: CrateAllow = toml::from_str(toml)?;
/// assert_eq!(allow.crates.len(), 1);
/// assert!(allow.paths.is_empty());
/// # Ok::<(), toml::de::Error>(())
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CrateAllow {
    /// Whole crates whose paths may appear.
    #[serde(default)]
    pub crates: Vec<AllowEntry>,
    /// Individual `::`-joined paths that may appear, when the crate as a whole
    /// must not.
    #[serde(default)]
    pub paths: Vec<AllowEntry>,
}

/// The parsed `xtask/allow/sealed.toml`.
///
/// ```
/// use xtask::sealed::AllowList;
///
/// let toml = r#"
/// [every_sealed_crate]
/// crates = [{ name = "serde", reason = "derive bounds" }]
///
/// [crate."moso-orm"]
/// crates = [{ name = "uuid", reason = "a column value type" }]
/// "#;
/// let allow = AllowList::parse(toml)?;
/// assert!(allow.allows("moso-orm", "uuid", "uuid::Uuid"));
/// assert!(allow.allows("moso-sql", "serde", "serde::Serialize"));
/// assert!(!allow.allows("moso-sql", "uuid", "uuid::Uuid"));
/// assert!(!allow.allows("moso-sql", "sea_query", "sea_query::Expr"));
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AllowList {
    /// Exceptions that apply to every sealed crate.
    #[serde(default)]
    pub every_sealed_crate: CrateAllow,
    /// Exceptions that apply to one crate, keyed by package name.
    #[serde(default, rename = "crate")]
    pub per_crate: BTreeMap<String, CrateAllow>,
}

impl AllowList {
    /// Parses the file and rejects an entry without a reason.
    ///
    /// ```
    /// use xtask::sealed::AllowList;
    ///
    /// let error = AllowList::parse(
    ///     "[every_sealed_crate]\ncrates = [{ name = \"sqlx\", reason = \"  \" }]",
    /// ).expect_err("blank reason");
    /// assert!(error.to_string().contains("reason"), "{error}");
    /// ```
    pub fn parse(toml_text: &str) -> Result<Self> {
        let list: Self = toml::from_str(toml_text)
            .map_err(|error| Error::from(error).with_context("xtask/allow/sealed.toml"))?;
        for (scope, allow) in std::iter::once(("every sealed crate", &list.every_sealed_crate))
            .chain(list.per_crate.iter().map(|(k, v)| (k.as_str(), v)))
        {
            for entry in allow.crates.iter().chain(allow.paths.iter()) {
                if entry.reason.trim().is_empty() {
                    bail!(
                        "the sealed allowlist entry `{}` under `{scope}` has an empty reason; \
                         say why the exception exists or remove it",
                        entry.name
                    );
                }
            }
        }
        Ok(list)
    }

    /// Reads the allowlist from disk, tolerating its absence.
    ///
    /// ```no_run
    /// use xtask::sealed::AllowList;
    ///
    /// let root = xtask::util::workspace_root()?;
    /// let allow = AllowList::load(&root.join("xtask/allow/sealed.toml"))?;
    /// assert!(allow.allows("moso-sql", "std", "std::string::String") || true);
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(Error::from(error).with_context(path.display().to_string())),
        }
    }

    /// Whether `crate_name`/`path` is permitted in `sealed_crate`'s public API.
    ///
    /// ```
    /// use xtask::sealed::AllowList;
    ///
    /// let allow = AllowList::default();
    /// assert!(allow.allows("moso-sql", "core", "core::option::Option"));
    /// assert!(allow.allows("moso-sql", "moso_schema", "moso_schema::Id"));
    /// assert!(!allow.allows("moso-sql", "sea_query", "sea_query::Value"));
    /// ```
    #[must_use]
    pub fn allows(&self, sealed_crate: &str, crate_name: &str, path: &str) -> bool {
        if ALWAYS_ALLOWED.contains(&crate_name)
            || crate_name == "moso"
            || crate_name.starts_with("moso_")
            || crate_name.starts_with("moso-")
        {
            return true;
        }
        let scopes = [
            Some(&self.every_sealed_crate),
            self.per_crate.get(sealed_crate),
        ];
        for allow in scopes.into_iter().flatten() {
            if allow.crates.iter().any(|entry| entry.name == crate_name) {
                return true;
            }
            if allow.paths.iter().any(|entry| entry.name == path) {
                return true;
            }
        }
        false
    }
}

/// One foreign path in one public position.
///
/// ```
/// use xtask::sealed::Leak;
///
/// let leak = Leak {
///     item: "moso_sql::Select::filter".into(),
///     kind: "function".into(),
///     position: "parameter `expr`".into(),
///     foreign_crate: "sea_query".into(),
///     foreign_path: "sea_query::SimpleExpr".into(),
///     printed: "SimpleExpr".into(),
///     file: Some("crates/moso-sql/src/select.rs".into()),
///     line: Some(88),
/// };
/// assert!(leak.to_string().contains("sea_query::SimpleExpr"));
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Leak {
    /// The public path of the item whose signature leaks.
    pub item: String,
    /// The item's rustdoc kind.
    pub kind: String,
    /// Where in the signature the foreign path appears.
    pub position: String,
    /// The crate the foreign path belongs to.
    pub foreign_crate: String,
    /// The foreign path, as its own crate spells it.
    pub foreign_path: String,
    /// The foreign path as printed at the use site.
    pub printed: String,
    /// The file the item is declared in.
    pub file: Option<String>,
    /// The line the item starts on.
    pub line: Option<u64>,
}

impl std::fmt::Display for Leak {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}) leaks {} in its {}",
            self.item, self.kind, self.foreign_path, self.position
        )?;
        if let Some(file) = &self.file {
            write!(f, " — {file}")?;
            if let Some(line) = self.line {
                write!(f, ":{line}")?;
            }
        }
        Ok(())
    }
}

/// What one crate's check found.
///
/// ```
/// use xtask::sealed::CrateReport;
///
/// let report = CrateReport {
///     crate_name: "moso-sql".into(), format_version: 57,
///     items_checked: 120, refs_checked: 400, leaks: Vec::new(), unresolved: Vec::new(),
/// };
/// assert!(report.sealed());
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CrateReport {
    /// The package that was checked.
    pub crate_name: String,
    /// The rustdoc JSON format version the answer came from.
    pub format_version: u64,
    /// How many public items were inspected.
    pub items_checked: usize,
    /// How many named type references were resolved.
    pub refs_checked: usize,
    /// Every leak found, sorted by item path.
    pub leaks: Vec<Leak>,
    /// References rustdoc did not give a path for. Reported rather than
    /// silently dropped, because a gate that cannot see is not a gate.
    pub unresolved: Vec<String>,
}

impl CrateReport {
    /// Whether the crate's public API is sealed.
    ///
    /// ```
    /// # use xtask::sealed::{CrateReport, Leak};
    /// let mut report = CrateReport { crate_name: "moso-sql".into(), format_version: 57,
    ///     items_checked: 1, refs_checked: 1, leaks: Vec::new(), unresolved: Vec::new() };
    /// assert!(report.sealed());
    /// report.leaks.push(Leak { item: "i".into(), kind: "k".into(), position: "p".into(),
    ///     foreign_crate: "sea_query".into(), foreign_path: "sea_query::X".into(),
    ///     printed: "X".into(), file: None, line: None });
    /// assert!(!report.sealed());
    /// ```
    #[must_use]
    pub fn sealed(&self) -> bool {
        self.leaks.is_empty()
    }
}

/// The whole run: one report per crate that exists, plus the ones that do not.
///
/// ```
/// use xtask::sealed::Report;
///
/// let report = Report { crates: Vec::new(), skipped: vec!["moso-orm".into()] };
/// assert!(report.sealed());
/// assert_eq!(report.total_leaks(), 0);
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Report {
    /// One entry per crate that was checked.
    pub crates: Vec<CrateReport>,
    /// Crates named for checking that are not in the workspace yet.
    pub skipped: Vec<String>,
}

impl Report {
    /// Whether every checked crate is sealed.
    ///
    /// ```
    /// # use xtask::sealed::Report;
    /// assert!(Report { crates: Vec::new(), skipped: Vec::new() }.sealed());
    /// ```
    #[must_use]
    pub fn sealed(&self) -> bool {
        self.crates.iter().all(CrateReport::sealed)
    }

    /// How many leaks were found across every crate.
    ///
    /// ```
    /// # use xtask::sealed::Report;
    /// assert_eq!(Report { crates: Vec::new(), skipped: Vec::new() }.total_leaks(), 0);
    /// ```
    #[must_use]
    pub fn total_leaks(&self) -> usize {
        self.crates.iter().map(|c| c.leaks.len()).sum()
    }
}

/// Options for one run of the gate.
///
/// ```
/// use xtask::sealed::Options;
///
/// let options = Options::default();
/// assert_eq!(options.crates, vec!["moso-sql".to_owned(), "moso-orm".to_owned()]);
/// assert!(!options.self_test);
/// ```
#[derive(Clone, Debug)]
pub struct Options {
    /// The packages to check.
    pub crates: Vec<String>,
    /// Where the allowlist lives, relative to the workspace root.
    pub allow_file: PathBuf,
    /// Also run the fixture self-test.
    pub self_test: bool,
    /// Write the machine-readable report here.
    pub json: Option<PathBuf>,
    /// Build artefacts directory; `None` means the workspace's own `target/`.
    pub target_dir: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            crates: SEALED_CRATES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            allow_file: PathBuf::from("xtask/allow/sealed.toml"),
            self_test: false,
            json: None,
            target_dir: None,
        }
    }
}

/// Runs the gate and prints its findings.
///
/// Returns `Ok(false)` when a leak was found, so the caller decides the exit
/// code — `xtask ci` wants to keep going and print a summary.
///
/// ```no_run
/// let options = xtask::sealed::Options::default();
/// let sealed = xtask::sealed::run(&options)?;
/// assert!(sealed);
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn run(options: &Options) -> Result<bool> {
    let root = crate::util::workspace_root()?;
    let workspace = Workspace::load()?;
    let allow = AllowList::load(&root.join(&options.allow_file))?;

    let mut report = Report {
        crates: Vec::new(),
        skipped: Vec::new(),
    };

    ui::headline("check-sealed");
    for name in &options.crates {
        if !workspace.has(name) {
            report.skipped.push(name.clone());
            ui::warn(&format!(
                "{name} is not a workspace member yet — nothing to seal (ADR-0005 applies from the commit that adds it)"
            ));
            continue;
        }
        let doc = Doc::produce(&root, name, options.target_dir.as_deref())?;
        let crate_report = check_doc(&doc, name, &allow);
        if crate_report.sealed() {
            ui::ok(&format!(
                "{name}: {} public items, {} type references, 0 foreign paths (rustdoc format {})",
                crate_report.items_checked, crate_report.refs_checked, crate_report.format_version
            ));
        } else {
            ui::fail(&format!(
                "{name}: {} foreign path(s) in the public API",
                crate_report.leaks.len()
            ));
            for leak in &crate_report.leaks {
                ui::note(&leak.to_string());
            }
            ui::note("");
            ui::note(
                "help: wrap it — return a Moso-owned opaque type and keep the foreign type in a private field",
            );
            ui::note(
                "help: or, if the exception is deliberate, add it to xtask/allow/sealed.toml with a reason",
            );
        }
        for unresolved in &crate_report.unresolved {
            ui::warn(&format!("{name}: unresolved reference {unresolved}"));
        }
        report.crates.push(crate_report);
    }

    if options.self_test {
        self_test(&root, options.target_dir.as_deref())?;
    }

    if let Some(path) = &options.json {
        let text = serde_json::to_string_pretty(&report)?;
        std::fs::write(root.join(path), text + "\n")?;
        ui::note(&format!("report written to {}", path.display()));
    }

    Ok(report.sealed())
}

/// Checks one already-produced rustdoc document.
///
/// This is the whole gate, separated from the process management so the
/// fixtures — and a hand-written document in a unit test — exercise exactly the
/// code CI runs.
///
/// ```
/// use xtask::rustdoc::Doc;
/// use xtask::sealed::{AllowList, check_doc};
///
/// let json = r#"{"index":{
///     "2":{"id":"2","crate_id":0,"name":"build","attrs":[],
///          "span":{"filename":"src/lib.rs","begin":[4,1]},
///          "inner":{"function":{"sig":{"inputs":[],
///            "output":{"resolved_path":{"path":"SelectStatement","id":"90","args":null}}},
///            "generics":{"params":[],"where_predicates":[]}}}}},
///   "paths":{"2":{"crate_id":0,"path":["moso_sql","build"],"kind":"function"},
///            "90":{"crate_id":7,"path":["sea_query","SelectStatement"],"kind":"struct"}},
///   "external_crates":{"7":{"name":"sea_query"}},"format_version":57}"#;
/// let doc = Doc::from_json("moso-sql", json)?;
/// let report = check_doc(&doc, "moso-sql", &AllowList::default());
/// assert_eq!(report.leaks.len(), 1);
/// assert_eq!(report.leaks[0].position, "return type");
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[must_use]
pub fn check_doc(doc: &Doc, sealed_crate: &str, allow: &AllowList) -> CrateReport {
    let owners = doc.impl_owners();
    let mut leaks: Vec<Leak> = Vec::new();
    let mut unresolved: BTreeSet<String> = BTreeSet::new();
    let mut items_checked = 0_usize;
    let mut refs_checked = 0_usize;

    for (id, item) in doc.index() {
        let Some(inner) = item.get("inner").and_then(Value::as_object) else {
            continue;
        };
        let Some(kind) = inner.keys().next().map(String::as_str) else {
            continue;
        };
        if matches!(kind, "module" | "primitive" | "extern_crate" | "macro") {
            continue;
        }
        let owner = owners.get(id);
        let positions = positions_for(kind, inner, owner);
        if positions.is_empty() {
            continue;
        }
        items_checked += 1;
        let label = item_label(doc, id, item, owner);
        for (position, value) in positions {
            for reference in path_refs(value) {
                refs_checked += 1;
                match doc.owner_of(&reference.id) {
                    Some(owner) => {
                        if owner.local || allow.allows(sealed_crate, &owner.crate_name, &owner.path)
                        {
                            continue;
                        }
                        leaks.push(Leak {
                            item: label.clone(),
                            kind: kind.to_owned(),
                            position: position.clone(),
                            foreign_crate: owner.crate_name,
                            foreign_path: owner.path,
                            printed: reference.printed,
                            file: span_file(item),
                            line: span_line(item),
                        });
                    }
                    None => {
                        if doc.item(&reference.id).is_none() {
                            unresolved.insert(format!("{} in {label}", reference.printed));
                        }
                    }
                }
            }
        }
    }

    leaks.sort_by(|a, b| {
        (&a.item, &a.position, &a.foreign_path).cmp(&(&b.item, &b.position, &b.foreign_path))
    });
    leaks.dedup_by(|a, b| {
        (&a.item, &a.position, &a.foreign_path) == (&b.item, &b.position, &b.foreign_path)
    });

    CrateReport {
        crate_name: sealed_crate.to_owned(),
        format_version: doc.format_version(),
        items_checked,
        refs_checked,
        leaks,
        unresolved: unresolved.into_iter().collect(),
    }
}

/// The (position, subtree) pairs worth checking for one item kind.
fn positions_for<'a>(
    kind: &str,
    inner: &'a serde_json::Map<String, Value>,
    owner: Option<&ImplOwner>,
) -> Vec<(String, &'a Value)> {
    let in_trait_impl = owner.is_some_and(ImplOwner::is_trait_impl);
    let body = &inner[kind];
    let mut positions: Vec<(String, &Value)> = Vec::new();

    match kind {
        "function" => {
            // A method inside `impl ForeignTrait for Ours` has the signature
            // the trait demands. The trait itself is checked on the impl item.
            if in_trait_impl {
                return positions;
            }
            if let Some(inputs) = body.pointer("/sig/inputs").and_then(Value::as_array) {
                for input in inputs {
                    let name = input
                        .get(0)
                        .and_then(Value::as_str)
                        .unwrap_or("_")
                        .to_owned();
                    if let Some(ty) = input.get(1) {
                        positions.push((format!("parameter `{name}`"), ty));
                    }
                }
            }
            if let Some(output) = body.pointer("/sig/output").filter(|v| !v.is_null()) {
                positions.push(("return type".to_owned(), output));
            }
            if let Some(generics) = body.get("generics") {
                positions.push(("generic bounds".to_owned(), generics));
            }
        }
        "struct_field" => positions.push(("field type".to_owned(), body)),
        "variant" => {
            if let Some(kind) = body.get("kind") {
                positions.push(("variant field".to_owned(), kind));
            }
        }
        "type_alias" => {
            if let Some(ty) = body.get("type") {
                positions.push(("alias target".to_owned(), ty));
            }
            if let Some(generics) = body.get("generics") {
                positions.push(("generic bounds".to_owned(), generics));
            }
        }
        "constant" | "static" => {
            if let Some(ty) = body.get("type") {
                positions.push((format!("{kind} type"), ty));
            }
        }
        "trait" => {
            if let Some(bounds) = body.get("bounds") {
                positions.push(("supertrait bounds".to_owned(), bounds));
            }
            if let Some(generics) = body.get("generics") {
                positions.push(("generic bounds".to_owned(), generics));
            }
        }
        "struct" | "enum" | "union" => {
            if let Some(generics) = body.get("generics") {
                positions.push(("generic bounds".to_owned(), generics));
            }
        }
        "assoc_type" => {
            // The *value* of an associated type is ours to choose even inside a
            // foreign trait's impl, so this one is checked either way.
            if let Some(bounds) = body.get("bounds") {
                positions.push(("associated type bounds".to_owned(), bounds));
            }
            if let Some(ty) = body.get("type").filter(|v| !v.is_null()) {
                positions.push(("associated type value".to_owned(), ty));
            }
        }
        "assoc_const" => {
            if !in_trait_impl && let Some(ty) = body.get("type") {
                positions.push(("associated constant type".to_owned(), ty));
            }
        }
        "use" => {
            // `Use { source, name, id, is_glob }`: `source` is the printed path
            // and `id` the target, which is the pair `path_refs` recognises, so
            // the whole body can be handed over as-is.
            if body.get("id").is_some_and(|id| !id.is_null()) {
                positions.push(("re-export".to_owned(), body));
            }
        }
        "impl" => {
            // Blanket and synthetic impls are rustdoc's, not ours.
            if body.get("is_synthetic").and_then(Value::as_bool) == Some(true)
                || body.get("blanket_impl").is_some_and(|v| !v.is_null())
            {
                return positions;
            }
            if let Some(args) = body.pointer("/trait/args").filter(|v| !v.is_null()) {
                positions.push(("implemented trait's arguments".to_owned(), args));
            }
            if let Some(predicates) = body.pointer("/generics/where_predicates") {
                positions.push(("impl bounds".to_owned(), predicates));
            }
        }
        _ => {}
    }
    positions
}

fn item_label(doc: &Doc, id: &str, item: &Value, owner: Option<&ImplOwner>) -> String {
    if let Some(owner) = doc.owner_of(id)
        && !owner.path.is_empty()
    {
        return owner.path;
    }
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<unnamed>");
    match owner {
        Some(owner) => {
            let container = doc
                .item(&owner.impl_id)
                .and_then(|imp| imp.pointer("/inner/impl/for"))
                .map(|ty| {
                    path_refs(ty)
                        .first()
                        .map(|r| r.printed.clone())
                        .unwrap_or_else(|| "impl".to_owned())
                })
                .unwrap_or_else(|| "impl".to_owned());
            match &owner.trait_name {
                Some(trait_name) => format!("<{container} as {trait_name}>::{name}"),
                None => format!("{container}::{name}"),
            }
        }
        None => name.to_owned(),
    }
}

/// Runs the gate against the fixtures under `xtask/fixtures/`, which prove it
/// can see a leak and does not invent one.
///
/// `leaky-sql` puts a stand-in for `sea-query` into eight distinct public
/// positions. `sealed-sql` wraps the same engine correctly. If the first
/// produced no findings the gate would be decoration; if the second produced
/// any, nobody could keep it green.
///
/// ```no_run
/// let root = xtask::util::workspace_root()?;
/// xtask::sealed::self_test(&root, None)?;
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn self_test(root: &Path, target_dir: Option<&Path>) -> Result<()> {
    let fixtures = root.join("xtask/fixtures");
    if !fixtures.join("Cargo.toml").is_file() {
        bail!(
            "the fixture workspace {} is missing; check-sealed cannot prove it works",
            fixtures.display()
        );
    }
    let artefacts = target_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("target/xtask/sealed-fixtures"));
    let allow = AllowList::default();

    let leaky = Doc::produce(&fixtures, "leaky-sql", Some(&artefacts))?;
    let leaky_report = check_doc(&leaky, "leaky-sql", &allow);
    let positions: BTreeSet<&str> = leaky_report
        .leaks
        .iter()
        .map(|leak| leak.position.as_str())
        .collect();
    if leaky_report.leaks.len() < 8 || positions.len() < 6 {
        bail!(
            "self-test: the deliberately leaky fixture produced {} leak(s) in {} position(s); \
             expected at least 8 in at least 6. The gate is not seeing what it claims to see.\n{}",
            leaky_report.leaks.len(),
            positions.len(),
            crate::util::indent(
                &leaky_report
                    .leaks
                    .iter()
                    .map(Leak::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        );
    }
    if !leaky_report
        .leaks
        .iter()
        .any(|leak| leak.foreign_crate == "fake_query_engine")
    {
        bail!("self-test: the leaky fixture's findings do not name the stand-in query engine");
    }
    ui::ok(&format!(
        "self-test: the leaky fixture is caught — {} leaks in {} distinct positions ({})",
        leaky_report.leaks.len(),
        positions.len(),
        positions.into_iter().collect::<Vec<_>>().join(", ")
    ));

    let sealed = Doc::produce(&fixtures, "sealed-sql", Some(&artefacts))?;
    let sealed_report = check_doc(&sealed, "sealed-sql", &allow);
    if !sealed_report.sealed() {
        bail!(
            "self-test: the correctly-sealed fixture was flagged, so the gate has false \
             positives and nobody will keep it green:\n{}",
            crate::util::indent(
                &sealed_report
                    .leaks
                    .iter()
                    .map(Leak::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        );
    }
    ui::ok(&format!(
        "self-test: the sealed fixture is clean — {} public items, {} type references, 0 findings",
        sealed_report.items_checked, sealed_report.refs_checked
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(json: &str) -> Doc {
        Doc::from_json("moso-sql", json).expect("valid rustdoc JSON")
    }

    const PATHS: &str = r#""paths":{
        "2":{"crate_id":0,"path":["moso_sql","Select"],"kind":"struct"},
        "90":{"crate_id":7,"path":["sea_query","SelectStatement"],"kind":"struct"},
        "91":{"crate_id":8,"path":["serde","ser","Serialize"],"kind":"trait"},
        "92":{"crate_id":0,"path":["moso_sql","Sql"],"kind":"struct"}
      },
      "external_crates":{"7":{"name":"sea_query"},"8":{"name":"serde"}},
      "format_version":57"#;

    #[test]
    fn a_public_field_of_a_foreign_type_is_a_leak() {
        let json = format!(
            r#"{{"index":{{"3":{{"id":"3","crate_id":0,"name":"inner","attrs":[],
              "inner":{{"struct_field":{{"resolved_path":{{"path":"SelectStatement","id":"90","args":null}}}}}}}}}},
              {PATHS}}}"#
        );
        let report = check_doc(&doc(&json), "moso-sql", &AllowList::default());
        assert_eq!(report.leaks.len(), 1);
        assert_eq!(report.leaks[0].position, "field type");
        assert_eq!(report.leaks[0].foreign_crate, "sea_query");
    }

    #[test]
    fn a_method_of_a_foreign_trait_impl_is_not_a_leak() {
        let json = format!(
            r#"{{"index":{{
              "5":{{"id":"5","crate_id":0,"name":null,"attrs":[],
                "inner":{{"impl":{{"generics":{{"params":[],"where_predicates":[]}},
                  "trait":{{"path":"Serialize","id":"91","args":null}},
                  "for":{{"resolved_path":{{"path":"Sql","id":"92","args":null}}}},
                  "items":["6"],"is_synthetic":false,"blanket_impl":null}}}}}},
              "6":{{"id":"6","crate_id":0,"name":"serialize","attrs":[],
                "inner":{{"function":{{"sig":{{"inputs":[["s",{{"resolved_path":
                    {{"path":"SelectStatement","id":"90","args":null}}}}]],"output":null}},
                  "generics":{{"params":[],"where_predicates":[]}}}}}}}}}},
              {PATHS}}}"#
        );
        let report = check_doc(&doc(&json), "moso-sql", &AllowList::default());
        assert!(
            report.sealed(),
            "the trait dictates the signature: {:?}",
            report.leaks
        );
    }

    #[test]
    fn a_foreign_type_as_a_generic_argument_of_an_implemented_trait_is_a_leak() {
        let json = format!(
            r#"{{"index":{{
              "5":{{"id":"5","crate_id":0,"name":null,"attrs":[],
                "inner":{{"impl":{{"generics":{{"params":[],"where_predicates":[]}},
                  "trait":{{"path":"From","id":"91","args":{{"angle_bracketed":{{
                    "args":[{{"type":{{"resolved_path":{{"path":"SelectStatement","id":"90",
                      "args":null}}}}}}],"constraints":[]}}}}}},
                  "for":{{"resolved_path":{{"path":"Sql","id":"92","args":null}}}},
                  "items":[],"is_synthetic":false,"blanket_impl":null}}}}}}}},
              {PATHS}}}"#
        );
        let report = check_doc(&doc(&json), "moso-sql", &AllowList::default());
        assert_eq!(report.leaks.len(), 1);
        assert_eq!(report.leaks[0].position, "implemented trait's arguments");
    }

    #[test]
    fn a_synthetic_impl_is_rustdocs_not_ours() {
        let json = format!(
            r#"{{"index":{{
              "5":{{"id":"5","crate_id":0,"name":null,"attrs":[],
                "inner":{{"impl":{{"generics":{{"params":[],"where_predicates":[]}},
                  "trait":{{"path":"From","id":"91","args":{{"angle_bracketed":{{
                    "args":[{{"type":{{"resolved_path":{{"path":"SelectStatement","id":"90",
                      "args":null}}}}}}],"constraints":[]}}}}}},
                  "for":{{"generic":"T"}},
                  "items":[],"is_synthetic":true,"blanket_impl":null}}}}}}}},
              {PATHS}}}"#
        );
        let report = check_doc(&doc(&json), "moso-sql", &AllowList::default());
        assert!(report.sealed(), "{:?}", report.leaks);
    }

    #[test]
    fn an_allowlisted_crate_is_not_a_leak_but_only_where_it_is_listed() {
        let allow = AllowList::parse(
            r#"
            [crate."moso-orm"]
            crates = [{ name = "sea_query", reason = "a test, and only a test" }]
            "#,
        )
        .expect("valid allowlist");
        let json = format!(
            r#"{{"index":{{"3":{{"id":"3","crate_id":0,"name":"inner","attrs":[],
              "inner":{{"struct_field":{{"resolved_path":{{"path":"SelectStatement","id":"90","args":null}}}}}}}}}},
              {PATHS}}}"#
        );
        assert!(check_doc(&doc(&json), "moso-orm", &allow).sealed());
        assert!(!check_doc(&doc(&json), "moso-sql", &allow).sealed());
    }

    #[test]
    fn a_leak_is_reported_once_even_when_two_positions_agree() {
        let json = format!(
            r#"{{"index":{{"7":{{"id":"7","crate_id":0,"name":"round_trip","attrs":[],
              "inner":{{"function":{{"sig":{{
                "inputs":[["a",{{"resolved_path":{{"path":"SelectStatement","id":"90","args":null}}}}]],
                "output":{{"resolved_path":{{"path":"SelectStatement","id":"90","args":null}}}}}},
                "generics":{{"params":[],"where_predicates":[]}}}}}}}}}},
              {PATHS}}}"#
        );
        let report = check_doc(&doc(&json), "moso-sql", &AllowList::default());
        assert_eq!(report.leaks.len(), 2, "a parameter and a return type");
        let positions: Vec<&str> = report.leaks.iter().map(|l| l.position.as_str()).collect();
        assert!(positions.contains(&"return type"));
        assert!(positions.contains(&"parameter `a`"));
    }

    #[test]
    fn the_committed_allowlist_parses_and_every_entry_has_a_reason() {
        let root = crate::util::workspace_root().expect("a workspace");
        let path = root.join("xtask/allow/sealed.toml");
        let text = std::fs::read_to_string(&path).expect("the committed allowlist");
        AllowList::parse(&text).expect("every entry has a reason");
    }
}
