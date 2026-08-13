//! What cargo knows about the workspace, in the shape `xtask` asks questions in.
//!
//! Two sources, deliberately kept apart:
//!
//! * [`Workspace::load`] reads `cargo metadata --no-deps`, which is the
//!   *declared* graph — the edges a human wrote in a manifest. The dependency
//!   rules in `docs/00-foundations/03-crate-layout.md` are rules about those
//!   edges, so that is what they are checked against.
//! * [`resolved_packages`] reads `cargo tree`, which is the *resolved* graph
//!   after feature unification. The crate-count budget and "must not depend on
//!   `sqlx`, even by accident, six levels down" are questions about that one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Deserialize;

use crate::bail;
use crate::util::{Cmd, Error, Result};

/// One dependency edge as declared in a manifest.
///
/// ```
/// use xtask::meta::{Dep, DepKind};
///
/// let dep = Dep { name: "sqlx".into(), kind: DepKind::Normal, req: "0.9".into(), optional: true };
/// assert!(dep.is_build_relevant());
/// assert!(dep.has_version_requirement());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dep {
    /// The dependency's package name.
    pub name: String,
    /// Whether it is a normal, development or build dependency.
    pub kind: DepKind,
    /// The version requirement, as cargo resolved the manifest. A path
    /// dependency with no `version` key reads `*`.
    pub req: String,
    /// Whether it is behind a cargo feature.
    pub optional: bool,
}

impl Dep {
    /// Whether this edge can put code into a downstream library build.
    ///
    /// Development dependencies cannot, which is why `moso-test` depending on
    /// the facade in a `[dev-dependencies]` table would not be a rule
    /// violation, and depending on it in `[dependencies]` is.
    ///
    /// ```
    /// use xtask::meta::{Dep, DepKind};
    ///
    /// let dev = Dep { name: "trybuild".into(), kind: DepKind::Development, req: "1".into(),
    ///     optional: false };
    /// assert!(!dev.is_build_relevant());
    /// ```
    #[must_use]
    pub fn is_build_relevant(&self) -> bool {
        matches!(self.kind, DepKind::Normal | DepKind::Build)
    }

    /// Whether the edge names a version, as opposed to being path-only.
    ///
    /// This decides whether `cargo publish` has to resolve the dependency
    /// against the registry: a path-only dependency is *stripped* from the
    /// packaged manifest, and a versioned one is not.
    ///
    /// ```
    /// use xtask::meta::{Dep, DepKind};
    ///
    /// let path_only = Dep { name: "moso".into(), kind: DepKind::Development,
    ///     req: "*".into(), optional: false };
    /// assert!(!path_only.has_version_requirement());
    ///
    /// let pinned = Dep { name: "moso".into(), kind: DepKind::Development,
    ///     req: "=0.1.0".into(), optional: false };
    /// assert!(pinned.has_version_requirement());
    /// ```
    #[must_use]
    pub fn has_version_requirement(&self) -> bool {
        !matches!(self.req.trim(), "" | "*")
    }
}

/// Which table a dependency was declared in.
///
/// ```
/// use xtask::meta::DepKind;
///
/// assert_eq!(DepKind::from_metadata(None), DepKind::Normal);
/// assert_eq!(DepKind::from_metadata(Some("dev")), DepKind::Development);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepKind {
    /// `[dependencies]`.
    Normal,
    /// `[dev-dependencies]`.
    Development,
    /// `[build-dependencies]`.
    Build,
}

impl DepKind {
    /// Maps cargo metadata's `kind` field, where `null` means normal.
    ///
    /// ```
    /// use xtask::meta::DepKind;
    ///
    /// assert_eq!(DepKind::from_metadata(Some("build")), DepKind::Build);
    /// ```
    #[must_use]
    pub fn from_metadata(kind: Option<&str>) -> Self {
        match kind {
            Some("dev") => Self::Development,
            Some("build") => Self::Build,
            _ => Self::Normal,
        }
    }
}

