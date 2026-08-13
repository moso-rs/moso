//! `moso client` — a typed client, from the document the application already
//! publishes.
//!
//! # Where the document comes from
//!
//! Two sources, and the choice decides everything else the command needs:
//!
//! | Invocation | What happens |
//! | --- | --- |
//! | `moso client --out web/api` | discover the project, build it, run it with `--dump-openapi` |
//! | `moso client --input openapi.json --out api` | read the file, and touch no Rust at all |
//!
//! `--input` deliberately skips project discovery. The most common place to
//! *generate* a TypeScript client is the front-end repository, which has a
//! committed `openapi.json` and no `Cargo.toml` anywhere above it; requiring a
//! Moso project there would make the command useless exactly where it is most
//! wanted.
//!
//! # Why `--check` exists
//!
//! A generated client is a copy of a contract, and a copy drifts. `--check`
//! regenerates into memory, compares byte for byte against what is on disk, and
//! exits non-zero on any difference — the same shape as `moso openapi check`,
//! for the same reason. It reports which files differ rather than a diff: the
//! fix is always the same one command, and printing a thousand-line diff would
//! bury it.
//!
//! # What it will not do
//!
//! Delete. `--out` may hold hand-written code beside the generated files, so a
//! file the generator does not produce is left alone and is not reported as
//! stale. Removing something is the user's decision, and a code generator that
//! deletes files is a code generator nobody points at a real directory twice.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cli::{ClientArgs, ClientLang};
use crate::client::{Emitted, Target, generate, model::Api};
use crate::exit::{CliError, Outcome, io as io_error};
use crate::project::{Dump, Project};
use crate::ui::{Level, Ui};

/// Run `moso client`.
///
/// # Errors
/// [`Fault::User`](crate::exit::Fault::User) when the document cannot be read
/// or is not OpenAPI 3.1, and when `--check` finds a difference — all of them
/// things an author must fix, and all of them things that should fail a build.
/// [`Fault::Environment`](crate::exit::Fault::Environment) when a file cannot
/// be written.
pub fn run(ui: &Ui, args: &ClientArgs) -> Outcome<()> {
    let target = match args.lang {
        ClientLang::Ts => Target::TypeScript,
        ClientLang::Rust => Target::Rust,
    };

    let (document, root) = read_document(ui, args)?;
    let api = Api::parse(&document)?;
    let files = generate(&api, target);
    let out = root.join(&args.out);

    // Everything the document said that the client could not carry across, said
    // once here as well as in the generated file. A refusal a reader only meets
    // three months later in a doc comment is a refusal that was too quiet.
    for note in &api.notes {
        ui.status(Level::Warn, note, "");
    }
    for operation in &api.operations {
        for note in &operation.notes {
            ui.status(Level::Warn, &format!("{}: {note}", operation.name), "");
        }
    }

    if args.check {
        return check(ui, args, target, &out, &files);
    }
    write(ui, args, &out, &files, &api, target)
}

/// Obtain the OpenAPI document, and the directory `--out` is relative to.
fn read_document(ui: &Ui, args: &ClientArgs) -> Outcome<(Value, PathBuf)> {
    let Some(input) = &args.input else {
        let project = Project::discover(args.app.manifest_path.as_deref())?;
        project.require_moso()?;
        let answer = project.dump(&args.app, Dump::OpenApi)?;
        let document = serde_json::from_str(&answer).map_err(|error| {
            CliError::user(format!(
                "the application's `--dump-openapi` output is not JSON: {error}"
            ))
            .with_help(
                "everything except the document must go to stderr; check for a `println!` \
                 in a startup hook",
            )
        })?;
        return Ok((document, project.root));
    };

    if ui.is_verbose() {
        ui.status(Level::Info, "reading", &input.display().to_string());
    }
    let text = std::fs::read_to_string(input).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CliError::user(format!("there is no `{}` to read", input.display()))
                .with_help("write one with `moso openapi export --out openapi.json`")
        } else {
            io_error("could not read", input, &error)
        }
    })?;
    let document = serde_json::from_str(&text).map_err(|error| {
        CliError::user(format!("`{}` is not valid JSON: {error}", input.display()))
            .with_help("this command reads an OpenAPI document, not a YAML one")
    })?;

    Ok((document, PathBuf::from(".")))
}

