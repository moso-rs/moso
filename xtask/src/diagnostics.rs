//! `check-diagnostics` — every public trait carries a hand-written error message.
//!
//! `docs/04-devex/41-diagnostics.md` makes this a hard requirement and names the
//! enforcement: *"a CI check (`xtask check-diagnostics`) that fails if a public
//! trait in a Moso crate lacks the attribute or an explicit
//! allow-with-a-reason"*. The proxy metric in the same document is "public
//! traits with a hand-written diagnostic: 100%, CI-enforced". This is the
//! enforcement.
//!
//! # Why this is worth a gate rather than a review habit
//!
//! `#[diagnostic::on_unimplemented]` is invisible in normal use. Nothing fails,
//! no test goes red, no rustdoc page looks different; the only symptom is that
//! one day a user pastes a wall of trait-resolution output into an issue. A
//! trait added on a Friday without one is indistinguishable from a trait with
//! one until it is too late to be cheap. So it is counted.
//!
//! # Two escape hatches, one of which is not an escape
//!
//! * `[[exempt]]` in `xtask/allow/diagnostics.toml` silences a trait, and
//!   requires a reason. Sealed marker traits nobody can name or implement live
//!   here.
//! * `[[known_gap]]` records a trait that *should* have a diagnostic and does
//!   not. It does **not** silence the failure: the gate still exits non-zero,
//!   and the entry only turns the message from "unknown trait" into "known,
//!   here is the fix". `--tolerate-known-gaps` makes the exit code zero for a
//!   tree that is mid-migration; CI must not pass it.
//!
//! The `do_not_recommend` half of the gate has one hatch of its own,
//! `[[blanket_exempt]]`, keyed by `crate-name::TraitName`. It exists because the
//! attribute is not always an improvement: for a bound on an auto trait of a
//! coroutine, rustc's own nested-obligation message points at the `.await` that
//! made the future `!Send`, and `do_not_recommend` replaces that with a generic
//! line naming an unnameable async-block type. An entry here must say which
//! message it is protecting, and there should be a UI snapshot that notices if
//! the compiler's behaviour changes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bail;
use crate::meta::Workspace;
use crate::rustdoc::{BlanketImpl, Doc, TraitDef};
use crate::util::{Error, Result, ui};

/// One allowlist entry.
///
/// ```
/// use xtask::diagnostics::AllowEntry;
///
/// let entry: AllowEntry = toml::from_str(
///     "path = \"moso_core::sealed::Sealed\"\nreason = \"unnameable\"",
/// )?;
/// assert_eq!(entry.path, "moso_core::sealed::Sealed");
/// # Ok::<(), toml::de::Error>(())
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AllowEntry {
    /// The trait's full path, as rustdoc spells it.
    pub path: String,
    /// Why. Checked to be non-empty.
    pub reason: String,
}

/// The parsed `xtask/allow/diagnostics.toml`.
///
/// ```
/// use xtask::diagnostics::AllowList;
///
/// let allow = AllowList::parse(r#"
/// [[exempt]]
/// path = "moso_core::config::secret::sealed::Sealed"
/// reason = "a sealed marker in a private module: no user can name it"
///
/// [[known_gap]]
/// path = "moso_core::router::DynGuard"
/// reason = "the dyn-compatible half of `Guard`; needs its own message"
///
/// [[blanket_exempt]]
/// path = "moso-core::HandlerFuture"
/// reason = "rustc's own nested-obligation message is better here"
/// "#)?;
/// assert!(allow.is_exempt("moso_core::config::secret::sealed::Sealed"));
/// assert!(!allow.is_exempt("moso_core::router::DynGuard"));
/// assert!(allow.known_gap("moso_core::router::DynGuard").is_some());
/// assert!(allow.is_blanket_exempt("moso-core", "HandlerFuture"));
/// assert!(!allow.is_blanket_exempt("moso-core", "DynGuard"));
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AllowList {
    /// Traits that need no diagnostic, with the reason each is exempt.
    #[serde(default)]
    pub exempt: Vec<AllowEntry>,
    /// Traits that should have one and do not. Recorded, not forgiven.
    #[serde(default)]
    pub known_gap: Vec<AllowEntry>,
    /// Blanket impls that are better off *without*
    /// `#[diagnostic::do_not_recommend]`, keyed by `crate-name::TraitName`.
    #[serde(default)]
    pub blanket_exempt: Vec<AllowEntry>,
}