/// One workspace member.
///
/// ```
/// use xtask::meta::Package;
///
/// let package = Package {
///     name: "moso-macros".into(),
///     version: "0.1.0".into(),
///     manifest_path: "/tmp/Cargo.toml".into(),
///     publishable: true,
///     has_lib: true,
///     is_proc_macro: true,
///     deps: Vec::new(),
/// };
/// assert!(package.is_moso_crate());
/// ```
#[derive(Clone, Debug)]
pub struct Package {
    /// The package name, as cargo spells it.
    pub name: String,
    /// The version in the manifest, after workspace inheritance.
    pub version: String,
    /// Absolute path to the package's `Cargo.toml`.
    pub manifest_path: PathBuf,
    /// Whether `cargo publish` would accept it (`publish = false` makes this
    /// `false`).
    pub publishable: bool,
    /// Whether the package has a library target, and therefore whether
    /// `cargo rustdoc` can produce JSON for it.
    pub has_lib: bool,
    /// Whether that library target is a procedural macro.
    pub is_proc_macro: bool,
    /// Every dependency declared in the manifest, in all three tables.
    pub deps: Vec<Dep>,
}

impl Package {
    /// Whether this is one of Moso's own crates.
    ///
    /// ```
    /// # use xtask::meta::Package;
    /// # fn p(name: &str) -> Package { Package { name: name.into(), version: "0".into(),
    /// #   manifest_path: "/x".into(), publishable: true, has_lib: true, is_proc_macro: false,
    /// #   deps: Vec::new() } }
    /// assert!(p("moso").is_moso_crate());
    /// assert!(p("moso-orm").is_moso_crate());
    /// assert!(!p("example-crud").is_moso_crate());
    /// ```
    #[must_use]
    pub fn is_moso_crate(&self) -> bool {
        self.name == "moso" || self.name.starts_with("moso-")
    }

    /// The dependency with this name, in any table.
    ///
    /// ```
    /// # use xtask::meta::{Dep, DepKind, Package};
    /// let package = Package { name: "moso-test".into(), version: "0".into(),
    ///     manifest_path: "/x".into(), publishable: true, has_lib: true, is_proc_macro: false,
    ///     deps: vec![Dep { name: "moso".into(), kind: DepKind::Normal, req: "=0.1.0".into(),
    ///         optional: false }] };
    /// assert!(package.dep("moso").is_some());
    /// assert!(package.dep("sqlx").is_none());
    /// ```
    #[must_use]
    pub fn dep(&self, name: &str) -> Option<&Dep> {
        self.deps.iter().find(|dep| dep.name == name)
    }
}

/// The workspace, as declared.
///
/// ```no_run
/// let workspace = xtask::meta::Workspace::load()?;
/// assert!(workspace.package("moso-core").is_some());
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Workspace {
    /// The workspace root directory.
    pub root: PathBuf,
    /// Every member, in the order cargo listed them.
    pub packages: Vec<Package>,
}

impl Workspace {
    /// Runs `cargo metadata --no-deps` and parses what matters.
    ///
    /// ```no_run
    /// let workspace = xtask::meta::Workspace::load()?;
    /// assert!(workspace.moso_crates().iter().any(|p| p.name == "moso-schema"));
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn load() -> Result<Self> {
        let root = crate::util::workspace_root()?;
        let output = Cmd::cargo()
            .cwd(&root)
            .args(["metadata", "--no-deps", "--format-version", "1"])
            .run()
            .map_err(|error| error.with_context("cargo metadata"))?;
        Self::from_metadata_json(&output.stdout, root)
    }

