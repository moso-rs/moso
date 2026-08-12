//! `moso build` — the release build, plus the two things people forget.
//!
//! ```text
//! $ moso build --openapi
//!   ✓ built shop                        (release, in 41.20s)
//!   ✓ binary                            target/release/shop (8.4 MB)
//!   ✓ wrote target/release/openapi.json (12 operations)
//!     the runtime profile is chosen by MOSO_PROFILE, not by this build
//! ```
//!
//! # Why this exists next to `cargo build --release`
//!
//! Because `cargo build --release` answers with silence. It does not say where
//! the artefact went, how big it is, or that the contract clients will code
//! against is still sitting in a `target/` directory nobody copied. Those are
//! the three questions asked immediately afterwards, every time, and answering
//! them is the whole job.
//!
//! The two forgotten things:
//!
//! 1. **The document ships with the binary or it does not ship.** `--openapi`
//!    writes the OpenAPI document beside the artefact, so the image or the
//!    archive carries its own contract instead of one regenerated later from a
//!    different commit. It does that by *calling* `moso openapi export` — the
//!    dump protocol, the serialisation and the operation count all have one
//!    home, in [`commands::openapi`](super::openapi), and this command does not
//!    grow a second copy of any of it.
//! 2. **A release build is not a production profile.** Cargo's profile decides
//!    optimisation; `MOSO_PROFILE` decides whether `.env` is read and whether
//!    `/docs` is mounted. A release binary started with neither set runs as
//!    `dev`, and that is the misconfiguration this command prints a line about
//!    rather than leaving to be discovered.
//!
//! `--debug` opts out of release. It is spelled that way round because this
//! command is *for* release builds; a `--release` flag that was always on would
//! be a flag that means nothing.

use std::path::PathBuf;
use std::time::Instant;

use crate::cli::{AppArgs, BuildArgs, OpenapiExportArgs};
use crate::exit::{Outcome, io as io_error};
use crate::project::Project;
use crate::ui::{Level, Ui};

use super::doctor::human_bytes;

/// The name given to the document written beside the binary.
const DOCUMENT: &str = "openapi.json";

/// Run `moso build`.
///
/// # Errors
/// [`Fault::Environment`](crate::exit::Fault::Environment) when the project
/// cannot be found or the artefact cannot be measured, and
/// [`Fault::User`](crate::exit::Fault::User) when the package does not compile
/// or the document cannot be exported.
pub fn run(ui: &Ui, args: &BuildArgs) -> Outcome<()> {
    let project = Project::discover(args.manifest_path.as_deref())?;
    project.require_moso()?;

    let app = app_args(args);
    let started = Instant::now();
    ui.status(
        Level::Ok,
        "building",
        &format!("{} ({})", project.name, profile(args)),
    );

    let executable = project.build(&app)?;
    let elapsed = started.elapsed();

    let bytes = std::fs::metadata(&executable)
        .map_err(|error| io_error("could not measure", &executable, &error))?
        .len();

    let document = if args.openapi {
        Some(export(ui, &project, args, &app, &executable)?)
    } else {
        None
    };

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": true,
            "package": project.name,
            "binary": executable.display().to_string(),
            "bytes": bytes,
            "profile": profile(args),
            "seconds": elapsed.as_secs_f64(),
            "openapi": document.as_ref().map(|path| path.display().to_string()),
        }));
        return Ok(());
    }

    ui.status(
        Level::Ok,
        &format!("built {}", project.name),
        &format!("({}, in {:.2}s)", profile(args), elapsed.as_secs_f64()),
    );
    ui.status(
        Level::Ok,
        "binary",
        &format!(
            "{} ({})",
            display(&project, &executable),
            human_bytes(bytes)
        ),
    );
    if let Some(path) = &document {
        ui.status(Level::Ok, "openapi", &display(&project, path));
    }
    ui.blank();
    ui.status(Level::Info, "profile", &runtime_note(args));

    Ok(())
}

/// Export the document beside the binary, by calling `moso openapi export`.
///
/// Under `--json` the export is driven with a muted [`Ui`]: it would otherwise
/// print a document of its own, and two JSON documents on one standard output is
/// not JSON. The path it wrote is returned so this command can name it in the
/// one document it does emit.
fn export(
    ui: &Ui,
    project: &Project,
    args: &BuildArgs,
    app: &AppArgs,
    executable: &std::path::Path,
) -> Outcome<PathBuf> {
    let path = args
        .openapi_out
        .clone()
        .unwrap_or_else(|| beside(executable));

    let export_args = OpenapiExportArgs {
        out: Some(path.clone()),
        pretty: false,
        compact: false,
        prefix: None,
        app: app.clone(),
    };

    let muted = ui.muted();
    let reporting = if ui.is_json() { &muted } else { ui };
    super::openapi::export(reporting, &export_args)?;

    // `openapi export` resolves a relative `--out` against the project root, so
    // the path it actually wrote is the one this reports, not the one asked for.
    Ok(project.root.join(&path))
}

