//! `check-crates` — the structural properties every Moso crate must have.
//!
//! `docs/05-delivery/53-quality-gates.md` names this command as the enforcement
//! for **G5**, *"`#![forbid(unsafe_code)]` present in every crate"*, and until
//! now it did not exist. `.github/workflows/ci.yml` says so out loud: its xtask
//! job probes `--help` for the subcommand and emits `::warning::xtask has no
//! 'check-crates' subcommand; that gate is not enforced here`. A gate that
//! announces its own absence is honest, and it is still not a gate.
//!
//! # Why a tool rather than the greps CI already runs
//!
//! The `hygiene` job checks two of these rules with `grep`, deliberately, so
//! that the most important properties do not wait on tooling. That was the right
//! call and those greps should stay — but grep can only ask "does this string
//! appear". It cannot ask whether `#![forbid(unsafe_code)]` comes *before*
//! `#![deny(missing_docs)]`, whether a crate that declares `[lints] workspace =
//! true` is a crate that should, or whether `anyhow` is a direct dependency
//! rather than something four levels down that nobody chose. Those are the
//! questions that catch a new crate on the day it is added, which is the only
//! day the fix is cheap.
//!
//! # The seven rules
//!
//! | Rule | Property | Where it is written down |
//! | --- | --- | --- |
//! | 1 | `[workspace.lints]` forbids unsafe and denies missing docs | `AGENTS.md` |
//! | 2 | every crate opts in with `[lints] workspace = true` | `AGENTS.md` |
//! | 3 | every crate root restates both lints, in that order, then its `//!` | `AGENTS.md` |
//! | 4 | no `unsafe` anywhere under `crates/` or `examples/` | G5 |
//! | 5 | every `.rs` file under `crates/*/src` opens with a `//!` module doc | `AGENTS.md` |
//! | 6 | no banned direct dependency | ADR-0004, ADR-0011, `AGENTS.md` |
//! | 7 | every publishable crate carries its registry metadata | release readiness |
//!
//! Rules 2 and 3 look redundant and are not. The manifest table is what actually
//! turns the lints on; the crate-root attributes are what a reader sees. A crate
//! with the table and no attributes is correct and illegible; a crate with the
//! attributes and no table has *nine other* workspace lints silently switched
//! off, which is precisely the failure `AGENTS.md` warns about: *"a new crate
//! that forgets the manifest table silently loses all ten lints."*
//!
//! # `examples/` is deliberately different
//!
//! Rules 2, 3, 5 and 7 do not apply to `examples/`. `AGENTS.md`: *"`examples/`
//! deliberately does not inherit `[workspace.lints]` — sample apps must look
//! exactly like code a user would write."* An example carrying
//! `#![deny(missing_docs)]` would be teaching a house rule as if it were a
//! framework requirement. Rule 4 *does* apply to examples, because
//! "no unsafe in this repository" is a claim about the repository.
//!
//! # The escape hatch
//!
//! `xtask/allow/crates.toml` holds `[[exempt]]` entries, each keyed by
//! `<rule>:<subject>` and each requiring a reason. It is the same shape as
//! `xtask/allow/diagnostics.toml` and `xtask/allow/sealed.toml`, for the same
//! stated purpose: an exemption is a decision somebody made and signed, not a
//! silence.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::meta::{DepKind, Workspace};
use crate::util::{Error, Result, ui};

// ---------------------------------------------------------------------------
// What is banned, and why
// ---------------------------------------------------------------------------

