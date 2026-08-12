//! `check-deps` — the six dependency rules, and the crate-count budget.
//!
//! `docs/00-foundations/03-crate-layout.md` ends with six rules and the note
//! *"enforced in CI by `xtask check-deps` in the original plan. There is no
//! `xtask` in this build, so these are currently reviewed by hand."* Five of the
//! six are the kind of thing a hand review gets right for a year and then gets
//! wrong once, permanently: nothing about `moso-schema` gaining a transitive
//! dependency on `http` is visible in a diff. The sixth — the crate count — is
//! not reviewable by hand at all.
//!
//! | Rule | What it protects |
//! | --- | --- |
//! | 1 | `moso-macros` depends on no runtime Moso crate, so a macro change does not rebuild the world |
//! | 2 | `moso-core` depends on no battery, so a stateless service compiles no database code |
//! | 3 | `moso-schema` never sees `http`, `axum` or `sqlx`, so the model layer is usable standalone |
//! | 4 | nothing the facade depends on depends back on the facade |
//! | 5 | batteries couple only along declared edges |
//! | 6 | ≤ 90 third-party crates with default features, ≤ 260 with `full` |
//!
//! Rules 1, 2, 4 and 5 are about *declared* edges and are read from
//! `cargo metadata`. Rules 3 and 6 are about the *resolved* graph and are read
//! from `cargo tree`, because "does not depend on `sqlx`" has to mean "not even
//! six levels down".

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::meta::{Package, Workspace, resolved_packages};
use crate::util::{Error, Result, ui};

/// The third-party crate budget with the facade's default features.
///
/// ```
/// assert_eq!(xtask::deps::DEFAULT_BUDGET, 90);
/// ```
pub const DEFAULT_BUDGET: usize = 90;

/// The third-party crate budget with the facade's `full` feature.
///
/// ```
/// assert_eq!(xtask::deps::FULL_BUDGET, 260);
/// ```
pub const FULL_BUDGET: usize = 260;

/// Crates `moso-schema` must never reach, in any table, at any depth.
///
/// ```
/// assert!(xtask::deps::SCHEMA_FORBIDDEN.contains(&"sqlx"));
/// ```
pub const SCHEMA_FORBIDDEN: [&str; 3] = ["http", "axum", "sqlx"];

/// The one exception to rule 4, and why.
///
/// ```
/// assert_eq!(xtask::deps::FACADE_DEPENDENT_EXCEPTION, "moso-test");
/// ```
pub const FACADE_DEPENDENT_EXCEPTION: &str = "moso-test";

/// The declared battery topology, read from `xtask/allow/dep-edges.toml`.
///
/// ```
/// use xtask::deps::Topology;
///
/// let topology = Topology::parse(r#"
/// [batteries]
/// members = ["moso-orm", "moso-sql", "moso-kv"]
///
/// [edges]
/// "moso-orm" = ["moso-sql"]
/// "moso-kv" = []
/// "#)?;
/// assert!(topology.is_battery("moso-orm"));
/// assert!(topology.edge_allowed("moso-orm", "moso-sql"));
/// assert!(!topology.edge_allowed("moso-kv", "moso-orm"));
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Topology {
    /// The `[batteries]` table.
    #[serde(default)]
    pub batteries: Batteries,
    /// The `[edges]` table: package name to the batteries it may depend on.
    #[serde(default)]
    pub edges: BTreeMap<String, Vec<String>>,
}

/// The `[batteries]` table.
///
/// ```
/// use xtask::deps::Batteries;
///
/// let batteries: Batteries = toml::from_str("members = [\"moso-kv\"]")?;
/// assert_eq!(batteries.members, ["moso-kv"]);
/// # Ok::<(), toml::de::Error>(())
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Batteries {
    /// Every crate rule 5 governs.
    #[serde(default)]
    pub members: Vec<String>,
}

impl Topology {
    /// Parses the declaration.
    ///
    /// ```
    /// use xtask::deps::Topology;
    ///
    /// assert!(Topology::parse("").is_ok());
    /// assert!(Topology::parse("[edges]\nx = 3").is_err());
    /// ```
    pub fn parse(toml_text: &str) -> Result<Self> {
        toml::from_str(toml_text)
            .map_err(|error| Error::from(error).with_context("xtask/allow/dep-edges.toml"))
    }