impl AllowList {
    /// Parses the file, rejecting an entry without a reason.
    ///
    /// ```
    /// use xtask::diagnostics::AllowList;
    ///
    /// let error = AllowList::parse("[[exempt]]\npath = \"a::B\"\nreason = \"\"")
    ///     .expect_err("blank reason");
    /// assert!(error.to_string().contains("reason"), "{error}");
    ///
    /// let error = AllowList::parse("[[blanket_exempt]]\npath = \"c::D\"\nreason = \" \"")
    ///     .expect_err("a blanket exemption needs a reason too");
    /// assert!(error.to_string().contains("reason"), "{error}");
    /// ```
    pub fn parse(toml_text: &str) -> Result<Self> {
        let list: Self = toml::from_str(toml_text)
            .map_err(|error| Error::from(error).with_context("xtask/allow/diagnostics.toml"))?;
        for entry in list
            .exempt
            .iter()
            .chain(list.known_gap.iter())
            .chain(list.blanket_exempt.iter())
        {
            if entry.reason.trim().is_empty() {
                bail!(
                    "the diagnostics allowlist entry for `{}` has an empty reason; \
                     say why or remove it",
                    entry.path
                );
            }
        }
        Ok(list)
    }

    /// Reads the allowlist from disk, tolerating its absence.
    ///
    /// ```no_run
    /// use xtask::diagnostics::AllowList;
    ///
    /// let root = xtask::util::workspace_root()?;
    /// let allow = AllowList::load(&root.join("xtask/allow/diagnostics.toml"))?;
    /// assert!(!allow.exempt.is_empty());
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(Error::from(error).with_context(path.display().to_string())),
        }
    }

    /// Whether this trait needs no diagnostic.
    ///
    /// ```
    /// # use xtask::diagnostics::AllowList;
    /// assert!(!AllowList::default().is_exempt("moso_core::Handler"));
    /// ```
    #[must_use]
    pub fn is_exempt(&self, path: &str) -> bool {
        self.exempt.iter().any(|entry| entry.path == path)
    }

    /// The recorded reason this trait is still missing its diagnostic, if it is
    /// recorded at all.
    ///
    /// ```
    /// # use xtask::diagnostics::AllowList;
    /// assert!(AllowList::default().known_gap("moso_core::Handler").is_none());
    /// ```
    #[must_use]
    pub fn known_gap(&self, path: &str) -> Option<&str> {
        self.known_gap
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.reason.as_str())
    }

    /// Whether this crate's blanket impl of this trait is deliberately left
    /// without `#[diagnostic::do_not_recommend]`.
    ///
    /// The key is `crate-name::TraitName` — the package name as cargo spells it,
    /// so `moso-core`, not `moso_core`, because that is what the gate prints.
    ///
    /// ```
    /// # use xtask::diagnostics::AllowList;
    /// assert!(!AllowList::default().is_blanket_exempt("moso-core", "Handler"));
    /// ```
    #[must_use]
    pub fn is_blanket_exempt(&self, crate_name: &str, trait_name: &str) -> bool {
        let key = format!("{crate_name}::{trait_name}");
        self.blanket_exempt.iter().any(|entry| entry.path == key)
    }
}

/// How one trait fared.
///
/// ```
/// use xtask::diagnostics::{Verdict, TraitVerdict};
///
/// let verdict = TraitVerdict { crate_name: "moso-core".into(),
///     path: "moso_core::Handler".into(), location: None, verdict: Verdict::Diagnosed };
/// assert!(verdict.verdict.passes());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The trait carries `#[diagnostic::on_unimplemented]`.
    Diagnosed,
    /// The trait is on the exemption list.
    Exempt,
    /// The trait lacks a diagnostic, and the gap is recorded.
    KnownGap,
    /// The trait lacks a diagnostic and nobody has looked at it.
    Missing,
}