/// Crates that must never be a **direct** dependency of a workspace member,
/// each with the sentence that explains the refusal.
///
/// Direct, not transitive, and the distinction is load-bearing:
/// `webauthn-rs-core` links OpenSSL, so a transitive scan would fail this
/// workspace for a decision nobody here took. What this rule protects is the
/// choice a contributor makes when they type `cargo add`.
///
/// ```
/// use xtask::crates::BANNED_DEPS;
///
/// let names: Vec<&str> = BANNED_DEPS.iter().map(|(name, _)| *name).collect();
/// assert!(names.contains(&"anyhow"));
/// assert!(names.contains(&"inventory"));
/// assert!(names.contains(&"ctor"));
/// // Every ban carries its reason: a refusal nobody can explain gets reverted.
/// assert!(BANNED_DEPS.iter().all(|(_, why)| !why.is_empty()));
/// ```
pub const BANNED_DEPS: &[(&str, &str)] = &[
    (
        "anyhow",
        "handlers return `moso::Result<T>` over one concrete `Error`, and a battery defines its \
         own `Error` in `src/error.rs` with `thiserror` (AGENTS.md)",
    ),
    (
        "inventory",
        "link-time registration: everything is registered by a statement you can read \
         (ADR-0004)",
    ),
    (
        "ctor",
        "link-time registration: everything is registered by a statement you can read \
         (ADR-0004)",
    ),
    (
        "async-trait",
        "RPITIT for generic traits, a hand-written `BoxFuture` for dyn-compatible ones \
         (AGENTS.md)",
    ),
    (
        "async-std",
        "Tokio only, and reopening that needs an ADR (ADR-0011)",
    ),
    (
        "smol",
        "Tokio only, and reopening that needs an ADR (ADR-0011)",
    ),
    (
        "openssl",
        "rustls, not OpenSSL: a C toolchain in the build is a portability and audit cost \
         (AGENTS.md)",
    ),
    (
        "native-tls",
        "rustls, not the platform TLS stack (AGENTS.md)",
    ),
];

/// The registry fields a publishable crate needs before a release is possible.
///
/// Checked as *present*, not as *literal*: every one of them is inherited from
/// `[workspace.package]` in this tree, and the point of the rule is that a new
/// crate has not forgotten to inherit.
///
/// ```
/// assert!(xtask::crates::REQUIRED_MANIFEST_KEYS.contains(&"description"));
/// ```
pub const REQUIRED_MANIFEST_KEYS: &[&str] = &[
    "description",
    "license",
    "repository",
    "categories",
    "keywords",
    "readme",
];

// ---------------------------------------------------------------------------
// The allowlist
// ---------------------------------------------------------------------------

/// One exemption.
///
/// ```
/// use xtask::crates::AllowEntry;
///
/// let entry: AllowEntry =
///     toml::from_str("subject = \"5:crates/moso-core/src/lib.rs\"\nreason = \"generated\"")?;
/// assert_eq!(entry.subject, "5:crates/moso-core/src/lib.rs");
/// # Ok::<(), toml::de::Error>(())
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AllowEntry {
    /// `<rule number>:<subject>`, where the subject is whatever the rule's
    /// finding names — a crate, a repository-relative path, or a crate/dep pair.
    pub subject: String,
    /// Why. Checked to be non-empty, because an unexplained exemption is
    /// indistinguishable from a mistake six months later.
    pub reason: String,
}

/// The parsed `xtask/allow/crates.toml`.
///
/// ```
/// use xtask::crates::AllowList;
///
/// let allow = AllowList::parse(
///     "[[exempt]]\nsubject = \"6:moso-auth/openssl\"\nreason = \"webauthn-rs-core links it\"",
/// )?;
/// assert!(allow.is_exempt("6:moso-auth/openssl"));
/// assert!(!allow.is_exempt("6:moso-core/openssl"));
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AllowList {
    /// Subjects this gate will not report, each with its reason.
    #[serde(default)]
    pub exempt: Vec<AllowEntry>,
}

impl AllowList {
    /// Parses the file, rejecting an entry with a blank reason.
    ///
    /// # Errors
    ///
    /// When the TOML does not parse, or an entry's `reason` is empty or blank.
    ///
    /// ```
    /// use xtask::crates::AllowList;
    ///
    /// let error = AllowList::parse("[[exempt]]\nsubject = \"1:x\"\nreason = \" \"")
    ///     .expect_err("a blank reason is not a reason");
    /// assert!(error.to_string().contains("reason"), "{error}");
    /// ```
    pub fn parse(toml_text: &str) -> Result<Self> {
        let allow: Self = toml::from_str(toml_text).map_err(|error| {
            Error::new(format!("xtask/allow/crates.toml does not parse: {error}"))
        })?;
        for entry in &allow.exempt {
            if entry.reason.trim().is_empty() {
                return Err(Error::new(format!(
                    "the exemption for `{}` has an empty `reason`; an exemption nobody explained \
                     is a mistake nobody can review",
                    entry.subject
                )));
            }
        }
        Ok(allow)
    }

    /// Reads the file, treating an absent one as empty.
    ///
    /// # Errors
    ///
    /// When the file exists and cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(Error::new(format!(
                "cannot read {}: {error}",
                path.display()
            ))),
        }
    }

    /// Whether this subject is exempt.
    #[must_use]
    pub fn is_exempt(&self, subject: &str) -> bool {
        self.exempt.iter().any(|entry| entry.subject == subject)
    }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// Whether a rule passed, failed, or could not run.