    /// Reads the declaration from disk, tolerating its absence.
    ///
    /// ```no_run
    /// use xtask::deps::Topology;
    ///
    /// let root = xtask::util::workspace_root()?;
    /// let topology = Topology::load(&root.join("xtask/allow/dep-edges.toml"))?;
    /// assert!(topology.is_battery("moso-orm"));
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(Error::from(error).with_context(path.display().to_string())),
        }
    }

    /// Whether rule 5 governs this crate.
    ///
    /// ```
    /// # use xtask::deps::Topology;
    /// assert!(!Topology::default().is_battery("moso-core"));
    /// ```
    #[must_use]
    pub fn is_battery(&self, name: &str) -> bool {
        self.batteries.members.iter().any(|member| member == name)
    }

    /// Whether `from` is declared to be allowed to depend on `to`.
    ///
    /// ```
    /// # use xtask::deps::Topology;
    /// assert!(!Topology::default().edge_allowed("moso-kv", "moso-orm"));
    /// ```
    #[must_use]
    pub fn edge_allowed(&self, from: &str, to: &str) -> bool {
        self.edges
            .get(from)
            .is_some_and(|allowed| allowed.iter().any(|name| name == to))
    }
}

/// Whether a rule passed, failed, or had nothing to check.
///
/// ```
/// use xtask::deps::Status;
///
/// assert!(Status::Pass.ok());
/// assert!(Status::Skipped.ok());
/// assert!(!Status::Fail.ok());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The rule holds.
    Pass,
    /// The rule is broken.
    Fail,
    /// There was nothing to check — usually a crate that does not exist yet.
    Skipped,
}

impl Status {
    /// Whether this status lets the gate stay green.
    ///
    /// ```
    /// assert!(xtask::deps::Status::Pass.ok());
    /// ```
    #[must_use]
    pub fn ok(self) -> bool {
        !matches!(self, Self::Fail)
    }
}

/// One rule's outcome.
///
/// ```
/// use xtask::deps::{RuleOutcome, Status};
///
/// let outcome = RuleOutcome { id: 3, title: "moso-schema is standalone".into(),
///     status: Status::Pass, findings: Vec::new(), detail: Some("311 crates".into()) };
/// assert!(outcome.status.ok());
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuleOutcome {
    /// The rule's number in `docs/00-foundations/03-crate-layout.md`.
    pub id: u8,
    /// A one-line statement of the rule.
    pub title: String,
    /// What happened.
    pub status: Status,
    /// One line per violation, each naming the fix.
    pub findings: Vec<String>,
    /// A measurement worth recording even when the rule passes.
    pub detail: Option<String>,
}

/// The whole run.
///
/// ```
/// use xtask::deps::Report;
///
/// let report = Report { rules: Vec::new() };
/// assert!(report.ok());
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Report {
    /// One entry per rule, in rule order.
    pub rules: Vec<RuleOutcome>,
}

impl Report {
    /// Whether every rule passed or was skipped.
    ///
    /// ```
    /// # use xtask::deps::{Report, RuleOutcome, Status};
    /// let report = Report { rules: vec![RuleOutcome { id: 1, title: "t".into(),
    ///     status: Status::Fail, findings: vec!["x".into()], detail: None }] };
    /// assert!(!report.ok());
    /// ```
    #[must_use]
    pub fn ok(&self) -> bool {
        self.rules.iter().all(|rule| rule.status.ok())
    }
}

/// Options for one run of the gate.
///
/// ```
/// let options = xtask::deps::Options::default();
/// assert_eq!(options.default_budget, 90);
/// ```
#[derive(Clone, Debug)]
pub struct Options {
    /// Where the battery topology is declared.
    pub edges_file: PathBuf,
    /// The default-features crate budget.
    pub default_budget: usize,
    /// The `full`-features crate budget.
    pub full_budget: usize,
    /// Write the machine-readable report here.
    pub json: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            edges_file: PathBuf::from("xtask/allow/dep-edges.toml"),
            default_budget: DEFAULT_BUDGET,
            full_budget: FULL_BUDGET,
            json: None,
        }
    }
}

