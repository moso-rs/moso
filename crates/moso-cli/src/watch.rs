//! Noticing that a source file changed.
//!
//! # Why polling and not `notify`
//!
//! The obvious implementation subscribes to the operating system: `inotify` on
//! Linux, `FSEvents` on macOS, `ReadDirectoryChangesW` on Windows, all of which
//! the `notify` crate wraps behind one API. It is the better engineering in the
//! abstract and it is not what this module does, for one reason that is specific
//! to this workspace: `notify` and its platform backends are **eight crates that
//! are not otherwise in the dependency graph**, and
//! `xtask check-deps` rule 6 is already over its budget. A dev-loop convenience
//! is not what should push it further over.
//!
//! What polling costs is bounded here in a way it is not in general. The watcher
//! walks a *declared* set of roots — `src/`, `Cargo.toml`, `config/` and the
//! like — never the whole project, and never `target/`, which is the directory
//! that makes naive polling untenable. A project with 500 source files costs one
//! `stat` each, a few hundred microseconds, every 300 ms. That is far below the
//! noise floor of the `cargo build` it exists to trigger.
//!
//! The trade worth naming: an OS-level watcher reacts in ~1 ms and this reacts
//! in up to one poll interval. Against a rebuild measured in seconds, 300 ms of
//! latency is not the term that matters.
//!
//! # What counts as a change
//!
//! A file's *modification time or length*. Content hashing would be stricter —
//! it would ignore a save that rewrote identical bytes — but it means reading
//! every watched file on every poll, which is the cost polling exists to avoid.
//! Editors that write-then-rename produce a new mtime either way.
//!
//! Creations and deletions are changes too: the fingerprint is a map, and a key
//! appearing or disappearing compares unequal just as a changed value does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Directory names never descended into.
///
/// `target` is the one that matters — it holds tens of thousands of files and
/// every build rewrites it, so a watcher that walked it would both cost real
/// time and retrigger itself forever. The rest are the same argument at smaller
/// scale.
pub const IGNORED_DIRECTORIES: &[&str] = &[
    "target",
    ".git",
    ".jj",
    "node_modules",
    ".venv",
    "__pycache__",
    ".moso",
    "dist",
];

/// The directories and files a Moso project is watched at, when they exist.
///
/// Deliberately a short list of things that affect the *built binary or its
/// startup*: source, the manifest, the lockfile, configuration. Watching the
/// whole project directory would pick up a `README.md` edit and rebuild for it.
pub const DEFAULT_ROOTS: &[&str] = &[
    "src",
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "config",
    "templates",
    "migrations",
    ".env",
];

/// Whether a directory entry's name should be skipped entirely.
///
/// Covers [`IGNORED_DIRECTORIES`] and the temporary files editors leave beside
/// the real one: without this, one `:w` in vim produces a change event for
/// `foo.rs`, another for `foo.rs~`, and a third when the backup is removed.
#[must_use]
pub fn is_ignored(name: &str) -> bool {
    if IGNORED_DIRECTORIES.contains(&name) {
        return true;
    }
    // Vim backups and swap files, Emacs lock files and auto-saves, and the
    // partial files editors write before renaming into place.
    name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.ends_with(".tmp")
        || name.starts_with(".#")
        || (name.starts_with('#') && name.ends_with('#'))
        || name.ends_with(".rs.bk")
}

/// What one watched file looked like at one moment.
///
/// Length is carried alongside the modification time because a filesystem whose
/// mtime granularity is coarse — some network filesystems round to the second —
/// would otherwise miss a fast edit-save-edit sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    /// Last modification, when the platform reports one.
    modified: Option<SystemTime>,
    /// Size in bytes.
    length: u64,
}

/// A snapshot of every watched file.
///
/// Compared wholesale: any key added, removed or altered is a change. Ordered
/// so that the first differing path is deterministic, which is what makes the
/// "changed: src/routes.rs" line reproducible rather than dependent on hash
/// iteration order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Every watched file, by path.
    files: BTreeMap<PathBuf, Stamp>,
}