///
/// A rule that could not run is **not** a pass. `xtask`'s stated design rule is
/// that a gate which cannot see reports a skip rather than a green tick.
///
/// ```
/// assert_ne!(xtask::crates::Status::Skipped, xtask::crates::Status::Pass);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Status {
    /// The rule held everywhere it applied.
    Pass,
    /// At least one subject violated it.
    Fail,
    /// The rule could not be evaluated.
    Skipped,
}

/// One rule's outcome.
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct Rule {
    /// The rule number, as the module documentation numbers them.
    pub id: u8,
    /// One line saying what the rule asserts.
    pub title: &'static str,
    /// Whether it held.
    pub status: Status,
    /// How many subjects the rule examined, so a rule that checked nothing is
    /// visible rather than green.
    pub checked: usize,
    /// One line per violation, each naming the subject and the fix.
    pub findings: Vec<String>,
}

impl Rule {
    /// A rule that held over `checked` subjects.
    #[must_use]
    fn pass(id: u8, title: &'static str, checked: usize) -> Self {
        Self {
            id,
            title,
            status: Status::Pass,
            checked,
            findings: Vec::new(),
        }
    }

    /// Attaches findings, flipping the status when there are any.
    #[must_use]
    fn with(mut self, findings: Vec<String>) -> Self {
        if !findings.is_empty() {
            self.status = Status::Fail;
        }
        self.findings = findings;
        self
    }
}

/// The whole run, as `--json` writes it.
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct Report {
    /// Every rule, in order.
    pub rules: Vec<Rule>,
}

impl Report {
    /// Whether every rule passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.rules.iter().all(|rule| rule.status == Status::Pass)
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// How to run the gate.
///
/// Deliberately not `#[non_exhaustive]`, matching [`deps::Options`] and its
/// siblings: the binary in `main.rs` constructs one field by field, and a
/// non-exhaustive struct cannot be built that way from another crate.
///
/// [`deps::Options`]: crate::deps::Options
#[derive(Clone, Debug)]
pub struct Options {
    /// Where the exemptions live, relative to the workspace root.
    pub allow_file: PathBuf,
    /// Write the machine-readable report here.
    pub json: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            allow_file: PathBuf::from("xtask/allow/crates.toml"),
            json: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Source scanning
// ---------------------------------------------------------------------------

/// Every `.rs` file under `dir`, sorted, so a failure lists the same file first
/// on every machine.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // `target/` under a crate is build output, never source.
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// A line with its string literals and its `//`, `///` and `//!` comments
/// removed, so a scan for a keyword cannot be fooled by prose.
///
/// Deliberately not a parser, but it does understand the three forms that
/// actually appear in this tree and would otherwise produce false positives:
/// line comments, ordinary strings with escapes, and **raw strings**
/// (`r"…"`, `r#"…"#`, `br#"…"#`). The raw-string case is not hypothetical —
/// this very file asserts `uses_unsafe(r#"let message = "unsafe { }";"#)` is
/// false, and a stripper that treated `r#"` as an opening quote would see the
/// inner `"` as a *closing* one and report the assertion as a violation.
///
/// Two limits, recorded rather than implied: a literal that spans lines is only
/// stripped on the line where it opens, and block comments (`/* … */`) are not
/// understood. Neither appears around the keywords this gate looks for.
///
/// ```
/// use xtask::crates::strip_comments_and_strings;
///
/// assert_eq!(strip_comments_and_strings("let x = 1; // unsafe").trim(), "let x = 1;");
/// assert_eq!(strip_comments_and_strings("//! unsafe").trim(), "");
/// assert!(!strip_comments_and_strings(r#"let s = "unsafe";"#).contains("unsafe"));
/// assert!(!strip_comments_and_strings(r##"f(r#"unsafe { }"#);"##).contains("unsafe"));
/// assert!(strip_comments_and_strings("unsafe { }").contains("unsafe"));
/// ```
#[must_use]
pub fn strip_comments_and_strings(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut index = 0;
    let mut kept_from = 0;

    while index < bytes.len() {
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
            out.push_str(&line[kept_from..index]);
            return out;
        }
        if let Some((quote_at, hashes)) = raw_string_opener(bytes, index) {
            out.push_str(&line[kept_from..index]);
            index = skip_raw_string(bytes, quote_at + 1, hashes);
            kept_from = index;
            continue;
        }
        if bytes[index] == b'"' {
            out.push_str(&line[kept_from..index]);
            index = skip_string(bytes, index + 1);
            kept_from = index;
            continue;
        }
        index += 1;
    }