/// Write every file, reporting which changed.
fn write(
    ui: &Ui,
    args: &ClientArgs,
    out: &Path,
    files: &[Emitted],
    api: &Api,
    target: Target,
) -> Outcome<()> {
    std::fs::create_dir_all(out).map_err(|error| io_error("could not create", out, &error))?;

    let mut written = Vec::new();
    for file in files {
        let path = out.join(&file.path);
        let previous = std::fs::read_to_string(&path).ok();
        let state = match previous.as_deref() {
            Some(existing) if existing == file.contents => "unchanged",
            Some(_) => "updated",
            None => "created",
        };
        if state != "unchanged" {
            std::fs::write(&path, &file.contents)
                .map_err(|error| io_error("could not write", &path, &error))?;
        }
        written.push((file, state));
    }

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": true,
            "lang": target.flag(),
            "out": out.display().to_string(),
            "operations": api.operations.len(),
            "types": api.types.len(),
            "notes": api.notes,
            "files": written.iter().map(|(file, state)| serde_json::json!({
                "path": file.path,
                "bytes": file.contents.len(),
                "state": state,
            })).collect::<Vec<_>>(),
        }));
        return Ok(());
    }

    for (file, state) in &written {
        ui.status(
            Level::Ok,
            &format!("{state} {}", args.out.join(&file.path).display()),
            &format!("({} bytes)", file.contents.len()),
        );
    }
    ui.blank();
    ui.status(
        Level::Ok,
        &format!("{} client", target.label()),
        &format!(
            "({} operations, {} types)",
            api.operations.len(),
            api.types.len()
        ),
    );
    ui.fix(&format!(
        "moso client --lang {} --out {} --check   # in CI",
        target.flag(),
        args.out.display()
    ));
    Ok(())
}