impl Snapshot {
    /// Whether nothing at all is being watched.
    ///
    /// True means every declared root was missing, which is worth reporting: it
    /// is what a `--watch` pointed at a typo looks like.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The paths that differ between two snapshots, in path order.
    ///
    /// Includes files added since `previous` and files removed since it, not
    /// only files whose contents moved.
    #[must_use]
    pub fn changes_since(&self, previous: &Self) -> Vec<PathBuf> {
        let mut changed = Vec::new();
        for (path, stamp) in &self.files {
            if previous.files.get(path) != Some(stamp) {
                changed.push(path.clone());
            }
        }
        for path in previous.files.keys() {
            if !self.files.contains_key(path) {
                changed.push(path.clone());
            }
        }
        changed.sort();
        changed.dedup();
        changed
    }
}

/// The set of paths a `moso dev` session watches.
#[derive(Debug, Clone)]
pub struct Watcher {
    /// Absolute roots to walk. A root may be a file or a directory.
    roots: Vec<PathBuf>,
}

impl Watcher {
    /// Watch `roots`, resolved relative to `base`, keeping the ones that exist.
    ///
    /// A root that does not exist is dropped rather than being an error: the
    /// default list names `config/` and `migrations/`, and most projects have
    /// neither. A caller that cares whether anything was found checks
    /// [`Snapshot::is_empty`].
    #[must_use]
    pub fn new(base: &Path, roots: &[PathBuf]) -> Self {
        let roots = roots
            .iter()
            .map(|root| {
                if root.is_absolute() {
                    root.clone()
                } else {
                    base.join(root)
                }
            })
            .filter(|root| root.exists())
            .collect();
        Self { roots }
    }

    /// The default watcher for a project rooted at `base`.
    #[must_use]
    pub fn for_project(base: &Path) -> Self {
        let roots: Vec<PathBuf> = DEFAULT_ROOTS.iter().map(PathBuf::from).collect();
        Self::new(base, &roots)
    }

    /// The roots that survived the existence check.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Stat every watched file.
    ///
    /// Errors are swallowed on purpose. A file that vanished between the
    /// directory listing and the `stat` is a file that changed, and it will be
    /// absent from this snapshot and present in the last one — which is exactly
    /// the comparison that reports it. Turning the race into an error would make
    /// `moso dev` fall over during a `git checkout`.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let mut files = BTreeMap::new();
        for root in &self.roots {
            collect(root, &mut files);
        }
        Snapshot { files }
    }
}

/// Walk one root, adding every non-ignored file to `files`.
///
/// Iterative rather than recursive: a symlink cycle in a source tree would
/// overflow the stack, and a `while let` with an explicit worklist cannot.
/// Symlinked *directories* are not descended into for the same reason — a link
/// pointing at an ancestor is otherwise an infinite walk.
fn collect(root: &Path, files: &mut BTreeMap<PathBuf, Stamp>) {
    let mut queue = vec![root.to_path_buf()];
    while let Some(path) = queue.pop() {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };

        if metadata.is_symlink() {
            // Follow a symlinked *file* (a linked `.env` is common) but never a
            // symlinked directory.
            let Ok(target) = std::fs::metadata(&path) else {
                continue;
            };
            if target.is_file() {
                files.insert(path, stamp(&target));
            }
            continue;
        }

        if metadata.is_file() {
            files.insert(path, stamp(&metadata));
            continue;
        }

        if !metadata.is_dir() {
            continue;
        }

        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_ignored(&name) {
                continue;
            }
            queue.push(entry.path());
        }
    }
}