/// Where the document goes when `--openapi-out` did not say.
///
/// Beside the binary, because that is what "the artefact and its contract ship
/// together" means: a `COPY target/release/ /app/` takes both, and a `docker
/// build` that took only the binary is the failure mode being prevented.
fn beside(executable: &std::path::Path) -> PathBuf {
    executable
        .parent()
        .map_or_else(|| PathBuf::from(DOCUMENT), |parent| parent.join(DOCUMENT))
}

/// The [`AppArgs`] this command's own flags amount to.
///
/// `moso build` does not flatten [`AppArgs`] — see [`BuildArgs`] — so this is
/// where the two shapes meet, in one place rather than at four call sites.
fn app_args(args: &BuildArgs) -> AppArgs {
    AppArgs {
        manifest_path: args.manifest_path.clone(),
        bin: args.bin.clone(),
        release: !args.debug,
        features: args.features.clone(),
    }
}

/// Cargo's profile, as a word.
fn profile(args: &BuildArgs) -> &'static str {
    if args.debug { "debug" } else { "release" }
}

/// The line about the *other* profile.
fn runtime_note(args: &BuildArgs) -> String {
    if args.debug {
        "a debug build: unoptimised, with debug assertions on. Drop --debug to \
         build what you would deploy"
            .to_owned()
    } else {
        "cargo's profile only. Set MOSO_PROFILE=production where this runs, or \
         it will start as `dev` and mount /docs"
            .to_owned()
    }
}

/// A path relative to the project root when it is inside it.
///
/// An absolute `target/release/shop` under a long temp directory pushes the size
/// off the line it belongs on, and the reader already knows which project this
/// is.
fn display(project: &Project, path: &std::path::Path) -> String {
    path.strip_prefix(&project.root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> BuildArgs {
        BuildArgs {
            debug: false,
            openapi: false,
            openapi_out: None,
            manifest_path: None,
            bin: None,
            features: None,
        }
    }

    fn project(root: &str) -> Project {
        Project {
            manifest_path: PathBuf::from(root).join("Cargo.toml"),
            root: PathBuf::from(root),
            name: "shop".to_owned(),
            rust_version: None,
            uses_moso: true,
        }
    }

    #[test]
    fn the_default_is_a_release_build_and_debug_is_the_opt_out() {
        assert!(app_args(&args()).release);
        assert_eq!(profile(&args()), "release");

        let debug = BuildArgs {
            debug: true,
            ..args()
        };
        assert!(!app_args(&debug).release);
        assert_eq!(profile(&debug), "debug");
    }

    #[test]
    fn every_build_flag_survives_the_hop_into_app_args() {
        let args = BuildArgs {
            bin: Some("worker".to_owned()),
            features: Some("orm,jobs".to_owned()),
            manifest_path: Some(PathBuf::from("/tmp/shop/Cargo.toml")),
            ..args()
        };
        let app = app_args(&args);
        assert_eq!(app.bin.as_deref(), Some("worker"));
        assert_eq!(app.features.as_deref(), Some("orm,jobs"));
        assert_eq!(
            app.manifest_path,
            Some(PathBuf::from("/tmp/shop/Cargo.toml"))
        );
    }

    #[test]
    fn the_document_lands_next_to_the_binary() {
        let path = beside(std::path::Path::new("/tmp/shop/target/release/shop"));
        assert_eq!(path, PathBuf::from("/tmp/shop/target/release/openapi.json"));
    }

    #[test]
    fn a_binary_with_no_parent_still_names_a_document() {
        assert_eq!(
            beside(std::path::Path::new("shop")),
            PathBuf::from(DOCUMENT)
        );
    }

    #[test]
    fn paths_are_reported_relative_to_the_project() {
        let project = project("/tmp/shop");
        assert_eq!(
            display(
                &project,
                std::path::Path::new("/tmp/shop/target/release/shop")
            ),
            "target/release/shop"
        );
        assert_eq!(
            display(&project, std::path::Path::new("/elsewhere/shop")),
            "/elsewhere/shop"
        );
    }

    #[test]
    fn a_release_build_is_told_that_it_is_not_a_production_profile() {
        // The whole point of the line: cargo's profile and MOSO_PROFILE are
        // different decisions, and only one of them was just made.
        let note = runtime_note(&args());
        assert!(note.contains("MOSO_PROFILE=production"), "{note}");
        assert!(
            runtime_note(&BuildArgs {
                debug: true,
                ..args()
            })
            .contains("--debug")
        );
    }
}