    /// Parses the JSON `cargo metadata --no-deps` prints.
    ///
    /// Split out from [`Workspace::load`] so the dependency rules can be tested
    /// against a hand-written graph without a cargo invocation.
    ///
    /// ```
    /// use xtask::meta::Workspace;
    ///
    /// let json = r#"{"packages":[{"name":"moso-core","version":"0.1.0",
    ///   "manifest_path":"/w/crates/moso-core/Cargo.toml","publish":null,
    ///   "targets":[{"name":"moso-core","kind":["lib"]}],
    ///   "dependencies":[{"name":"axum","kind":null,"optional":false}]}],
    ///   "workspace_root":"/w"}"#;
    /// let workspace = Workspace::from_metadata_json(json, "/w".into())?;
    /// assert_eq!(workspace.packages.len(), 1);
    /// assert!(workspace.package("moso-core").unwrap().has_lib);
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn from_metadata_json(json: &str, root: PathBuf) -> Result<Self> {
        #[derive(Deserialize)]
        struct Metadata {
            packages: Vec<RawPackage>,
        }
        #[derive(Deserialize)]
        struct RawPackage {
            name: String,
            version: String,
            manifest_path: PathBuf,
            #[serde(default)]
            publish: Option<Vec<String>>,
            #[serde(default)]
            targets: Vec<RawTarget>,
            #[serde(default)]
            dependencies: Vec<RawDep>,
        }
        #[derive(Deserialize)]
        struct RawTarget {
            kind: Vec<String>,
        }
        #[derive(Deserialize)]
        struct RawDep {
            name: String,
            #[serde(default)]
            kind: Option<String>,
            #[serde(default)]
            req: String,
            #[serde(default)]
            optional: bool,
        }

        let metadata: Metadata = serde_json::from_str(json)
            .map_err(|error| Error::from(error).with_context("cargo metadata output"))?;
        let packages = metadata
            .packages
            .into_iter()
            .map(|raw| Package {
                name: raw.name,
                version: raw.version,
                manifest_path: raw.manifest_path,
                // `publish = false` serialises as an empty array.
                publishable: raw.publish.as_ref().is_none_or(|list| !list.is_empty()),
                has_lib: raw
                    .targets
                    .iter()
                    .any(|target| target.kind.iter().any(|k| k == "lib" || k == "proc-macro")),
                is_proc_macro: raw
                    .targets
                    .iter()
                    .any(|target| target.kind.iter().any(|k| k == "proc-macro")),
                deps: raw
                    .dependencies
                    .into_iter()
                    .map(|dep| Dep {
                        name: dep.name,
                        kind: DepKind::from_metadata(dep.kind.as_deref()),
                        req: dep.req,
                        optional: dep.optional,
                    })
                    .collect(),
            })
            .collect();
        Ok(Self { root, packages })
    }