impl Verdict {
    /// Whether this verdict lets the gate stay green.
    ///
    /// ```
    /// use xtask::diagnostics::Verdict;
    ///
    /// assert!(Verdict::Diagnosed.passes());
    /// assert!(Verdict::Exempt.passes());
    /// assert!(!Verdict::KnownGap.passes());
    /// assert!(!Verdict::Missing.passes());
    /// ```
    #[must_use]
    pub fn passes(self) -> bool {
        matches!(self, Self::Diagnosed | Self::Exempt)
    }
}

/// One trait and its verdict.
///
/// ```
/// use xtask::diagnostics::{TraitVerdict, Verdict};
///
/// let verdict = TraitVerdict { crate_name: "moso-test".into(),
///     path: "moso_test::response::IntoStatus".into(),
///     location: Some("crates/moso-test/src/response.rs:31".into()),
///     verdict: Verdict::Missing };
/// assert!(verdict.to_string().contains("IntoStatus"));
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TraitVerdict {
    /// The package the trait is defined in.
    pub crate_name: String,
    /// The trait's full path.
    pub path: String,
    /// `file:line`, when rustdoc recorded a span.
    pub location: Option<String>,
    /// What the gate decided.
    pub verdict: Verdict,
}

impl std::fmt::Display for TraitVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path)?;
        if let Some(location) = &self.location {
            write!(f, " — {location}")?;
        }
        Ok(())
    }
}

/// The whole run.
///
/// ```
/// use xtask::diagnostics::Report;
///
/// let report = Report { traits: Vec::new(), blanket_impls: Vec::new(),
///     format_versions: Default::default(), skipped: Vec::new() };
/// assert_eq!(report.coverage_pct(), 100.0, "no traits is full coverage");
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Report {
    /// Every public trait found, in path order.
    pub traits: Vec<TraitVerdict>,
    /// Every blanket impl that lacks `#[diagnostic::do_not_recommend]`.
    pub blanket_impls: Vec<String>,
    /// The rustdoc format version each crate's answer came from.
    pub format_versions: BTreeMap<String, u64>,
    /// Crates that could not be inspected, and why.
    pub skipped: Vec<String>,
}

impl Report {
    /// The proxy metric from `docs/04-devex/41-diagnostics.md`: the percentage
    /// of public traits with a hand-written diagnostic, counting exemptions as
    /// covered.
    ///
    /// ```
    /// use xtask::diagnostics::{Report, TraitVerdict, Verdict};
    ///
    /// let verdict = |v| TraitVerdict { crate_name: "c".into(), path: "p".into(),
    ///     location: None, verdict: v };
    /// let report = Report {
    ///     traits: vec![verdict(Verdict::Diagnosed), verdict(Verdict::Missing),
    ///                  verdict(Verdict::Exempt), verdict(Verdict::Diagnosed)],
    ///     blanket_impls: Vec::new(), format_versions: Default::default(),
    ///     skipped: Vec::new() };
    /// assert_eq!(report.coverage_pct(), 75.0);
    /// ```
    #[must_use]
    pub fn coverage_pct(&self) -> f64 {
        if self.traits.is_empty() {
            return 100.0;
        }
        let passing = self
            .traits
            .iter()
            .filter(|verdict| verdict.verdict.passes())
            .count();
        passing as f64 / self.traits.len() as f64 * 100.0
    }

    /// The traits with the given verdict.
    ///
    /// ```
    /// use xtask::diagnostics::{Report, Verdict};
    ///
    /// let report = Report { traits: Vec::new(), blanket_impls: Vec::new(),
    ///     format_versions: Default::default(), skipped: Vec::new() };
    /// assert!(report.with_verdict(Verdict::Missing).is_empty());
    /// ```
    #[must_use]
    pub fn with_verdict(&self, verdict: Verdict) -> Vec<&TraitVerdict> {
        self.traits
            .iter()
            .filter(|entry| entry.verdict == verdict)
            .collect()
    }
}