/// Runs every rule and prints the outcome.
///
/// ```no_run
/// let ok = xtask::deps::run(&xtask::deps::Options::default())?;
/// assert!(ok);
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn run(options: &Options) -> Result<bool> {
    let root = crate::util::workspace_root()?;
    let workspace = Workspace::load()?;
    let topology = Topology::load(&root.join(&options.edges_file))?;

    let mut report = Report { rules: Vec::new() };
    ui::headline("check-deps");

    report.rules.push(rule_1(&workspace, &topology));
    report.rules.push(rule_2(&workspace, &topology));
    report.rules.push(rule_3(&root, &workspace)?);
    report.rules.push(rule_4(&workspace));
    report.rules.push(rule_5(&workspace, &topology));
    report.rules.push(rule_6(&root, &workspace, options)?);

    for rule in &report.rules {
        let line = format!("rule {}: {}", rule.id, rule.title);
        match rule.status {
            Status::Pass => ui::ok(&line),
            Status::Fail => ui::fail(&line),
            Status::Skipped => ui::warn(&line),
        }
        if let Some(detail) = &rule.detail {
            ui::note(detail);
        }
        for finding in &rule.findings {
            ui::note(finding);
        }
    }

    if let Some(path) = &options.json {
        let text = serde_json::to_string_pretty(&report)?;
        std::fs::write(root.join(path), text + "\n")?;
        ui::note(&format!("report written to {}", path.display()));
    }

    Ok(report.ok())
}

/// Rule 1 — a proc-macro crate depends on no runtime Moso crate.
///
/// ```
/// use xtask::deps::{Topology, rule_1};
/// use xtask::meta::Workspace;
///
/// let json = r#"{"packages":[{"name":"moso-macros","version":"0.1.0",
///   "manifest_path":"/w/m/Cargo.toml","publish":null,
///   "targets":[{"name":"moso-macros","kind":["proc-macro"]}],
///   "dependencies":[{"name":"moso-core","kind":null,"optional":false}]}],
///   "workspace_root":"/w"}"#;
/// let workspace = Workspace::from_metadata_json(json, "/w".into())?;
/// let outcome = rule_1(&workspace, &Topology::default());
/// assert_eq!(outcome.findings.len(), 1);
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[must_use]
pub fn rule_1(workspace: &Workspace, _topology: &Topology) -> RuleOutcome {
    let mut findings = Vec::new();
    let mut checked = 0;
    for package in workspace.packages.iter().filter(|p| p.is_proc_macro) {
        checked += 1;
        for dep in package.deps.iter().filter(|dep| dep.is_build_relevant()) {
            let is_moso_runtime = (dep.name == "moso" || dep.name.starts_with("moso-"))
                && !workspace
                    .package(&dep.name)
                    .is_some_and(|dep_package| dep_package.is_proc_macro);
            if is_moso_runtime {
                findings.push(format!(
                    "{} depends on {} — generated code must name ::moso::__private::* instead, \
                     so the macro crate stays off the critical path of every downstream build",
                    package.name, dep.name
                ));
            }
        }
    }
    RuleOutcome {
        id: 1,
        title: "no proc-macro crate depends on a runtime Moso crate".to_owned(),
        status: if findings.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        findings,
        detail: Some(format!("{checked} proc-macro crate(s) checked")),
    }
}

/// Rule 2 — `moso-core` depends on no battery.
///
/// ```
/// use xtask::deps::{Topology, rule_2};
/// use xtask::meta::Workspace;
///
/// let json = r#"{"packages":[{"name":"moso-core","version":"0.1.0",
///   "manifest_path":"/w/c/Cargo.toml","publish":null,
///   "targets":[{"name":"moso-core","kind":["lib"]}],
///   "dependencies":[{"name":"moso-orm","kind":null,"optional":false}]}],
///   "workspace_root":"/w"}"#;
/// let topology = Topology::parse("[batteries]\nmembers = [\"moso-orm\"]")?;
/// let workspace = Workspace::from_metadata_json(json, "/w".into())?;
/// assert_eq!(rule_2(&workspace, &topology).findings.len(), 1);
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[must_use]
pub fn rule_2(workspace: &Workspace, topology: &Topology) -> RuleOutcome {
    let Some(core) = workspace.package("moso-core") else {
        return RuleOutcome {
            id: 2,
            title: "moso-core depends on no battery crate".to_owned(),
            status: Status::Skipped,
            findings: Vec::new(),
            detail: Some("moso-core is not in the workspace".to_owned()),
        };
    };
    let findings: Vec<String> = core
        .deps
        .iter()
        .filter(|dep| dep.is_build_relevant() && topology.is_battery(&dep.name))
        .map(|dep| {
            format!(
                "moso-core depends on {} — a stateless service would then compile it; \
                 move the coupling into the battery or into the facade's feature plumbing",
                dep.name
            )
        })
        .collect();
    RuleOutcome {
        id: 2,
        title: "moso-core depends on no battery crate".to_owned(),
        status: if findings.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        findings,
        detail: Some(format!(
            "{} battery crate(s) declared, {} present in the workspace",
            topology.batteries.members.len(),
            topology
                .batteries
                .members
                .iter()
                .filter(|name| workspace.has(name))
                .count()
        )),
    }
}