    /// The member with this name, if the workspace has one.
    ///
    /// ```no_run
    /// let workspace = xtask::meta::Workspace::load()?;
    /// assert!(workspace.package("moso-sql").is_none(), "not built yet");
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn package(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|package| package.name == name)
    }

    /// Whether a member with this name exists. The "skip with a warning when
    /// the crate does not exist yet" check every gate starts with.
    ///
    /// ```no_run
    /// let workspace = xtask::meta::Workspace::load()?;
    /// assert!(workspace.has("moso-core"));
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.package(name).is_some()
    }

    /// Every member whose name is `moso` or starts with `moso-`.
    ///
    /// ```no_run
    /// let workspace = xtask::meta::Workspace::load()?;
    /// assert!(workspace.moso_crates().len() >= 8);
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn moso_crates(&self) -> Vec<&Package> {
        self.packages
            .iter()
            .filter(|package| package.is_moso_crate())
            .collect()
    }

    /// Cycles that would stop `cargo publish` even though [`publish_order`]
    /// found an order.
    ///
    /// [`publish_order`]: Workspace::publish_order
    ///
    /// A `[dev-dependencies]` edge does not constrain build order, so
    /// [`publish_order`] ignores it — but `cargo publish` still has to *resolve*
    /// it against the registry, unless it is path-only, in which case cargo
    /// strips it from the packaged manifest. A versioned dev-dependency on
    /// another workspace member is therefore a release blocker that no amount
    /// of ordering fixes, and it is invisible until the day of the release.
    ///
    /// ```
    /// use xtask::meta::Workspace;
    ///
    /// // `moso-schema` dev-depends on the facade with a version pin, and the
    /// // facade depends on `moso-schema`: neither can be published first.
    /// let json = r#"{"packages":[
    ///   {"name":"moso","version":"0.1.0","manifest_path":"/w/f/Cargo.toml","publish":null,
    ///    "targets":[{"name":"moso","kind":["lib"]}],
    ///    "dependencies":[{"name":"moso-schema","kind":null,"req":"=0.1.0","optional":false}]},
    ///   {"name":"moso-schema","version":"0.1.0","manifest_path":"/w/s/Cargo.toml","publish":null,
    ///    "targets":[{"name":"moso-schema","kind":["lib"]}],
    ///    "dependencies":[{"name":"moso","kind":"dev","req":"=0.1.0","optional":false}]}],
    ///   "workspace_root":"/w"}"#;
    /// let workspace = Workspace::from_metadata_json(json, "/w".into())?;
    /// let blockers = workspace.publish_blockers();
    /// assert_eq!(blockers.len(), 1);
    /// assert!(blockers[0].contains("moso-schema"), "{:?}", blockers);
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn publish_blockers(&self) -> Vec<String> {
        let mut blockers = Vec::new();
        for package in self.packages.iter().filter(|p| p.publishable) {
            for dep in &package.deps {
                if dep.kind != DepKind::Development || !dep.has_version_requirement() {
                    continue;
                }
                let Some(target) = self.package(&dep.name) else {
                    continue;
                };
                // Only a *cycle* is a blocker: a dev-dependency on a member that
                // is published earlier resolves fine.
                let reaches_back = self.reachable_from(target).contains(package.name.as_str());
                if reaches_back {
                    blockers.push(format!(
                        "{} dev-depends on {} with `{}`, and {} depends on {} — neither can be \
                         published first. Fix: declare the dev-dependency path-only \
                         (`{} = {{ path = \"../{}\" }}`, no version), which cargo strips from the \
                         packaged manifest",
                        package.name, dep.name, dep.req, dep.name, package.name, dep.name, dep.name
                    ));
                }
            }
        }
        blockers.sort_unstable();
        blockers.dedup();
        blockers
    }

    /// Every workspace member reachable from `package` along build-relevant
    /// edges.
    ///
    /// ```
    /// use xtask::meta::Workspace;
    ///
    /// let json = r#"{"packages":[
    ///   {"name":"a","version":"0.1.0","manifest_path":"/w/a/Cargo.toml","publish":null,
    ///    "targets":[{"name":"a","kind":["lib"]}],
    ///    "dependencies":[{"name":"b","kind":null,"req":"=0.1.0","optional":false}]},
    ///   {"name":"b","version":"0.1.0","manifest_path":"/w/b/Cargo.toml","publish":null,
    ///    "targets":[{"name":"b","kind":["lib"]}],"dependencies":[]}],
    ///   "workspace_root":"/w"}"#;
    /// let workspace = Workspace::from_metadata_json(json, "/w".into())?;
    /// let from_a = workspace.reachable_from(workspace.package("a").expect("a"));
    /// assert!(from_a.contains("b"));
    /// assert!(workspace.reachable_from(workspace.package("b").expect("b")).is_empty());
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn reachable_from<'a>(&'a self, package: &'a Package) -> BTreeSet<&'a str> {
        let mut reachable: BTreeSet<&str> = BTreeSet::new();
        let mut queue: Vec<&str> = package
            .deps
            .iter()
            .filter(|dep| dep.is_build_relevant())
            .map(|dep| dep.name.as_str())
            .collect();
        while let Some(name) = queue.pop() {
            if !reachable.insert(name) {
                continue;
            }
            if let Some(next) = self.package(name) {
                for dep in next.deps.iter().filter(|dep| dep.is_build_relevant()) {
                    queue.push(dep.name.as_str());
                }
            }
        }
        reachable
    }

    /// Members in an order where every package comes after everything it
    /// depends on — the order `cargo publish` has to be run in.
    ///
    /// Only intra-workspace, build-relevant edges are considered: a
    /// development-dependency cycle between two members does not constrain the
    /// order. It can still stop a release, which is what
    /// [`publish_blockers`](Workspace::publish_blockers) is for.
    ///
    /// ```
    /// use xtask::meta::Workspace;
    ///
    /// let json = r#"{"packages":[
    ///   {"name":"moso","version":"0.1.0","manifest_path":"/w/a/Cargo.toml","publish":null,
    ///    "targets":[{"name":"moso","kind":["lib"]}],
    ///    "dependencies":[{"name":"moso-core","kind":null,"req":"=0.1.0","optional":false}]},
    ///   {"name":"moso-core","version":"0.1.0","manifest_path":"/w/b/Cargo.toml","publish":null,
    ///    "targets":[{"name":"moso-core","kind":["lib"]}],"dependencies":[]}],
    ///   "workspace_root":"/w"}"#;
    /// let workspace = Workspace::from_metadata_json(json, "/w".into())?;
    /// let order: Vec<&str> = workspace.publish_order()?.iter().map(|p| p.name.as_str()).collect();
    /// assert_eq!(order, ["moso-core", "moso"]);
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn publish_order(&self) -> Result<Vec<&Package>> {
        let members: BTreeSet<&str> = self.packages.iter().map(|p| p.name.as_str()).collect();
        let mut pending: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for package in &self.packages {
            let edges = package
                .deps
                .iter()
                .filter(|dep| dep.is_build_relevant())
                .map(|dep| dep.name.as_str())
                .filter(|name| members.contains(name))
                .collect();
            pending.insert(package.name.as_str(), edges);
        }

        let mut ordered: Vec<&Package> = Vec::new();
        while !pending.is_empty() {
            let ready: Vec<&str> = pending
                .iter()
                .filter(|(_, edges)| edges.is_empty())
                .map(|(name, _)| *name)
                .collect();
            if ready.is_empty() {
                let stuck: Vec<&str> = pending.keys().copied().collect();
                bail!(
                    "the workspace has a dependency cycle among {}; publishing has no valid order",
                    stuck.join(", ")
                );
            }
            for name in ready {
                pending.remove(name);
                for edges in pending.values_mut() {
                    edges.remove(name);
                }
                if let Some(package) = self.package(name) {
                    ordered.push(package);
                }
            }
        }
        Ok(ordered)
    }
}