    out.push_str(&line[kept_from..]);
    out
}

/// If a raw string literal opens at `index`, the index of its `"` and the
/// number of `#` between the `r` and that quote.
///
/// The `r` must not be the tail of an identifier, or `for r"x"` would be read
/// correctly while `let power = r"x"` would not; `br"…"` is accepted the same
/// way.
fn raw_string_opener(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    if bytes[index] != b'r' {
        return None;
    }
    let prefix_start = if index > 0 && bytes[index - 1] == b'b' {
        index - 1
    } else {
        index
    };
    if prefix_start > 0 && is_ident_byte(bytes[prefix_start - 1]) {
        return None;
    }
    let mut at = index + 1;
    while bytes.get(at) == Some(&b'#') {
        at += 1;
    }
    (bytes.get(at) == Some(&b'"')).then_some((at, at - index - 1))
}

/// The index just past a raw string that opened at `from`, or the end of the
/// line when it does not close on this one.
fn skip_raw_string(bytes: &[u8], from: usize, hashes: usize) -> usize {
    let mut at = from;
    while at < bytes.len() {
        if bytes[at] == b'"' {
            let closed = (1..=hashes).all(|offset| bytes.get(at + offset) == Some(&b'#'));
            if closed {
                return at + 1 + hashes;
            }
        }
        at += 1;
    }
    bytes.len()
}

/// The index just past an ordinary string that opened at `from`, honouring
/// backslash escapes, or the end of the line when it does not close.
fn skip_string(bytes: &[u8], from: usize) -> usize {
    let mut at = from;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return at + 1,
            _ => at += 1,
        }
    }
    bytes.len()
}

/// Whether `text` uses `unsafe` as a keyword rather than as a word.
///
/// ```
/// use xtask::crates::uses_unsafe;
///
/// assert!(uses_unsafe("unsafe { *ptr }"));
/// assert!(uses_unsafe("pub unsafe fn go() {}"));
/// assert!(!uses_unsafe("#![forbid(unsafe_code)]"));
/// assert!(!uses_unsafe("// this would be unsafe"));
/// assert!(!uses_unsafe("let unsafely = 1;"));
/// ```
#[must_use]
pub fn uses_unsafe(text: &str) -> bool {
    let code = strip_comments_and_strings(text);
    let bytes = code.as_bytes();
    let mut at = 0;
    while let Some(found) = code[at..].find("unsafe") {
        let start = at + found;
        let end = start + "unsafe".len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        // `unsafe_code` inside `#![forbid(..)]` is the lint's name, not a use.
        let after = code[end..].trim_start();
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        let is_keyword_use = after.starts_with('{')
            || after.starts_with("fn ")
            || after.starts_with("impl ")
            || after.starts_with("trait ")
            || after.starts_with("extern ");
        if before_ok && after_ok && is_keyword_use {
            return true;
        }
        at = end;
    }
    false
}

/// Whether a byte can appear inside a Rust identifier.
fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// What one line of a file's preamble is.
///
/// The preamble is everything above the first item: blank lines, comments and
/// inner attributes, in any order. Classifying a line is only hard because an
/// inner attribute can span several lines — `moso-macros`'s `util/attrs.rs`
/// opens with a five-line `#![allow(dead_code, reason = "…")]` — and a scanner
/// that reads line 2 of one as if it were an item declares the module doc
/// missing. That was the first false positive this gate produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Preamble {
    /// Blank, or a `//`, `///` or `//!` comment.
    Trivia,
    /// A complete inner attribute, however many lines it took.
    Attribute,
    /// The first item. The preamble is over.
    Item,
}