/// Rule 3 — `moso-schema` never reaches `http`, `axum` or `sqlx`, transitively.
fn rule_3(root: &Path, workspace: &Workspace) -> Result<RuleOutcome> {
    if !workspace.has("moso-schema") {
        return Ok(RuleOutcome {
            id: 3,
            title: "moso-schema depends on no HTTP or database crate".to_owned(),
            status: Status::Skipped,
            findings: Vec::new(),
            detail: Some("moso-schema is not in the workspace".to_owned()),
        });
    }
    let graph = resolved_packages(root, "moso-schema", &[])?;
    let findings: Vec<String> = SCHEMA_FORBIDDEN
        .iter()
        .filter_map(|forbidden| {
            graph
                .iter()
                .find(|package| package.name == *forbidden)
                .map(|package| {
                    format!(
                        "moso-schema resolves {package} — the claim that the model layer is usable \
                         standalone, with no HTTP and no database, is what makes D2 correct"
                    )
                })
        })
        .collect();
    Ok(RuleOutcome {
        id: 3,
        title: "moso-schema depends on no HTTP or database crate".to_owned(),
        status: if findings.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        findings,
        detail: Some(format!(
            "{} crates resolved, none of {}",
            graph.len(),
            SCHEMA_FORBIDDEN.join("/")
        )),
    })
}

/// Rule 4 — nothing the facade depends on depends back on the facade.
///
/// The rule as written in `docs/00-foundations/03` is "no Moso crate may depend
/// on `moso`", with `moso-test` called out as a deliberate exception; the
/// amendment recorded in the same section is the one checked here, because it is
/// the version with a reason behind it. `moso-test` drives a user's application,
/// which depends on the facade, and routing it through `moso-core` instead would
/// let the harness and the application disagree about feature resolution.
///
/// ```
/// use xtask::deps::rule_4;
/// use xtask::meta::Workspace;
///
/// let json = r#"{"packages":[
///   {"name":"moso","version":"0.1.0","manifest_path":"/w/f/Cargo.toml","publish":null,
///    "targets":[{"name":"moso","kind":["lib"]}],
///    "dependencies":[{"name":"moso-core","kind":null,"optional":false}]},
///   {"name":"moso-core","version":"0.1.0","manifest_path":"/w/c/Cargo.toml","publish":null,
///    "targets":[{"name":"moso-core","kind":["lib"]}],
///    "dependencies":[{"name":"moso","kind":null,"optional":false}]}],
///   "workspace_root":"/w"}"#;
/// let workspace = Workspace::from_metadata_json(json, "/w".into())?;
/// assert_eq!(rule_4(&workspace).findings.len(), 1, "moso-core cannot depend on the facade");
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[must_use]
pub fn rule_4(workspace: &Workspace) -> RuleOutcome {
    let Some(facade) = workspace.package("moso") else {
        return RuleOutcome {
            id: 4,
            title: "nothing the facade depends on depends back on the facade".to_owned(),
            status: Status::Skipped,
            findings: Vec::new(),
            detail: Some("the facade is not in the workspace".to_owned()),
        };
    };
    let reachable = facade_closure(workspace, facade);
    let findings: Vec<String> = workspace
        .packages
        .iter()
        .filter(|package| package.is_moso_crate() && package.name != "moso")
        .filter(|package| {
            package
                .dep("moso")
                .is_some_and(|dep| dep.is_build_relevant())
        })
        .filter(|package| reachable.contains(package.name.as_str()))
        .map(|package| {
            format!(
                "{} depends on the facade and the facade depends on it — a cycle cargo cannot \
                 publish and a feature-resolution loop nobody can reason about",
                package.name
            )
        })
        .collect();
    let exception = workspace
        .package(FACADE_DEPENDENT_EXCEPTION)
        .and_then(|package| package.dep("moso"))
        .is_some();
    RuleOutcome {
        id: 4,
        title: "nothing the facade depends on depends back on the facade".to_owned(),
        status: if findings.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        findings,
        detail: Some(format!(
            "{} crate(s) reachable from the facade; {FACADE_DEPENDENT_EXCEPTION} {} the facade, \
             which is the documented exception",
            reachable.len(),
            if exception {
                "depends on"
            } else {
                "does not depend on"
            }
        )),
    }
}