/// Options for one run of the gate.
///
/// ```
/// let options = xtask::diagnostics::Options::default();
/// assert!(options.crates.is_empty(), "empty means every Moso crate");
/// assert!(!options.tolerate_known_gaps);
/// ```
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Packages to inspect; empty means every Moso crate with a library target.
    pub crates: Vec<String>,
    /// Where the allowlist lives, relative to the workspace root.
    pub allow_file: PathBuf,
    /// Exit zero even when a recorded gap is still open.
    pub tolerate_known_gaps: bool,
    /// Also require `#[diagnostic::do_not_recommend]` on every blanket impl.
    pub check_blanket_impls: bool,
    /// Write the machine-readable report here.
    pub json: Option<PathBuf>,
    /// Build artefacts directory; `None` means the workspace's own `target/`.
    pub target_dir: Option<PathBuf>,
}

/// Runs the gate and prints its findings.
///
/// ```no_run
/// let mut options = xtask::diagnostics::Options::default();
/// options.allow_file = "xtask/allow/diagnostics.toml".into();
/// let clean = xtask::diagnostics::run(&options)?;
/// # let _ = clean;
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn run(options: &Options) -> Result<bool> {
    let root = crate::util::workspace_root()?;
    let workspace = Workspace::load()?;
    let allow_file = if options.allow_file.as_os_str().is_empty() {
        PathBuf::from("xtask/allow/diagnostics.toml")
    } else {
        options.allow_file.clone()
    };
    let allow = AllowList::load(&root.join(&allow_file))?;

    let selected: Vec<String> = if options.crates.is_empty() {
        workspace
            .moso_crates()
            .into_iter()
            .filter(|package| package.has_lib)
            .map(|package| package.name.clone())
            .collect()
    } else {
        options.crates.clone()
    };

    let mut report = Report {
        traits: Vec::new(),
        blanket_impls: Vec::new(),
        format_versions: BTreeMap::new(),
        skipped: Vec::new(),
    };

    ui::headline("check-diagnostics");
    for name in &selected {
        let Some(package) = workspace.package(name) else {
            report
                .skipped
                .push(format!("{name} (not a workspace member)"));
            ui::warn(&format!("{name} is not a workspace member yet; skipping"));
            continue;
        };
        if !package.has_lib {
            report
                .skipped
                .push(format!("{name} (no library target to document)"));
            continue;
        }
        let doc = Doc::produce(&root, name, options.target_dir.as_deref())?;
        report
            .format_versions
            .insert(name.clone(), doc.format_version());

        let traits = doc.local_traits();
        for def in &traits {
            report.traits.push(verdict_for(name, def, &allow));
        }
        if options.check_blanket_impls {
            for imp in doc.blanket_impls() {
                if imp.has_do_not_recommend || allow.is_blanket_exempt(name, &imp.trait_name) {
                    continue;
                }
                report.blanket_impls.push(describe_blanket(name, &imp));
            }
        }
        let covered = traits
            .iter()
            .filter(|def| def.has_on_unimplemented() || allow.is_exempt(&def.path))
            .count();
        let message = format!("{name}: {covered}/{} public traits diagnosed", traits.len());
        if covered == traits.len() {
            ui::ok(&message);
        } else {
            ui::fail(&message);
        }
    }

    report.traits.sort_by(|a, b| a.path.cmp(&b.path));

    let missing = report.with_verdict(Verdict::Missing);
    let gaps = report.with_verdict(Verdict::KnownGap);
    for entry in &missing {
        ui::note(&format!("no on_unimplemented: {entry}"));
        ui::note(&format!(
            "  help: add #[diagnostic::on_unimplemented(message = \"`{{Self}}` is not a …\", \
             label = \"…\", note = \"…\")] above `pub trait {}`",
            entry.path.rsplit("::").next().unwrap_or(&entry.path)
        ));
    }
    for entry in &gaps {
        let reason = allow.known_gap(&entry.path).unwrap_or("");
        ui::note(&format!("known gap: {entry} — {}", one_line(reason, 140)));
    }
    for blanket in &report.blanket_impls {
        ui::note(&format!("no do_not_recommend: {blanket}"));
    }

    println!(
        "  coverage {:.1}% of {} public traits ({} exempt, {} known gaps, {} unreviewed)",
        report.coverage_pct(),
        report.traits.len(),
        report.with_verdict(Verdict::Exempt).len(),
        gaps.len(),
        missing.len()
    );

    if let Some(path) = &options.json {
        let text = serde_json::to_string_pretty(&report)?;
        std::fs::write(root.join(path), text + "\n")?;
        ui::note(&format!("report written to {}", path.display()));
    }

    let blanket_ok = report.blanket_impls.is_empty();
    let traits_ok = missing.is_empty() && (gaps.is_empty() || options.tolerate_known_gaps);
    Ok(traits_ok && blanket_ok)
}

