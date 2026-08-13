//! `moso generate` — scaffolding into a project that already exists.
//!
//! # Why this writes ordinary code and then stops
//!
//! Everything here produces a plain `.rs` file the user owns from the moment it
//! lands. There is no registry the generator writes into, no marker comment it
//! will come back and rewrite, and no second invocation that "updates" what it
//! produced. A generator that keeps ownership of its output is a generator whose
//! output you cannot edit, and the whole value of scaffolding is that the first
//! draft is *yours*.
//!
//! The one exception is module registration, and it is deliberately the smallest
//! edit that could work: one `pub mod` line, and for an endpoint one `.mount(..)`
//! call. Both are found by matching text that `moso new` wrote, and when the
//! match fails — because the project has been restructured, which is expected —
//! the command says exactly which line to add by hand rather than guessing.
//!
//! # What it refuses to do
//!
//! Overwrite. A generator that clobbers is a generator nobody runs twice, so an
//! existing target is an error naming `--force`.

use std::path::{Path, PathBuf};

use crate::cli::{GenerateArgs, GenerateKind};
use crate::exit::{CliError, Outcome, io as io_error};
use crate::naming::Names;
use crate::project::Project;
use crate::ui::{Level, Ui};

/// The body of each generated file, chosen by kind.
const ENDPOINT: &str = include_str!("../../templates/generate/endpoint.rs.tpl");
/// See [`ENDPOINT`].
const SCHEMA: &str = include_str!("../../templates/generate/schema.rs.tpl");
/// See [`ENDPOINT`].
const ERROR: &str = include_str!("../../templates/generate/error.rs.tpl");
/// See [`ENDPOINT`].
const MIDDLEWARE: &str = include_str!("../../templates/generate/middleware.rs.tpl");
/// See [`ENDPOINT`].
const TEST: &str = include_str!("../../templates/generate/test.rs.tpl");

/// Scaffold one resource into the project.
///
/// # Errors
/// [`Fault::Environment`](crate::exit::Fault::Environment) when the project
/// cannot be found or a file cannot be written, and
/// [`Fault::User`](crate::exit::Fault::User) when the target already exists and
/// `--force` was not given.
pub fn run(ui: &Ui, args: &GenerateArgs) -> Outcome<()> {
    // The one kind that restructures the project instead of writing a file into
    // it, and therefore the one kind with nothing to name. It finds the project
    // itself, because it is also the only command that has to recognise a
    // project it has already split — which is a workspace root and no longer a
    // package, and so is invisible to ordinary discovery.
    if args.kind == GenerateKind::Workspace {
        return super::workspace::run(ui, args);
    }

    let project = Project::discover(args.manifest_path.as_deref())?;
    project.require_moso()?;

    let name = args.name.as_deref().ok_or_else(|| {
        CliError::usage(format!(
            "`moso generate {}` needs a name",
            kind_name(args.kind)
        ))
        .with_help(format!("moso generate {} post", kind_name(args.kind)))
    })?;

    let names = Names::new(name, args.singular.as_deref());
    let lib_name = project.name.replace('-', "_");
    let plan = plan(args.kind, &names, &lib_name);

    let target = project.root.join(&plan.path);
    if target.exists() && !args.force {
        return Err(
            CliError::user(format!("`{}` already exists", plan.path.display()))
                .with_help("pass --force to overwrite it, or choose another name"),
        );
    }

    if args.dry_run {
        ui.status(Level::Ok, "would write", &plan.path.display().to_string());
        for edit in &plan.edits {
            ui.status(
                Level::Ok,
                "would edit",
                &format!("{} — add `{}`", edit.file, edit.line),
            );
        }
        if ui.is_json() {
            ui.emit_json(&plan.to_json(&project));
        }
        return Ok(());
    }

    if let Some(parent) = target.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| io_error("could not create", parent, &error))?;
    }
    std::fs::write(&target, &plan.contents)
        .map_err(|error| io_error("could not write", &target, &error))?;

    ui.status(Level::Ok, "created", &plan.path.display().to_string());

    // Registration is best-effort by design: a project that has outgrown the
    // generated layout still gets its file, and is told what to add.
    let mut manual = Vec::new();
    for edit in &plan.edits {
        match apply(&project.root, edit) {
            Ok(true) => ui.status(
                Level::Ok,
                "registered",
                &format!("{} — {}", edit.file, edit.line),
            ),
            Ok(false) => manual.push(edit),
            Err(error) => return Err(error),
        }
    }

    if !manual.is_empty() {
        ui.blank();
        ui.warn("could not register the module automatically");
        for edit in manual {
            ui.line(&format!("  add to {}:  {}", edit.file, edit.line));
        }
    }

    if ui.is_json() {
        ui.emit_json(&plan.to_json(&project));
    }

    Ok(())
}