/// Compare what is on disk with what would be generated.
fn check(ui: &Ui, args: &ClientArgs, target: Target, out: &Path, files: &[Emitted]) -> Outcome<()> {
    let mut stale = Vec::new();
    for file in files {
        let path = out.join(&file.path);
        let state = match std::fs::read_to_string(&path) {
            Ok(existing) if existing == file.contents => continue,
            Ok(_) => "differs",
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing",
            Err(error) => return Err(io_error("could not read", &path, &error)),
        };
        stale.push((file.path.clone(), state));
    }

    if stale.is_empty() {
        if ui.is_json() {
            ui.emit_json(&serde_json::json!({
                "ok": true,
                "out": out.display().to_string(),
                "stale": [],
            }));
        } else {
            ui.status(
                Level::Ok,
                &format!("{} is up to date", args.out.display()),
                &format!("({} files)", files.len()),
            );
        }
        return Ok(());
    }

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": false,
            "out": out.display().to_string(),
            "stale": stale.iter().map(|(path, state)| serde_json::json!({
                "path": path,
                "state": state,
            })).collect::<Vec<_>>(),
        }));
    } else {
        ui.status(
            Level::Fail,
            &format!("{} is out of date", args.out.display()),
            &format!("({} of {} files)", stale.len(), files.len()),
        );
        for (path, state) in &stale {
            ui.line(&format!("      {state:<9}{path}"));
        }
    }

    let regenerate = format!(
        "moso client --lang {} --out {}",
        target.flag(),
        args.out.display()
    );
    Err(CliError::user("the generated client does not match the document").with_help(regenerate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{ClientArgs, ClientLang};
    use serde_json::json;

    /// A scratch directory nothing else in the suite writes into.
    fn scratch(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "moso-client-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("a scratch directory");
        base
    }

    fn document() -> Value {
        json!({
            "openapi": "3.1.1",
            "info": {"title": "Shop", "version": "1.0.0"},
            "paths": {"/posts": {"get": {"operationId": "posts_list", "responses": {
                "200": {"description": "ok", "content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/PostOut"}}}},
            }}}},
            "components": {"schemas": {
                "PostOut": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
            }},
        })
    }

    fn args(base: &Path, lang: ClientLang, check: bool) -> ClientArgs {
        ClientArgs {
            lang,
            out: base.join("api"),
            input: Some(base.join("openapi.json")),
            check,
            app: crate::cli::AppArgs::default(),
        }
    }

    fn seed(base: &Path) {
        std::fs::write(
            base.join("openapi.json"),
            serde_json::to_string_pretty(&document()).expect("json"),
        )
        .expect("the fixture document");
    }

    #[test]
    fn generating_writes_the_files_and_checking_then_passes() {
        let base = scratch("roundtrip");
        seed(&base);
        let ui = Ui::silent();

        run(&ui, &args(&base, ClientLang::Ts, false)).expect("generates");
        for name in ["types.ts", "client.ts", "index.ts"] {
            assert!(base.join("api").join(name).is_file(), "{name} is missing");
        }
        run(&ui, &args(&base, ClientLang::Ts, true)).expect("the check passes");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_client_that_drifted_fails_the_check_and_names_the_file() {
        let base = scratch("drift");
        seed(&base);
        let ui = Ui::silent();
        run(&ui, &args(&base, ClientLang::Ts, false)).expect("generates");

        std::fs::write(base.join("api/types.ts"), "// hand edited\n").expect("edit");
        let error = run(&ui, &args(&base, ClientLang::Ts, true)).expect_err("the check fails");
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert!(
            error
                .help
                .is_some_and(|help| help.starts_with("moso client")),
            "the fix must be pasteable"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn checking_a_directory_that_was_never_generated_fails_rather_than_writing() {
        let base = scratch("absent");
        seed(&base);
        let ui = Ui::silent();
        assert!(run(&ui, &args(&base, ClientLang::Rust, true)).is_err());
        assert!(
            !base.join("api").exists(),
            "--check must not create anything"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_missing_document_names_the_command_that_makes_one() {
        let base = scratch("missing");
        let error = run(&Ui::silent(), &args(&base, ClientLang::Ts, false))
            .expect_err("there is no document");
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert!(
            error
                .help
                .is_some_and(|help| help.contains("moso openapi export")),
            "the help must say where a document comes from"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_document_that_is_not_json_is_a_user_error_not_a_panic() {
        let base = scratch("garbage");
        std::fs::write(base.join("openapi.json"), "openapi: 3.1.1\n").expect("write");
        let error = run(&Ui::silent(), &args(&base, ClientLang::Ts, false)).expect_err("refused");
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert!(error.message.contains("not valid JSON"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn regenerating_over_an_unchanged_client_rewrites_nothing() {
        let base = scratch("idempotent");
        seed(&base);
        let ui = Ui::silent();
        run(&ui, &args(&base, ClientLang::Rust, false)).expect("generates");

        let path = base.join("api/models.rs");
        let before = std::fs::metadata(&path).expect("metadata");
        let first = std::fs::read_to_string(&path).expect("read");
        run(&ui, &args(&base, ClientLang::Rust, false)).expect("generates again");
        let after = std::fs::metadata(&path).expect("metadata");

        assert_eq!(first, std::fs::read_to_string(&path).expect("read"));
        assert_eq!(
            before.modified().ok(),
            after.modified().ok(),
            "an unchanged file must not be rewritten"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_file_the_generator_does_not_own_is_left_alone() {
        let base = scratch("coexist");
        seed(&base);
        let ui = Ui::silent();
        run(&ui, &args(&base, ClientLang::Ts, false)).expect("generates");
        std::fs::write(base.join("api/hand-written.ts"), "export const x = 1;\n").expect("write");

        run(&ui, &args(&base, ClientLang::Ts, true)).expect("the check still passes");
        assert!(base.join("api/hand-written.ts").is_file());

        let _ = std::fs::remove_dir_all(&base);
    }
}