/// Decides one trait's verdict.
///
/// ```
/// use xtask::diagnostics::{AllowList, Verdict, verdict_for};
/// use xtask::rustdoc::TraitDef;
///
/// let def = TraitDef { id: "1".into(), path: "moso_orm::Entity".into(), name: "Entity".into(),
///     attrs: String::new(), file: None, line: None };
/// assert_eq!(verdict_for("moso-orm", &def, &AllowList::default()).verdict, Verdict::Missing);
/// ```
#[must_use]
pub fn verdict_for(crate_name: &str, def: &TraitDef, allow: &AllowList) -> TraitVerdict {
    let verdict = if def.has_on_unimplemented() {
        Verdict::Diagnosed
    } else if allow.is_exempt(&def.path) {
        Verdict::Exempt
    } else if allow.known_gap(&def.path).is_some() {
        Verdict::KnownGap
    } else {
        Verdict::Missing
    };
    TraitVerdict {
        crate_name: crate_name.to_owned(),
        path: def.path.clone(),
        location: def.location(),
        verdict,
    }
}

/// Collapses a multi-line allowlist reason onto one line, truncated, so that a
/// gate's output stays a table.
///
/// ```
/// use xtask::diagnostics::one_line;
///
/// assert_eq!(one_line("a\n  b\nc", 40), "a b c");
/// assert_eq!(one_line("abcdef", 4), "a…");
/// ```
#[must_use]
pub fn one_line(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    let kept: String = collapsed.chars().take(limit.saturating_sub(3)).collect();
    let kept = kept.trim_end().to_owned();
    format!("{kept}…")
}