/// How a kind is spelled on the command line.
///
/// `clap` knows this mapping and will not hand it over, so it is written once
/// here rather than at each of the two places a diagnostic needs it.
const fn kind_name(kind: GenerateKind) -> &'static str {
    match kind {
        GenerateKind::Endpoint => "endpoint",
        GenerateKind::Schema => "schema",
        GenerateKind::Error => "error",
        GenerateKind::Middleware => "middleware",
        GenerateKind::Test => "test",
        GenerateKind::Workspace => "workspace",
    }
}

/// One line to insert into an existing file, and where it goes.
#[derive(Debug, Clone)]
struct Edit {
    /// The file to edit, relative to the project root.
    file: &'static str,
    /// The exact line to insert, already indented.
    line: String,
    /// The text to insert after. The last occurrence wins, so that a repeated
    /// anchor — `pub mod`, of which there are several — appends to the group.
    anchor: Anchor,
}

/// How an [`Edit`] finds its insertion point.
#[derive(Debug, Clone)]
enum Anchor {
    /// Insert a whole new line after the last line beginning with this prefix.
    ///
    /// For `pub mod` declarations, which are one per line by construction.
    NewLineAfterLineStartingWith(&'static str),
    /// Insert the text immediately after the last occurrence of this needle,
    /// on the same line.
    ///
    /// For builder chains. `moso new` writes
    /// `App::new(config).mount(routes::router()).build()` on a single line, so
    /// a line-based insert would put the new `.mount` *after* `.build()` and
    /// chain it onto a `Result<App>`. Splicing into the line is the only
    /// placement that is correct regardless of how the user has since
    /// reformatted it.
    InlineAfter(&'static str),
}

/// Everything one invocation will do.
#[derive(Debug, Clone)]
struct Plan {
    /// Where the new file goes, relative to the project root.
    path: PathBuf,
    /// What it contains.
    contents: String,
    /// The registrations that follow it.
    edits: Vec<Edit>,
}

impl Plan {
    /// The `--json` rendering.
    fn to_json(&self, project: &Project) -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "created": self.path,
            "root": project.root,
            "edits": self.edits.iter().map(|edit| serde_json::json!({
                "file": edit.file,
                "line": edit.line,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Decide what to write and what to register, for one kind.
fn plan(kind: GenerateKind, names: &Names, lib_name: &str) -> Plan {
    let render = |source: &str| {
        source
            .replace("@@MODULE@@", &names.module)
            .replace("@@SINGULAR@@", &names.singular)
            .replace("@@TYPE_PLURAL@@", &names.type_plural)
            .replace("@@TYPE@@", &names.type_name)
            .replace("@@PATH@@", &names.path)
            .replace("@@RAW_SCREAMING@@", &names.raw_screaming)
            .replace("@@RAW_KEBAB@@", &names.raw_kebab)
            .replace("@@RAW_TYPE@@", &names.raw_type)
            .replace("@@RAW@@", &names.raw)
            .replace("@@LIB_NAME@@", lib_name)
    };

    // `mod` declarations go after the last existing one in `lib.rs`, which is
    // where `moso new` put `pub mod dump;` and `pub mod routes;`.
    let declare = |module: &str| Edit {
        file: "src/lib.rs",
        line: format!("pub mod {module};"),
        anchor: Anchor::NewLineAfterLineStartingWith("pub mod "),
    };

    match kind {
        GenerateKind::Endpoint => Plan {
            path: PathBuf::from(format!("src/{}.rs", names.module)),
            contents: render(ENDPOINT),
            edits: vec![
                declare(&names.module),
                // Mounting is a second edit rather than part of the first
                // because a router that is declared but not mounted is a
                // module that compiles and serves nothing — the confusing
                // half-state this avoids.
                Edit {
                    file: "src/lib.rs",
                    line: format!(".mount({}::router())", names.module),
                    anchor: Anchor::InlineAfter(".mount(routes::router())"),
                },
                // And the store the handlers `Inject`. Without this the project
                // compiles and then refuses to boot, because `App::build()`
                // proves the provider graph — a good error, but not one the
                // generator should be handing anybody.
                Edit {
                    file: "src/lib.rs",
                    line: format!(
                        ".provide({}::{}Store::default())",
                        names.module, names.type_name
                    ),
                    anchor: Anchor::InlineAfter("App::new(config)"),
                },
            ],
        },
        GenerateKind::Schema => Plan {
            path: PathBuf::from(format!("src/{}.rs", names.module)),
            contents: render(SCHEMA),
            edits: vec![declare(&names.module)],
        },
        GenerateKind::Error => Plan {
            path: PathBuf::from(format!("src/{}_error.rs", names.raw)),
            contents: render(ERROR),
            edits: vec![declare(&format!("{}_error", names.raw))],
        },
        GenerateKind::Middleware => Plan {
            path: PathBuf::from(format!("src/{}.rs", names.raw)),
            contents: render(MIDDLEWARE),
            edits: vec![declare(&names.raw)],
        },
        // A file under `tests/` is its own crate; nothing declares it.
        GenerateKind::Test => Plan {
            path: PathBuf::from(format!("tests/{}.rs", names.module)),
            contents: render(TEST),
            edits: Vec::new(),
        },
        GenerateKind::Workspace => {
            // Unreachable: `run` returns above for this kind, because a
            // workspace split writes no single file and registers no module, so
            // it has no path, no contents and no edit to describe. Named rather
            // than caught by a wildcard, so that a sixth kind is a compile error
            // here instead of silently generating a `test`.
            unreachable!("`moso generate workspace` is dispatched before a plan is made")
        }
    }
}

/// Apply one [`Edit`], reporting whether the anchor was found.
///
/// An edit whose line is already present is a success with nothing done, so
/// re-running the generator after `--force` does not produce a duplicate
/// `pub mod`.
fn apply(root: &Path, edit: &Edit) -> Outcome<bool> {
    let path = root.join(edit.file);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };

    let Some(rewritten) = splice(&text, edit) else {
        return Ok(false);
    };

    // `splice` returns the text unchanged when the edit is already present, so
    // re-running the generator does not duplicate a `pub mod` or a `.mount`.
    if rewritten != text {
        std::fs::write(&path, rewritten)
            .map_err(|error| io_error("could not update", &path, &error))?;
    }
    Ok(true)
}

/// Produce the edited text, or `None` when the anchor is absent.
///
/// Pure, so the placement rules are testable without a filesystem.
fn splice(text: &str, edit: &Edit) -> Option<String> {
    match edit.anchor {
        Anchor::NewLineAfterLineStartingWith(prefix) => {
            if text.lines().any(|line| line.trim() == edit.line.trim()) {
                return Some(text.to_owned());
            }
            let index = text
                .lines()
                .enumerate()
                .filter(|(_, line)| line.trim_start().starts_with(prefix))
                .map(|(index, _)| index)
                // `last`, not `next_back`: `Enumerate` is only double-ended
                // over an `ExactSizeIterator`, and `Lines` is not one.
                .last()?;

            let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
            let indent: String = lines[index]
                .chars()
                .take_while(|character| character.is_whitespace())
                .collect();
            lines.insert(index + 1, format!("{indent}{}", edit.line));

            let mut rewritten = lines.join("\n");
            if text.ends_with('\n') {
                rewritten.push('\n');
            }
            Some(rewritten)
        }
        Anchor::InlineAfter(needle) => {
            if text.contains(edit.line.trim()) {
                return Some(text.to_owned());
            }
            let at = text.rfind(needle)? + needle.len();
            let mut rewritten = String::with_capacity(text.len() + edit.line.len());
            rewritten.push_str(&text[..at]);
            rewritten.push_str(&edit.line);
            rewritten.push_str(&text[at..]);
            Some(rewritten)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(input: &str) -> Names {
        Names::new(input, None)
    }

    #[test]
    fn an_endpoint_lands_in_a_module_named_after_the_plural() {
        let plan = plan(GenerateKind::Endpoint, &names("post"), "shop");
        assert_eq!(plan.path, PathBuf::from("src/posts.rs"));
        assert!(plan.contents.contains("pub struct CreatePost"));
        assert!(plan.contents.contains(r#"GET    "/posts""#));
        assert!(plan.contents.contains(".tag(\"posts\")"));
        assert_eq!(
            plan.edits.len(),
            3,
            "declare it, mount it, and provide the store it injects"
        );
    }

    /// The three edits together must produce a chain that both compiles and
    /// boots: the provider and the mount before `build()`, and the store named
    /// through its module.
    #[test]
    fn the_generated_edits_compose_into_a_valid_builder_chain() {
        let plan = plan(GenerateKind::Endpoint, &names("post"), "shop");
        let mut text = "    App::new(config).mount(routes::router()).build()\n".to_owned();
        for edit in &plan.edits {
            if edit.file == "src/lib.rs"
                && matches!(edit.anchor, Anchor::InlineAfter(_))
                && let Some(next) = splice(&text, edit)
            {
                text = next;
            }
        }
        assert_eq!(
            text,
            "    App::new(config).provide(posts::PostStore::default())\
             .mount(routes::router()).mount(posts::router()).build()\n"
        );
    }

    #[test]
    fn no_placeholder_survives_rendering() {
        for kind in [
            GenerateKind::Endpoint,
            GenerateKind::Schema,
            GenerateKind::Error,
            GenerateKind::Middleware,
            GenerateKind::Test,
        ] {
            let plan = plan(kind, &names("blog_post"), "shop");
            assert!(
                !plan.contents.contains("@@"),
                "{kind:?} left a placeholder:\n{}",
                plan.contents
                    .lines()
                    .filter(|line| line.contains("@@"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }

    #[test]
    fn a_middleware_is_named_for_the_verb_and_not_pluralised() {
        let plan = plan(GenerateKind::Middleware, &names("observe"), "shop");
        assert_eq!(plan.path, PathBuf::from("src/observe.rs"));
        assert!(
            plan.contents.contains("pub async fn observe("),
            "{}",
            plan.contents
        );
        assert!(plan.contents.contains("OBSERVE_HEADER"));
        assert!(plan.contents.contains("\"x-observe\""));
    }

    #[test]
    fn a_test_goes_under_tests_and_registers_nothing() {
        let plan = plan(GenerateKind::Test, &names("posts"), "shop");
        assert_eq!(plan.path, PathBuf::from("tests/posts.rs"));
        assert!(plan.edits.is_empty());
        assert!(plan.contents.contains("shop::build()"));
    }

    #[test]
    fn the_module_declaration_lands_after_the_last_existing_one() {
        let base = std::env::temp_dir().join(format!("moso-gen-decl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("src")).expect("src");
        std::fs::write(
            base.join("src/lib.rs"),
            "//! doc\n\npub mod dump;\npub mod routes;\n\nuse moso::prelude::*;\n",
        )
        .expect("lib.rs");

        let edit = Edit {
            file: "src/lib.rs",
            line: "pub mod posts;".to_owned(),
            anchor: Anchor::NewLineAfterLineStartingWith("pub mod "),
        };
        assert!(apply(&base, &edit).expect("applies"));

        let text = std::fs::read_to_string(base.join("src/lib.rs")).expect("read");
        assert_eq!(
            text,
            "//! doc\n\npub mod dump;\npub mod routes;\npub mod posts;\n\nuse moso::prelude::*;\n"
        );

        // Idempotent: applying it again must not duplicate the line.
        assert!(apply(&base, &edit).expect("applies"));
        let again = std::fs::read_to_string(base.join("src/lib.rs")).expect("read");
        assert_eq!(again, text);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Regression: `moso new` writes the whole builder chain on one line, so an
    /// insert that appended a *line* put `.mount(..)` after `.build()` and
    /// chained it onto a `Result<App>` — which does not compile. The mount must
    /// be spliced into the chain itself.
    #[test]
    fn a_mount_is_spliced_into_a_single_line_builder_chain() {
        let edit = Edit {
            file: "src/lib.rs",
            line: ".mount(posts::router())".to_owned(),
            anchor: Anchor::InlineAfter(".mount(routes::router())"),
        };

        let one_line = "    App::new(config).mount(routes::router()).build()\n";
        let spliced = splice(one_line, &edit).expect("anchor found");
        assert_eq!(
            spliced,
            "    App::new(config).mount(routes::router()).mount(posts::router()).build()\n"
        );
        assert!(
            !spliced.contains(".build().mount("),
            "the mount must precede build(): {spliced}"
        );
    }

    #[test]
    fn a_mount_also_splices_into_a_chain_broken_over_several_lines() {
        let edit = Edit {
            file: "src/lib.rs",
            line: ".mount(posts::router())".to_owned(),
            anchor: Anchor::InlineAfter(".mount(routes::router())"),
        };

        let wrapped = "    App::new(config)\n        .mount(routes::router())\n        .build()\n";
        let spliced = splice(wrapped, &edit).expect("anchor found");
        assert!(
            spliced.contains(".mount(routes::router()).mount(posts::router())"),
            "{spliced}"
        );
        assert!(!spliced.contains(".build().mount("), "{spliced}");
    }

    #[test]
    fn splicing_the_same_mount_twice_is_a_no_op() {
        let edit = Edit {
            file: "src/lib.rs",
            line: ".mount(posts::router())".to_owned(),
            anchor: Anchor::InlineAfter(".mount(routes::router())"),
        };
        let once = splice("    App::new(c).mount(routes::router()).build()\n", &edit)
            .expect("anchor found");
        let twice = splice(&once, &edit).expect("anchor found");
        assert_eq!(once, twice);
    }

    #[test]
    fn a_missing_anchor_is_reported_rather_than_guessed() {
        let base = std::env::temp_dir().join(format!("moso-gen-anchor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("src")).expect("src");
        std::fs::write(
            base.join("src/lib.rs"),
            "// a project that looks nothing like ours\n",
        )
        .expect("lib.rs");

        let edit = Edit {
            file: "src/lib.rs",
            line: "pub mod posts;".to_owned(),
            anchor: Anchor::NewLineAfterLineStartingWith("pub mod "),
        };
        assert!(
            !apply(&base, &edit).expect("does not fail"),
            "a missing anchor is a false, not an error"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_absent_file_is_not_an_error_either() {
        let base = std::env::temp_dir().join(format!("moso-gen-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("base");
        let edit = Edit {
            file: "src/lib.rs",
            line: "pub mod posts;".to_owned(),
            anchor: Anchor::NewLineAfterLineStartingWith("pub mod "),
        };
        assert!(!apply(&base, &edit).expect("does not fail"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