/// Walks the preamble, calling `visit` once per complete element.
///
/// Stops at the first item. Bracket depth is counted on the comment- and
/// string-stripped line, so a `]` inside a `reason = "…"` cannot close an
/// attribute early.
fn walk_preamble(source: &str, mut visit: impl FnMut(Preamble, &str)) {
    let mut pending = String::new();
    let mut depth: i32 = 0;

    for line in source.lines() {
        let trimmed = line.trim();

        if depth == 0 {
            if trimmed.is_empty() || trimmed.starts_with("//") {
                visit(Preamble::Trivia, trimmed);
                continue;
            }
            if !trimmed.starts_with("#![") {
                visit(Preamble::Item, trimmed);
                return;
            }
            pending.clear();
        }

        pending.push_str(trimmed);
        let stripped = strip_comments_and_strings(trimmed);
        depth += i32::try_from(stripped.matches('[').count()).unwrap_or(0)
            - i32::try_from(stripped.matches(']').count()).unwrap_or(0);

        if depth <= 0 {
            depth = 0;
            visit(Preamble::Attribute, &pending);
            pending.clear();
        }
    }
}

/// The inner attributes at the top of a file, in order, one per entry, with
/// whitespace squeezed out so `#![ forbid( unsafe_code ) ]` compares equal to
/// the canonical spelling and a five-line attribute collapses to one string.
///
/// ```
/// use xtask::crates::inner_attributes;
///
/// let source = "#![forbid(unsafe_code)]\n#![deny(missing_docs)]\n\n//! Hello.\n\npub fn f() {}\n";
/// assert_eq!(
///     inner_attributes(source),
///     vec!["#![forbid(unsafe_code)]".to_owned(), "#![deny(missing_docs)]".to_owned()],
/// );
///
/// // A multi-line attribute is one entry, not four lines of confusion.
/// let wrapped = "#![allow(\n    dead_code,\n    reason = \"why\"\n)]\n//! Doc.\n";
/// assert_eq!(inner_attributes(wrapped), vec!["#![allow(dead_code,reason=\"why\")]".to_owned()]);
/// ```
#[must_use]
pub fn inner_attributes(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    walk_preamble(source, |kind, text| {
        if kind == Preamble::Attribute {
            found.push(text.chars().filter(|c| !c.is_whitespace()).collect());
        }
    });
    found
}

/// Whether the file opens with a `//!` module doc, ignoring the comments and
/// inner attributes that may precede it.
///
/// ```
/// use xtask::crates::has_module_doc;
///
/// assert!(has_module_doc("//! What this module is for.\n\npub fn f() {}\n"));
/// assert!(has_module_doc("#![allow(clippy::all)]\n//! Doc.\n"));
/// assert!(has_module_doc("#![allow(\n    dead_code,\n    reason = \"why\"\n)]\n//! Doc.\n"));
/// assert!(!has_module_doc("// not a module doc\npub fn f() {}\n"));
/// assert!(!has_module_doc("pub fn f() {}\n"));
/// ```
#[must_use]
pub fn has_module_doc(source: &str) -> bool {
    let mut found = false;
    walk_preamble(source, |kind, text| {
        if kind == Preamble::Trivia && text.starts_with("//!") {
            found = true;
        }
    });
    found
}

// ---------------------------------------------------------------------------
// The rules
// ---------------------------------------------------------------------------

/// Rule 1 — the workspace declares the two lints this gate exists to protect.
fn rule_1(root: &Path) -> Result<Rule> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|error| Error::new(format!("cannot read the workspace manifest: {error}")))?;
    let squeezed: String = manifest.chars().filter(|c| !c.is_whitespace()).collect();
    let mut findings = Vec::new();
    if !squeezed.contains("unsafe_code=\"forbid\"") {
        findings.push(
            "the root Cargo.toml's [workspace.lints.rust] does not set `unsafe_code = \"forbid\"`; \
             help: add it, so every crate with `[lints] workspace = true` inherits it"
                .to_owned(),
        );
    }
    if !squeezed.contains("missing_docs=\"deny\"") {
        findings.push(
            "the root Cargo.toml's [workspace.lints.rust] does not set `missing_docs = \"deny\"`; \
             help: add it, so every crate with `[lints] workspace = true` inherits it"
                .to_owned(),
        );
    }
    Ok(Rule::pass(1, "the workspace forbids unsafe and denies missing docs", 1).with(findings))
}