/// Reduce metadata to the two fields the comparison uses.
fn stamp(metadata: &std::fs::Metadata) -> Stamp {
    Stamp {
        modified: metadata.modified().ok(),
        length: metadata.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway directory under the target directory, so that a failed
    /// test leaves its evidence somewhere `cargo clean` removes.
    fn scratch(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("moso-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch directory");
        base
    }

    #[test]
    fn target_and_editor_droppings_are_ignored() {
        assert!(is_ignored("target"));
        assert!(is_ignored(".git"));
        assert!(is_ignored("node_modules"));
        assert!(is_ignored("routes.rs~"));
        assert!(is_ignored(".#routes.rs"));
        assert!(is_ignored("#routes.rs#"));
        assert!(is_ignored(".routes.rs.swp"));
        assert!(!is_ignored("routes.rs"));
        assert!(!is_ignored("src"));
        // `targets` is not `target`: the check is equality, not a prefix.
        assert!(!is_ignored("targets"));
    }

    #[test]
    fn a_missing_root_is_dropped_rather_than_fatal() {
        let base = scratch("missing-root");
        std::fs::create_dir(base.join("src")).expect("src");
        let watcher = Watcher::new(
            &base,
            &[PathBuf::from("src"), PathBuf::from("does-not-exist")],
        );
        assert_eq!(watcher.roots().len(), 1);
    }

    #[test]
    fn an_edited_file_is_reported_and_an_untouched_one_is_not() {
        let base = scratch("edit");
        std::fs::create_dir(base.join("src")).expect("src");
        std::fs::write(base.join("src/lib.rs"), "// one").expect("write");
        std::fs::write(base.join("src/other.rs"), "// other").expect("write");

        let watcher = Watcher::for_project(&base);
        let before = watcher.snapshot();
        assert_eq!(before.files.len(), 2);

        // Length differs, so this is detected even where mtime granularity is
        // coarse enough to round two writes into the same instant.
        std::fs::write(base.join("src/lib.rs"), "// one, edited").expect("rewrite");

        let after = watcher.snapshot();
        let changes = after.changes_since(&before);
        assert_eq!(changes, vec![base.join("src/lib.rs")]);
    }

    #[test]
    fn a_new_file_and_a_deleted_file_both_count_as_changes() {
        let base = scratch("add-remove");
        std::fs::create_dir(base.join("src")).expect("src");
        std::fs::write(base.join("src/lib.rs"), "// one").expect("write");

        let watcher = Watcher::for_project(&base);
        let before = watcher.snapshot();

        std::fs::write(base.join("src/new.rs"), "// new").expect("write");
        let added = watcher.snapshot();
        assert_eq!(added.changes_since(&before), vec![base.join("src/new.rs")]);

        std::fs::remove_file(base.join("src/new.rs")).expect("remove");
        let removed = watcher.snapshot();
        assert_eq!(removed.changes_since(&added), vec![base.join("src/new.rs")]);
    }

    #[test]
    fn the_target_directory_is_not_walked() {
        let base = scratch("target");
        std::fs::create_dir_all(base.join("src")).expect("src");
        std::fs::create_dir_all(base.join("target/debug")).expect("target");
        std::fs::write(base.join("src/lib.rs"), "// one").expect("write");
        std::fs::write(base.join("target/debug/huge"), "artefact").expect("write");

        // `target` is not among the default roots, but this proves the walk
        // would skip it even if a `--watch .` put it in reach.
        let watcher = Watcher::new(&base, &[PathBuf::from(".")]);
        let snapshot = watcher.snapshot();

        // Compare against the artefact's own path rather than searching for the
        // substring "target": the scratch directory is itself named after the
        // test, so a substring check matches the temporary root and fails for
        // the wrong reason.
        let artefact = base.join("target/debug/huge");
        assert!(
            !snapshot.files.keys().any(|path| path.ends_with("huge")),
            "{} should not have been walked, saw {:?}",
            artefact.display(),
            snapshot.files.keys().collect::<Vec<_>>()
        );
        assert_eq!(snapshot.files.len(), 1, "only src/lib.rs is watchable here");
    }

    #[test]
    fn an_unchanged_tree_reports_nothing() {
        let base = scratch("stable");
        std::fs::create_dir(base.join("src")).expect("src");
        std::fs::write(base.join("src/lib.rs"), "// one").expect("write");

        let watcher = Watcher::for_project(&base);
        let first = watcher.snapshot();
        let second = watcher.snapshot();
        assert!(second.changes_since(&first).is_empty());
        assert_eq!(first, second);
    }

    #[test]
    fn a_watcher_with_no_surviving_roots_snapshots_empty() {
        let base = scratch("empty");
        let watcher = Watcher::for_project(&base);
        assert!(watcher.snapshot().is_empty());
    }
}