fn describe_blanket(crate_name: &str, imp: &BlanketImpl) -> String {
    let where_ = match (&imp.file, imp.line) {
        (Some(file), Some(line)) => format!(" — {file}:{line}"),
        (Some(file), None) => format!(" — {file}"),
        _ => String::new(),
    };
    format!(
        "{crate_name}: impl<{}> {} for {}{where_}",
        imp.self_param, imp.trait_name, imp.self_param
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(path: &str, attrs: &str) -> TraitDef {
        TraitDef {
            id: "1".to_owned(),
            path: path.to_owned(),
            name: path.rsplit("::").next().unwrap_or(path).to_owned(),
            attrs: attrs.to_owned(),
            file: Some("src/lib.rs".to_owned()),
            line: Some(7),
        }
    }

    #[test]
    fn a_diagnosed_trait_passes_whatever_the_rendering() {
        let allow = AllowList::default();
        for attrs in [
            r##"[{"other":"#[attr = OnUnimplemented {}]"}]"##,
            r##"["#[diagnostic::on_unimplemented(message = \"x\")]"]"##,
        ] {
            let verdict = verdict_for("moso-core", &def("c::T", attrs), &allow);
            assert_eq!(verdict.verdict, Verdict::Diagnosed, "{attrs}");
        }
    }

    #[test]
    fn an_exemption_passes_and_a_known_gap_does_not() {
        let allow = AllowList::parse(
            r#"
            [[exempt]]
            path = "c::Sealed"
            reason = "unnameable"

            [[known_gap]]
            path = "c::Wrapper"
            reason = "needs a message"
            "#,
        )
        .expect("valid allowlist");
        assert_eq!(
            verdict_for("c", &def("c::Sealed", ""), &allow).verdict,
            Verdict::Exempt
        );
        assert_eq!(
            verdict_for("c", &def("c::Wrapper", ""), &allow).verdict,
            Verdict::KnownGap
        );
        assert!(!Verdict::KnownGap.passes(), "a recorded gap is still a gap");
    }

    #[test]
    fn coverage_counts_exemptions_as_covered_and_gaps_as_not() {
        let entry = |path: &str, verdict| TraitVerdict {
            crate_name: "c".to_owned(),
            path: path.to_owned(),
            location: None,
            verdict,
        };
        let report = Report {
            traits: vec![
                entry("a", Verdict::Diagnosed),
                entry("b", Verdict::Exempt),
                entry("c", Verdict::KnownGap),
                entry("d", Verdict::Missing),
            ],
            blanket_impls: Vec::new(),
            format_versions: BTreeMap::new(),
            skipped: Vec::new(),
        };
        assert_eq!(report.coverage_pct(), 50.0);
        assert_eq!(report.with_verdict(Verdict::Missing).len(), 1);
    }

    #[test]
    fn a_blanket_impl_is_described_where_a_reader_can_find_it() {
        let imp = BlanketImpl {
            trait_name: "DynGuard".to_owned(),
            self_param: "G".to_owned(),
            has_do_not_recommend: false,
            file: Some("crates/moso-core/src/router.rs".to_owned()),
            line: Some(651),
        };
        let described = describe_blanket("moso-core", &imp);
        assert!(described.contains("impl<G> DynGuard for G"), "{described}");
        assert!(described.contains("router.rs:651"), "{described}");
    }

    #[test]
    fn a_blanket_exemption_is_keyed_by_crate_and_trait_and_nothing_wider() {
        let allow = AllowList::parse(
            r#"
            [[blanket_exempt]]
            path = "moso-core::HandlerFuture"
            reason = "rustc's nested-obligation message is better"
            "#,
        )
        .expect("valid allowlist");
        assert!(allow.is_blanket_exempt("moso-core", "HandlerFuture"));
        // A different trait in the same crate, and the same trait in a different
        // crate, are both still checked: an exemption is one impl, not a licence.
        assert!(!allow.is_blanket_exempt("moso-core", "DynGuard"));
        assert!(!allow.is_blanket_exempt("moso-orm", "HandlerFuture"));
        // The key is the cargo package name, not the crate's module path.
        assert!(!allow.is_blanket_exempt("moso_core", "HandlerFuture"));
    }

    #[test]
    fn a_blanket_exemption_does_not_touch_the_on_unimplemented_half() {
        let allow = AllowList::parse(
            r#"
            [[blanket_exempt]]
            path = "moso-core::HandlerFuture"
            reason = "rustc's nested-obligation message is better"
            "#,
        )
        .expect("valid allowlist");
        // Exempting the *impl* from `do_not_recommend` says nothing about the
        // *trait's* message, which is still required.
        assert_eq!(
            verdict_for("moso-core", &def("moso_core::HandlerFuture", ""), &allow).verdict,
            Verdict::Missing
        );
    }

    #[test]
    fn the_committed_allowlist_parses_and_every_entry_has_a_reason() {
        let root = crate::util::workspace_root().expect("a workspace");
        let text = std::fs::read_to_string(root.join("xtask/allow/diagnostics.toml"))
            .expect("the committed allowlist");
        let allow = AllowList::parse(&text).expect("every entry has a reason");
        assert!(
            !allow.exempt.is_empty()
                || !allow.known_gap.is_empty()
                || !allow.blanket_exempt.is_empty(),
            "an allowlist with no entries should be deleted rather than committed"
        );
    }
}