/// Rule 2 — every Moso crate opts in to the workspace lint table.
fn rule_2(workspace: &Workspace, allow: &AllowList) -> Result<Rule> {
    let mut findings = Vec::new();
    let mut checked = 0;
    for package in workspace.moso_crates() {
        let subject = format!("2:{}", package.name);
        if allow.is_exempt(&subject) {
            continue;
        }
        checked += 1;
        let manifest = std::fs::read_to_string(&package.manifest_path).map_err(|error| {
            Error::new(format!(
                "cannot read {}: {error}",
                package.manifest_path.display()
            ))
        })?;
        let squeezed: String = manifest.chars().filter(|c| !c.is_whitespace()).collect();
        if !squeezed.contains("[lints]workspace=true") {
            findings.push(format!(
                "{} has no `[lints]\\nworkspace = true` table, so it inherits none of the ten \
                 workspace lints; help: add those two lines to {}",
                package.name,
                package.manifest_path.display()
            ));
        }
    }
    Ok(Rule::pass(
        2,
        "every Moso crate opts in with `[lints] workspace = true`",
        checked,
    )
    .with(findings))
}

/// Rule 3 — every crate root restates both lints, in the documented order.
fn rule_3(root: &Path, workspace: &Workspace, allow: &AllowList) -> Result<Rule> {
    let mut findings = Vec::new();
    let mut checked = 0;
    for package in workspace.moso_crates() {
        let dir = package
            .manifest_path
            .parent()
            .ok_or_else(|| Error::new("a manifest with no directory"))?;
        for name in ["src/lib.rs", "src/main.rs"] {
            let path = dir.join(name);
            if !path.exists() {
                continue;
            }
            let relative = relative_to(root, &path);
            let subject = format!("3:{relative}");
            if allow.is_exempt(&subject) {
                continue;
            }
            checked += 1;
            let source = std::fs::read_to_string(&path)
                .map_err(|error| Error::new(format!("cannot read {relative}: {error}")))?;
            let attributes = inner_attributes(&source);
            let forbid = attributes
                .iter()
                .position(|a| a == "#![forbid(unsafe_code)]");
            let deny = attributes
                .iter()
                .position(|a| a == "#![deny(missing_docs)]");
            match (forbid, deny) {
                (None, _) => findings.push(format!(
                    "{relative} does not restate `#![forbid(unsafe_code)]`; help: make it the \
                     first line of the file"
                )),
                (_, None) => findings.push(format!(
                    "{relative} does not restate `#![deny(missing_docs)]`; help: put it directly \
                     under `#![forbid(unsafe_code)]`"
                )),
                (Some(f), Some(d)) if f > d => findings.push(format!(
                    "{relative} restates the two lints in the wrong order; help: \
                     `#![forbid(unsafe_code)]` comes first, then `#![deny(missing_docs)]`"
                )),
                _ => {}
            }
            if !has_module_doc(&source) {
                findings.push(format!(
                    "{relative} has no `//!` summary after its lint attributes; help: add a \
                     one-sentence description of what the crate is"
                ));
            }
        }
    }
    Ok(Rule::pass(
        3,
        "every crate root restates both lints, then its summary",
        checked,
    )
    .with(findings))
}

/// Rule 4 — no `unsafe` anywhere in the repository's own sources.
fn rule_4(root: &Path, allow: &AllowList) -> Rule {
    let mut findings = Vec::new();
    let mut checked = 0;
    for area in ["crates", "examples", "xtask"] {
        for path in rust_files(&root.join(area)) {
            let relative = relative_to(root, &path);
            if allow.is_exempt(&format!("4:{relative}")) {
                continue;
            }
            checked += 1;
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (index, line) in source.lines().enumerate() {
                if uses_unsafe(line) {
                    findings.push(format!(
                        "{relative}:{} uses `unsafe`; help: Moso forbids it — if this is genuinely \
                         unavoidable it needs an ADR, not an `#[allow]`",
                        index + 1
                    ));
                }
            }
        }
    }
    Rule::pass(4, "no `unsafe` in crates/, examples/ or xtask/", checked).with(findings)
}

/// Rule 5 — every source file under `crates/*/src` opens with a module doc.
fn rule_5(root: &Path, workspace: &Workspace, allow: &AllowList) -> Rule {
    let mut findings = Vec::new();
    let mut checked = 0;
    for package in workspace.moso_crates() {
        let Some(dir) = package.manifest_path.parent() else {
            continue;
        };
        for path in rust_files(&dir.join("src")) {
            let relative = relative_to(root, &path);
            if allow.is_exempt(&format!("5:{relative}")) {
                continue;
            }
            checked += 1;
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            if !has_module_doc(&source) {
                findings.push(format!(
                    "{relative} does not open with a `//!` module doc; help: say what the module \
                     is for and why it is shaped that way — a bare file is instantly foreign"
                ));
            }
        }
    }
    Rule::pass(
        5,
        "every file under crates/*/src opens with a `//!` module doc",
        checked,
    )
    .with(findings)
}

