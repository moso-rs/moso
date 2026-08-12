//! The diagnostics regression suite.
//!
//! [`ui`] does two things, and they check different properties:
//!
//! 1. It compiles every program under `tests/ui/` and asserts the compiler said
//!    exactly what the neighbouring `.stderr` file says it must. This is the
//!    snapshot: a change that degrades a message shows up as a reviewable diff.
//! 2. It then reads those snapshots as *text* and enforces the normative rules
//!    from `docs/04-devex/41-diagnostics.md` — a `help:` line on every one, no
//!    line long enough to wrap in a terminal, and a median under 25 lines.
//!
//! The second half is what stops the corpus rotting. A snapshot can be
//! re-recorded in a second with `TRYBUILD=overwrite`; without a lint over the
//! recorded text, re-recording is indistinguishable from fixing.
//!
//! [`the_corpus_has_no_orphans`] is separate because it reads no snapshot
//! contents: it only checks that every `.rs` has a `.stderr` and vice versa.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Every program in the corpus must fail to compile, with the exact output
/// recorded beside it — and the recorded output must obey the style guide.
///
/// ```text
/// cargo test -p moso-ui-tests                     # check
/// TRYBUILD=overwrite cargo test -p moso-ui-tests  # re-record
/// ```
///
/// The two halves are one test on purpose. `trybuild` does its work in the
/// `Drop` impl of [`trybuild::TestCases`], so under `TRYBUILD=overwrite` the
/// snapshots do not exist until that value is dropped; a separate `#[test]`
/// reading them would race the run that writes them.
#[test]
fn ui() {
    {
        let t = trybuild::TestCases::new();
        for case in cases() {
            t.compile_fail(&case);
        }
    }
    stderr_files_follow_the_style_guide();
}

/// The normative style rules from `docs/04-devex/41-diagnostics.md`, applied to
/// the recorded output rather than to the code that produces it.
///
/// Rule 3 of the style guide — "always give a fix, as code" — is the one that
/// decays silently, because a diagnostic without a `help:` line still *looks*
/// fine in isolation. Checking it here means a new case cannot be added without
/// one, and an existing case cannot lose one.
fn stderr_files_follow_the_style_guide() {
    /// Wide terminals are 120 columns; the style guide's 80-char rule is about
    /// *types*, which this lint cannot identify, so the line budget is the
    /// weaker of the two and is enforced on every line.
    const MAX_LINE: usize = 120;

    let mut problems = Vec::new();
    let mut checked = 0usize;

    for case in cases() {
        let snapshot = case.with_extension("stderr");
        let relative = display(&snapshot);
        let Ok(text) = fs::read_to_string(&snapshot) else {
            problems.push(format!(
                "{relative}: missing — record it with `TRYBUILD=overwrite cargo test -p \
                 moso-ui-tests`"
            ));
            continue;
        };
        checked += 1;

        if !text.lines().any(|line| line.contains("help:")) {
            problems.push(format!(
                "{relative}: no `help:` line — every Moso diagnostic must end in a fix the user \
                 can paste (docs/04-devex/41-diagnostics.md, style guide rule 3)"
            ));
        }

        for (number, line) in text.lines().enumerate() {
            let width = measured_width(line);
            if width > MAX_LINE {
                problems.push(format!(
                    "{relative}:{}: {width} characters, over the {MAX_LINE} budget — name the \
                     concept instead of printing the type",
                    number + 1
                ));
            }
        }
    }

    assert!(checked > 0, "the corpus is empty");
    problems.extend(median_over_budget());
    assert!(
        problems.is_empty(),
        "{} snapshot(s) violate the style guide:\n  {}",
        problems.len(),
        problems.join("\n  ")
    );
}

/// Every case must be reachable from the harness, and every recorded `.stderr`
/// must belong to a case.
///
/// A `.rs` file that no test compiles is a diagnostic nobody is checking, and a
/// stale `.stderr` is a snapshot of a message that no longer exists. Both are
/// invisible without this test, because both leave the suite green.
#[test]
fn the_corpus_has_no_orphans() {
    let mut expected = BTreeSet::new();
    for case in cases() {
        expected.insert(case.with_extension("stderr"));
    }

    let mut orphans = Vec::new();
    for entry in walk(&ui_root()) {
        match entry.extension().and_then(|e| e.to_str()) {
            Some("rs" | "stderr") => {}
            _ => orphans.push(format!(
                "{}: neither a case nor a snapshot",
                display(&entry)
            )),
        }
        if entry.extension().and_then(|e| e.to_str()) == Some("stderr")
            && !expected.contains(&entry)
        {
            orphans.push(format!(
                "{}: recorded output with no `.rs` beside it — delete it",
                display(&entry)
            ));
        }
    }

    assert!(orphans.is_empty(), "{}", orphans.join("\n"));
}

/// The corpus-wide budget from the "Measuring success" table in
/// `docs/04-devex/41-diagnostics.md`: a median snapshot under 25 lines.
///
/// It is a corpus metric rather than a per-file one on purpose. A single
/// diagnostic is occasionally allowed to be long — rustc appends its own
/// "the following other types implement" block to any trait error and nothing
/// can suppress it for a trait with concrete impls — but if the *typical* error
/// is 25 lines the reader has stopped reading them.
fn median_over_budget() -> Option<String> {
    /// The target from the table.
    const MEDIAN_BUDGET: usize = 25;

    let mut lengths: Vec<usize> = cases()
        .iter()
        .filter_map(|case| fs::read_to_string(case.with_extension("stderr")).ok())
        .map(|text| text.lines().count())
        .collect();
    lengths.sort_unstable();

    let median = *lengths.get(lengths.len() / 2)?;
    (median > MEDIAN_BUDGET).then(|| {
        format!(
            "the median snapshot is {median} lines, over the {MEDIAN_BUDGET}-line budget \
             (docs/04-devex/41-diagnostics.md, \"Measuring success\")"
        )
    })
}

/// The width the line budget is measured against.
///
/// rustc names an unnameable type — an async block, a closure — after the file
/// and span it was written at: `{async block@$DIR/tests/ui/handler/x.rs:14:10:
/// 14:14}`. Inside this corpus those paths are four segments deep, which is
/// deeper than the `src/routes/users.rs` a real application would show, so a
/// line can bust the budget for a reason the framework does not control and a
/// reader would never see. Collapsing the span to a placeholder measures the
/// part Moso is responsible for.
fn measured_width(line: &str) -> usize {
    let mut width = 0usize;
    let mut rest = line;
    while let Some(at) = rest.find("@$DIR/") {
        // Up to and including the `@`, then a stand-in for the location.
        width += rest[..at].chars().count() + "@…".chars().count();
        let after = &rest[at + "@$DIR/".len()..];
        match after.find('}') {
            Some(end) => rest = &after[end..],
            None => return width + after.chars().count(),
        }
    }
    width + rest.chars().count()
}

/// `tests/ui`, absolute.
fn ui_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ui")
}

/// Every `.rs` file in the corpus, in a stable order.
///
/// Sorted so that a failing run reports the same case first on every machine,
/// which matters when the output is being read in CI logs.
fn cases() -> Vec<PathBuf> {
    let mut cases: Vec<PathBuf> = walk(&ui_root())
        .into_iter()
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("rs"))
        .collect();
    cases.sort();
    cases
}

/// Every file under `root`, recursively.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| name != ".DS_Store") {
                found.push(path);
            }
        }
    }
    found
}

/// A path as it appears in a failure message: relative to the package root, so
/// it can be pasted into an editor.
fn display(path: &Path) -> String {
    path.strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap_or(path)
        .display()
        .to_string()
}