/// One node of the resolved dependency graph.
///
/// ```
/// use xtask::meta::ResolvedPackage;
///
/// let package = ResolvedPackage { name: "tokio".into(), version: "1.53.1".into(), local: false };
/// assert_eq!(package.to_string(), "tokio 1.53.1");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResolvedPackage {
    /// The package name.
    pub name: String,
    /// The resolved version.
    pub version: String,
    /// Whether it is a path dependency inside this workspace, and therefore not
    /// a third-party crate for the purposes of the budget.
    pub local: bool,
}

impl std::fmt::Display for ResolvedPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.name, self.version)
    }
}

/// The resolved, feature-aware, host-target dependency closure of one package.
///
/// Reads `cargo tree --edges normal`, which is the only tool that answers "what
/// does this actually compile into" without re-implementing feature
/// unification. Development and build dependencies are excluded because they do
/// not ship.
///
/// ```no_run
/// use xtask::meta::resolved_packages;
///
/// let root = xtask::util::workspace_root()?;
/// let graph = resolved_packages(&root, "moso", &[])?;
/// assert!(graph.iter().any(|p| p.name == "axum"));
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn resolved_packages(
    root: &std::path::Path,
    package: &str,
    features: &[&str],
) -> Result<BTreeSet<ResolvedPackage>> {
    let mut cmd = Cmd::cargo().cwd(root).args([
        "tree",
        "--package",
        package,
        "--edges",
        "normal",
        "--prefix",
        "none",
        "--format",
        "{p}",
    ]);
    if !features.is_empty() {
        cmd = cmd.args(["--features", &features.join(",")]);
    }
    let output = cmd
        .run()
        .map_err(|error| error.with_context(format!("cargo tree -p {package}")))?;
    Ok(parse_cargo_tree(&output.stdout))
}