/// Rule 6 — no banned crate is a direct dependency.
fn rule_6(workspace: &Workspace, allow: &AllowList) -> Rule {
    let mut findings = Vec::new();
    let mut checked = 0;
    let banned: BTreeSet<&str> = BANNED_DEPS.iter().map(|(name, _)| *name).collect();
    for package in &workspace.packages {
        for dep in &package.deps {
            if !banned.contains(dep.name.as_str()) {
                continue;
            }
            checked += 1;
            let subject = format!("6:{}/{}", package.name, dep.name);
            if allow.is_exempt(&subject) {
                continue;
            }
            let why = BANNED_DEPS
                .iter()
                .find(|(name, _)| *name == dep.name)
                .map_or("", |(_, why)| *why);
            let table = match dep.kind {
                DepKind::Normal => "[dependencies]",
                DepKind::Development => "[dev-dependencies]",
                DepKind::Build => "[build-dependencies]",
            };
            findings.push(format!(
                "{} declares `{}` in {table}: {why}; help: remove it, or add an entry to \
                 xtask/allow/crates.toml with `subject = \"{subject}\"` and a reason",
                package.name, dep.name
            ));
        }
    }
    Rule::pass(6, "no banned crate is a direct dependency", checked).with(findings)
}

/// Rule 7 — every publishable crate carries the metadata a release needs.
fn rule_7(workspace: &Workspace, allow: &AllowList) -> Result<Rule> {
    let mut findings = Vec::new();
    let mut checked = 0;
    for package in workspace.moso_crates() {
        if !package.publishable {
            continue;
        }
        let subject = format!("7:{}", package.name);
        if allow.is_exempt(&subject) {
            continue;
        }
        checked += 1;
        let manifest = std::fs::read_to_string(&package.manifest_path).map_err(|error| {
            Error::new(format!(
                "cannot read {}: {error}",
                package.manifest_path.display()
            ))
        })?;
        let missing: Vec<&str> = REQUIRED_MANIFEST_KEYS
            .iter()
            .copied()
            .filter(|key| {
                !manifest
                    .lines()
                    .any(|line| line.trim_start().starts_with(key))
            })
            .collect();
        if !missing.is_empty() {
            findings.push(format!(
                "{} is publishable but its manifest declares neither a value nor a \
                 `.workspace = true` inheritance for: {}; help: add them to {}",
                package.name,
                missing.join(", "),
                package.manifest_path.display()
            ));
        }
    }
    Ok(Rule::pass(
        7,
        "every publishable crate carries its registry metadata",
        checked,
    )
    .with(findings))
}