fn facade_closure<'a>(workspace: &'a Workspace, facade: &'a Package) -> BTreeSet<&'a str> {
    let mut reachable: BTreeSet<&str> = BTreeSet::new();
    let mut queue: Vec<&str> = facade
        .deps
        .iter()
        .filter(|dep| dep.is_build_relevant())
        .map(|dep| dep.name.as_str())
        .collect();
    while let Some(name) = queue.pop() {
        if !reachable.insert(name) {
            continue;
        }
        if let Some(package) = workspace.package(name) {
            for dep in package.deps.iter().filter(|dep| dep.is_build_relevant()) {
                queue.push(dep.name.as_str());
            }
        }
    }
    reachable
}

/// Rule 5 — batteries couple only along declared edges.
///
/// ```
/// use xtask::deps::{Topology, rule_5};
/// use xtask::meta::Workspace;
///
/// let json = r#"{"packages":[{"name":"moso-kv","version":"0.1.0",
///   "manifest_path":"/w/kv/Cargo.toml","publish":null,
///   "targets":[{"name":"moso-kv","kind":["lib"]}],
///   "dependencies":[{"name":"moso-orm","kind":null,"optional":false}]}],
///   "workspace_root":"/w"}"#;
/// let topology = Topology::parse(
///     "[batteries]\nmembers = [\"moso-kv\", \"moso-orm\"]\n[edges]\n\"moso-kv\" = []")?;
/// let workspace = Workspace::from_metadata_json(json, "/w".into())?;
/// let outcome = rule_5(&workspace, &topology);
/// assert_eq!(outcome.findings.len(), 1);
/// assert!(outcome.findings[0].contains("dep-edges.toml"), "the fix names the file");
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[must_use]
pub fn rule_5(workspace: &Workspace, topology: &Topology) -> RuleOutcome {
    let present: Vec<&Package> = workspace
        .packages
        .iter()
        .filter(|package| topology.is_battery(&package.name))
        .collect();
    if present.is_empty() {
        return RuleOutcome {
            id: 5,
            title: "batteries depend on each other only along declared edges".to_owned(),
            status: Status::Skipped,
            findings: Vec::new(),
            detail: Some("no battery crate is in the workspace yet".to_owned()),
        };
    }
    let mut findings = Vec::new();
    for package in &present {
        for dep in package.deps.iter().filter(|dep| dep.is_build_relevant()) {
            if !topology.is_battery(&dep.name) || dep.name == package.name {
                continue;
            }
            if !topology.edge_allowed(&package.name, &dep.name) {
                findings.push(format!(
                    "{} depends on {}, which is not a declared edge — either invert the \
                     dependency or add it to xtask/allow/dep-edges.toml with the reason",
                    package.name, dep.name
                ));
            }
        }
    }
    RuleOutcome {
        id: 5,
        title: "batteries depend on each other only along declared edges".to_owned(),
        status: if findings.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        findings,
        detail: Some(format!("{} battery crate(s) present", present.len())),
    }
}

/// Rule 6 — the third-party crate-count budget.
fn rule_6(root: &Path, workspace: &Workspace, options: &Options) -> Result<RuleOutcome> {
    if !workspace.has("moso") {
        return Ok(RuleOutcome {
            id: 6,
            title: "the third-party crate count is within budget".to_owned(),
            status: Status::Skipped,
            findings: Vec::new(),
            detail: Some("the facade is not in the workspace".to_owned()),
        });
    }
    let mut findings = Vec::new();
    let mut details = Vec::new();

    let default_graph = resolved_packages(root, "moso", &[])?;
    let default_count = third_party_count(&default_graph);
    details.push(format!(
        "default features: {default_count}/{} third-party crates",
        options.default_budget
    ));
    if default_count > options.default_budget {
        findings.push(format!(
            "the default feature set resolves {default_count} third-party crates, over the \
             budget of {}. Every crate here is on the critical path of `cargo add moso`",
            options.default_budget
        ));
    }

    if facade_has_feature(workspace, "full")? {
        let full_graph = resolved_packages(root, "moso", &["full"])?;
        let full_count = third_party_count(&full_graph);
        details.push(format!(
            "full features: {full_count}/{} third-party crates",
            options.full_budget
        ));
        if full_count > options.full_budget {
            findings.push(format!(
                "the `full` feature set resolves {full_count} third-party crates, over the \
                 budget of {}",
                options.full_budget
            ));
        }
    } else {
        details.push(
            "the facade has no `full` feature, so the 260-crate half of the budget has nothing \
             to measure (recorded as a known gap in 63-implementation-status.md)"
                .to_owned(),
        );
    }

    Ok(RuleOutcome {
        id: 6,
        title: "the third-party crate count is within budget".to_owned(),
        status: if findings.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        findings,
        detail: Some(details.join("; ")),
    })
}