/// Parses `cargo tree --prefix none --format {p}` output.
///
/// Each line is `name version [(path)]`, with `(*)` marking a subtree cargo
/// elided because it printed it already.
///
/// ```
/// use xtask::meta::parse_cargo_tree;
///
/// let text = "moso v0.1.0 (/w/crates/moso)\naxum v0.8.9\naxum v0.8.9 (*)\n\
///             serde_derive v1.0.229 (proc-macro)\n";
/// let graph = parse_cargo_tree(text);
/// assert_eq!(graph.len(), 3, "the elided repeat is the same node");
/// assert!(graph.iter().any(|p| p.name == "axum" && !p.local));
/// assert!(graph.iter().any(|p| p.name == "moso" && p.local));
/// // A third-party proc macro is third-party: it is compiled, and compiling it
/// // is exactly the cost the budget is about.
/// assert!(graph.iter().any(|p| p.name == "serde_derive" && !p.local));
/// ```
#[must_use]
pub fn parse_cargo_tree(text: &str) -> BTreeSet<ResolvedPackage> {
    let mut graph = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let line = line.strip_suffix(" (*)").unwrap_or(line);
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else { continue };
        let Some(version) = parts.next() else {
            continue;
        };
        if !version.starts_with('v') {
            continue;
        }
        // A path dependency is printed with its directory: `name vX (/path)`.
        // `(proc-macro)` is *not* a locality marker — third-party proc macros
        // carry it too, and they cost a compilation like anything else.
        let local = line.contains(" (/");
        graph.insert(ResolvedPackage {
            name: name.to_owned(),
            version: version.trim_start_matches('v').to_owned(),
            local,
        });
    }
    graph
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREE_MEMBERS: &str = r#"{
      "packages": [
        {"name":"moso","version":"0.1.0","manifest_path":"/w/crates/moso/Cargo.toml",
         "publish":null,"targets":[{"name":"moso","kind":["lib"]}],
         "dependencies":[
           {"name":"moso-core","kind":null,"optional":false},
           {"name":"moso-macros","kind":null,"optional":false}]},
        {"name":"moso-core","version":"0.1.0","manifest_path":"/w/crates/moso-core/Cargo.toml",
         "publish":null,"targets":[{"name":"moso-core","kind":["lib"]}],
         "dependencies":[{"name":"axum","kind":null,"optional":false}]},
        {"name":"moso-macros","version":"0.1.0","manifest_path":"/w/crates/moso-macros/Cargo.toml",
         "publish":null,"targets":[{"name":"moso-macros","kind":["proc-macro"]}],
         "dependencies":[{"name":"syn","kind":null,"optional":false},
                         {"name":"trybuild","kind":"dev","optional":false}]}
      ],
      "workspace_root": "/w"
    }"#;

    fn workspace() -> Workspace {
        Workspace::from_metadata_json(THREE_MEMBERS, PathBuf::from("/w")).expect("valid metadata")
    }

    #[test]
    fn a_proc_macro_target_counts_as_a_library() {
        let macros = workspace().package("moso-macros").cloned().expect("member");
        assert!(macros.has_lib);
        assert!(macros.is_proc_macro);
    }

    #[test]
    fn dependency_kinds_come_through() {
        let macros = workspace().package("moso-macros").cloned().expect("member");
        assert_eq!(macros.dep("syn").expect("syn").kind, DepKind::Normal);
        assert_eq!(
            macros.dep("trybuild").expect("trybuild").kind,
            DepKind::Development
        );
        assert!(
            !macros
                .dep("trybuild")
                .expect("trybuild")
                .is_build_relevant()
        );
    }

    #[test]
    fn publish_order_puts_the_facade_last() {
        let workspace = workspace();
        let order: Vec<&str> = workspace
            .publish_order()
            .expect("acyclic")
            .iter()
            .map(|package| package.name.as_str())
            .collect();
        assert_eq!(order.last(), Some(&"moso"));
        let core = order.iter().position(|n| *n == "moso-core").expect("core");
        let facade = order.iter().position(|n| *n == "moso").expect("facade");
        assert!(core < facade);
    }

    #[test]
    fn a_cycle_is_reported_rather_than_looping_forever() {
        let json = r#"{"packages":[
          {"name":"a","version":"0.1.0","manifest_path":"/w/a/Cargo.toml","publish":null,
           "targets":[{"name":"a","kind":["lib"]}],
           "dependencies":[{"name":"b","kind":null,"optional":false}]},
          {"name":"b","version":"0.1.0","manifest_path":"/w/b/Cargo.toml","publish":null,
           "targets":[{"name":"b","kind":["lib"]}],
           "dependencies":[{"name":"a","kind":null,"optional":false}]}],
          "workspace_root":"/w"}"#;
        let workspace =
            Workspace::from_metadata_json(json, PathBuf::from("/w")).expect("valid metadata");
        let error = workspace.publish_order().expect_err("a cycle");
        assert!(error.to_string().contains("dependency cycle"), "{error}");
    }

    #[test]
    fn publish_false_is_recognised() {
        let json = r#"{"packages":[{"name":"xtask","version":"0.1.0",
          "manifest_path":"/w/xtask/Cargo.toml","publish":[],
          "targets":[{"name":"xtask","kind":["lib"]}],"dependencies":[]}],
          "workspace_root":"/w"}"#;
        let workspace =
            Workspace::from_metadata_json(json, PathBuf::from("/w")).expect("valid metadata");
        assert!(!workspace.package("xtask").expect("member").publishable);
    }

    #[test]
    fn cargo_tree_lines_that_are_not_packages_are_ignored() {
        let text = "\nmoso v0.1.0 (/w/crates/moso)\n[build-dependencies]\nsyn v2.0.119\n";
        let graph = parse_cargo_tree(text);
        assert_eq!(graph.len(), 2);
        assert!(!graph.iter().any(|p| p.name == "[build-dependencies]"));
    }
}