/// A repository-relative path, for a finding a person has to act on.
fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// Runs every rule and prints the outcome.
///
/// # Errors
///
/// When the workspace metadata cannot be loaded, the allowlist does not parse,
/// or a manifest this gate must read is unreadable. Those are harness failures,
/// not gate failures, and the binary maps them to a different exit code.
///
/// ```no_run
/// let ok = xtask::crates::run(&xtask::crates::Options::default())?;
/// assert!(ok);
/// # Ok::<(), xtask::util::Error>(())
/// ```
pub fn run(options: &Options) -> Result<bool> {
    let root = crate::util::workspace_root()?;
    let workspace = Workspace::load()?;
    let allow = AllowList::load(&root.join(&options.allow_file))?;

    ui::headline("check-crates");

    let report = Report {
        rules: vec![
            rule_1(&root)?,
            rule_2(&workspace, &allow)?,
            rule_3(&root, &workspace, &allow)?,
            rule_4(&root, &allow),
            rule_5(&root, &workspace, &allow),
            rule_6(&workspace, &allow),
            rule_7(&workspace, &allow)?,
        ],
    };

    for rule in &report.rules {
        let line = format!(
            "rule {}: {} ({} checked)",
            rule.id, rule.title, rule.checked
        );
        match rule.status {
            Status::Pass => ui::ok(&line),
            Status::Fail => ui::fail(&line),
            Status::Skipped => ui::warn(&line),
        }
        for finding in &rule.findings {
            ui::note(finding);
        }
    }

    if !allow.exempt.is_empty() {
        ui::note(&format!(
            "{} exemption(s) in {}",
            allow.exempt.len(),
            options.allow_file.display()
        ));
    }

    if let Some(path) = &options.json {
        let text = serde_json::to_string_pretty(&report)?;
        let destination = root.join(path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::new(format!("cannot create {}: {error}", parent.display()))
            })?;
        }
        std::fs::write(&destination, text).map_err(|error| {
            Error::new(format!("cannot write {}: {error}", destination.display()))
        })?;
        ui::note(&format!("report written to {}", destination.display()));
    }

    Ok(report.passed())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── the source scanners ──────────────────────────────────────────────

    #[test]
    fn the_word_unsafe_in_a_comment_is_not_a_use() {
        assert!(!uses_unsafe("//! Never write unsafe code here."));
        assert!(!uses_unsafe("// unsafe { } would be wrong"));
        assert!(!uses_unsafe("/// `unsafe` is forbidden"));
    }

    #[test]
    fn the_word_unsafe_in_a_string_is_not_a_use() {
        assert!(!uses_unsafe(r#"let message = "unsafe { }";"#));
        assert!(!uses_unsafe(r#"assert!(source.contains("unsafe fn"));"#));
    }

    #[test]
    fn the_forbid_attribute_is_not_a_use() {
        assert!(!uses_unsafe("#![forbid(unsafe_code)]"));
        assert!(!uses_unsafe("#![cfg_attr(test, forbid(unsafe_code))]"));
    }

    #[test]
    fn an_identifier_that_merely_starts_with_unsafe_is_not_a_use() {
        assert!(!uses_unsafe("let unsafely = 1;"));
        assert!(!uses_unsafe("fn unsafe_looking() {}"));
    }

    #[test]
    fn every_real_form_of_unsafe_is_a_use() {
        assert!(uses_unsafe("unsafe { *pointer }"));
        assert!(uses_unsafe("pub unsafe fn go() {}"));
        assert!(uses_unsafe("unsafe impl Send for T {}"));
        assert!(uses_unsafe("unsafe trait Marker {}"));
        assert!(uses_unsafe("unsafe extern \"C\" { }"));
    }

    #[test]
    fn inner_attributes_stop_where_the_items_start() {
        let source = "#![forbid(unsafe_code)]\n#![deny(missing_docs)]\n//! Doc.\npub fn f() {}\n\
                      #![this_is_not_read]\n";
        assert_eq!(inner_attributes(source).len(), 2);
    }

    #[test]
    fn inner_attributes_ignore_the_spelling_of_whitespace() {
        assert_eq!(
            inner_attributes("#![ forbid( unsafe_code ) ]\n"),
            vec!["#![forbid(unsafe_code)]".to_owned()],
        );
    }

    #[test]
    fn a_module_doc_is_found_past_the_attributes() {
        assert!(has_module_doc("#![allow(clippy::pedantic)]\n\n//! Doc.\n"));
        assert!(!has_module_doc(
            "#![allow(clippy::pedantic)]\n\npub fn f() {}\n"
        ));
    }

    // ── the allowlist ────────────────────────────────────────────────────

    #[test]
    fn an_exemption_without_a_reason_is_refused() {
        let error = AllowList::parse("[[exempt]]\nsubject = \"4:a.rs\"\nreason = \"\"")
            .expect_err("an empty reason");
        assert!(error.to_string().contains("reason"), "{error}");
    }

    #[test]
    fn an_absent_allowlist_is_an_empty_one() {
        let allow = AllowList::load(Path::new("/nonexistent/crates.toml"))
            .expect("an absent file is not an error");
        assert!(allow.exempt.is_empty());
    }

    // ── the rules, against this very workspace ───────────────────────────

    #[test]
    fn every_ban_names_the_document_that_imposes_it() {
        for (name, why) in BANNED_DEPS {
            assert!(
                why.contains("ADR-") || why.contains("AGENTS.md"),
                "the ban on `{name}` cites no document: {why}"
            );
        }
    }

    #[test]
    fn a_failing_rule_reports_every_subject_rather_than_the_first() {
        let rule = Rule::pass(4, "t", 2).with(vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(rule.status, Status::Fail);
        assert_eq!(rule.findings.len(), 2);
    }

    #[test]
    fn a_report_with_one_failing_rule_has_not_passed() {
        let report = Report {
            rules: vec![
                Rule::pass(1, "t", 1),
                Rule::pass(2, "t", 1).with(vec!["x".to_owned()]),
            ],
        };
        assert!(!report.passed());
    }
}