/// How many crates in a resolved graph are third-party.
///
/// ```
/// use std::collections::BTreeSet;
/// use xtask::deps::third_party_count;
/// use xtask::meta::ResolvedPackage;
///
/// let mut graph = BTreeSet::new();
/// graph.insert(ResolvedPackage { name: "moso".into(), version: "0.1.0".into(), local: true });
/// graph.insert(ResolvedPackage { name: "axum".into(), version: "0.8.9".into(), local: false });
/// assert_eq!(third_party_count(&graph), 1);
/// ```
#[must_use]
pub fn third_party_count(graph: &BTreeSet<crate::meta::ResolvedPackage>) -> usize {
    graph
        .iter()
        .filter(|package| !package.local)
        .map(|package| package.name.as_str())
        .collect::<BTreeSet<_>>()
        .len()
}

fn facade_has_feature(workspace: &Workspace, feature: &str) -> Result<bool> {
    let Some(facade) = workspace.package("moso") else {
        return Ok(false);
    };
    let text = std::fs::read_to_string(&facade.manifest_path)
        .map_err(|error| Error::from(error).with_context("reading the facade's manifest"))?;
    let manifest: toml::Value = toml::from_str(&text)?;
    Ok(manifest
        .get("features")
        .and_then(toml::Value::as_table)
        .is_some_and(|features| features.contains_key(feature)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proc_macro_crate_may_depend_on_another_proc_macro_crate() {
        let json = r#"{"packages":[
          {"name":"moso-orm-macros","version":"0.1.0","manifest_path":"/w/a/Cargo.toml",
           "publish":null,"targets":[{"name":"moso-orm-macros","kind":["proc-macro"]}],
           "dependencies":[{"name":"moso-macros","kind":null,"optional":false}]},
          {"name":"moso-macros","version":"0.1.0","manifest_path":"/w/b/Cargo.toml",
           "publish":null,"targets":[{"name":"moso-macros","kind":["proc-macro"]}],
           "dependencies":[]}],
          "workspace_root":"/w"}"#;
        let workspace =
            Workspace::from_metadata_json(json, PathBuf::from("/w")).expect("valid metadata");
        let outcome = rule_1(&workspace, &Topology::default());
        assert_eq!(outcome.status, Status::Pass, "{:?}", outcome.findings);
    }

    #[test]
    fn a_dev_dependency_on_a_runtime_crate_is_not_a_rule_1_violation() {
        // `moso-macros` has a dev-dependency on `moso` for its doctests, which
        // cannot put code into a downstream build.
        let json = r#"{"packages":[{"name":"moso-macros","version":"0.1.0",
          "manifest_path":"/w/m/Cargo.toml","publish":null,
          "targets":[{"name":"moso-macros","kind":["proc-macro"]}],
          "dependencies":[{"name":"moso","kind":"dev","optional":false}]}],
          "workspace_root":"/w"}"#;
        let workspace =
            Workspace::from_metadata_json(json, PathBuf::from("/w")).expect("valid metadata");
        assert_eq!(
            rule_1(&workspace, &Topology::default()).status,
            Status::Pass
        );
    }

    #[test]
    fn rule_4_permits_the_documented_exception() {
        // moso-test depends on the facade; the facade does not depend on
        // moso-test, so the amended rule holds.
        let json = r#"{"packages":[
          {"name":"moso","version":"0.1.0","manifest_path":"/w/f/Cargo.toml","publish":null,
           "targets":[{"name":"moso","kind":["lib"]}],
           "dependencies":[{"name":"moso-core","kind":null,"optional":false}]},
          {"name":"moso-core","version":"0.1.0","manifest_path":"/w/c/Cargo.toml","publish":null,
           "targets":[{"name":"moso-core","kind":["lib"]}],"dependencies":[]},
          {"name":"moso-test","version":"0.1.0","manifest_path":"/w/t/Cargo.toml","publish":null,
           "targets":[{"name":"moso-test","kind":["lib"]}],
           "dependencies":[{"name":"moso","kind":null,"optional":false}]}],
          "workspace_root":"/w"}"#;
        let workspace =
            Workspace::from_metadata_json(json, PathBuf::from("/w")).expect("valid metadata");
        let outcome = rule_4(&workspace);
        assert_eq!(outcome.status, Status::Pass, "{:?}", outcome.findings);
        assert!(
            outcome
                .detail
                .expect("a detail line")
                .contains("depends on"),
            "the exception is recorded even when it passes"
        );
    }

    #[test]
    fn rule_5_is_skipped_rather_than_passed_when_no_battery_exists() {
        let json = r#"{"packages":[{"name":"moso-core","version":"0.1.0",
          "manifest_path":"/w/c/Cargo.toml","publish":null,
          "targets":[{"name":"moso-core","kind":["lib"]}],"dependencies":[]}],
          "workspace_root":"/w"}"#;
        let workspace =
            Workspace::from_metadata_json(json, PathBuf::from("/w")).expect("valid metadata");
        let topology = Topology::parse("[batteries]\nmembers = [\"moso-orm\"]").expect("valid");
        let outcome = rule_5(&workspace, &topology);
        assert_eq!(outcome.status, Status::Skipped);
        assert!(outcome.status.ok(), "a skip must not fail the gate");
    }

    #[test]
    fn rule_5_allows_a_declared_edge() {
        let json = r#"{"packages":[
          {"name":"moso-orm","version":"0.1.0","manifest_path":"/w/o/Cargo.toml","publish":null,
           "targets":[{"name":"moso-orm","kind":["lib"]}],
           "dependencies":[{"name":"moso-sql","kind":null,"optional":false},
                           {"name":"sqlx","kind":null,"optional":false}]},
          {"name":"moso-sql","version":"0.1.0","manifest_path":"/w/s/Cargo.toml","publish":null,
           "targets":[{"name":"moso-sql","kind":["lib"]}],"dependencies":[]}],
          "workspace_root":"/w"}"#;
        let workspace =
            Workspace::from_metadata_json(json, PathBuf::from("/w")).expect("valid metadata");
        let topology = Topology::parse(
            "[batteries]\nmembers = [\"moso-orm\", \"moso-sql\"]\n[edges]\n\"moso-orm\" = [\"moso-sql\"]\n\"moso-sql\" = []",
        )
        .expect("valid");
        let outcome = rule_5(&workspace, &topology);
        assert_eq!(outcome.status, Status::Pass, "{:?}", outcome.findings);
    }

    #[test]
    fn the_committed_topology_declares_every_battery_it_governs() {
        let root = crate::util::workspace_root().expect("a workspace");
        let text = std::fs::read_to_string(root.join("xtask/allow/dep-edges.toml"))
            .expect("the committed topology");
        let topology = Topology::parse(&text).expect("valid TOML");
        for battery in &topology.batteries.members {
            assert!(
                topology.edges.contains_key(battery),
                "{battery} is governed by rule 5 but has no [edges] entry, so every edge out of \
                 it would fail with no way to declare one"
            );
        }
        for (from, targets) in &topology.edges {
            assert!(
                topology.is_battery(from),
                "{from} has declared edges but is not listed as a battery"
            );
            for to in targets {
                assert!(
                    topology.is_battery(to),
                    "{from} declares an edge to {to}, which is not a battery"
                );
            }
        }
    }

    #[test]
    fn third_party_counting_ignores_path_dependencies_and_duplicates() {
        let mut graph = BTreeSet::new();
        for (name, version, local) in [
            ("moso", "0.1.0", true),
            ("moso-core", "0.1.0", true),
            ("axum", "0.8.9", false),
            ("hashbrown", "0.14.0", false),
            ("hashbrown", "0.15.0", false),
        ] {
            graph.insert(crate::meta::ResolvedPackage {
                name: name.to_owned(),
                version: version.to_owned(),
                local,
            });
        }
        assert_eq!(
            third_party_count(&graph),
            2,
            "two third-party names, whatever the version count"
        );
    }
}
