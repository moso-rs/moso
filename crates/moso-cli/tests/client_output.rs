//! The tests that prove a generated client is real code and not plausible text.
//!
//! The unit tests in `src/client/` assert about strings, which is the right
//! level for "does `oneOf` become a union". It is the wrong level for the only
//! question a user actually has: *does the thing I am about to commit compile?*
//! So this file drives the real binary against the real document
//! `examples/crud` publishes, and then hands the output to a real compiler.
//!
//! # Why the compiling half is opt-in
//!
//! Both checks need a toolchain the workspace does not otherwise require —
//! Node 22 or newer for the TypeScript, and a cargo registry that can resolve
//! `serde` into a scratch crate for the Rust — and the Rust one downloads and
//! builds `serde_derive`. Neither belongs in the default `cargo test` path, so:
//!
//! - [`the_crud_document_generates_deterministically`] always runs. It needs
//!   nothing but the binary, and it is the property the whole design rests on.
//! - The two `#[ignore]`d tests are how the generators are verified before a
//!   release:
//!
//! ```sh
//! cargo test -p moso-cli --test client_output -- --ignored --nocapture
//! ```
//!
//! A missing toolchain **skips** rather than fails, the same rule the database
//! suites follow: a test that fails because a machine has no Node says nothing
//! about Moso.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The `moso` binary cargo just built for this test.
const MOSO: &str = env!("CARGO_BIN_EXE_moso");

/// A scratch directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path =
            std::env::temp_dir().join(format!("moso-client-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).expect("scratch directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The committed document of the tutorial application.
///
/// A real Moso document rather than a fixture: it is the one shape the
/// generator has to be right about, and it grows new constructs whenever the
/// example does.
fn crud_document() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the repository root")
        .join("examples/crud/openapi.json")
}

/// Run `moso client` and fail loudly if it did not exit 0.
fn generate(out: &Path, lang: &str, extra: &[&str]) {
    let output = Command::new(MOSO)
        .args(["client", "--lang", lang, "--input"])
        .arg(crud_document())
        .arg("--out")
        .arg(out)
        .args(extra)
        .output()
        .expect("the moso binary runs");
    assert!(
        output.status.success(),
        "moso client --lang {lang} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every file under `directory`, as (name, contents), sorted.
fn snapshot(directory: &Path) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = std::fs::read_dir(directory)
        .expect("the output directory")
        .filter_map(Result::ok)
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read_to_string(entry.path()).unwrap_or_default(),
            )
        })
        .collect();
    found.sort();
    found
}

/// Whether a command exists and can be run.
fn available(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .is_ok_and(|output| output.status.success())
}

// ---------------------------------------------------------------------------
// Always run
// ---------------------------------------------------------------------------

/// The property everything else rests on: one document, one output, every time.
///
/// Also the property that makes `--check` meaningful, so it is checked in both
/// directions — regenerating changes nothing, and `--check` agrees.
#[test]
fn the_crud_document_generates_deterministically() {
    for lang in ["ts", "rust"] {
        let first = Scratch::new(&format!("det-a-{lang}"));
        let second = Scratch::new(&format!("det-b-{lang}"));
        generate(first.path(), lang, &[]);
        generate(second.path(), lang, &[]);

        let left = snapshot(first.path());
        let right = snapshot(second.path());
        assert_eq!(left.len(), 3, "{lang} should write three files");
        assert_eq!(left, right, "{lang} output is not deterministic");

        // And what is on disk is what --check expects to find.
        generate(first.path(), lang, &["--check"]);
    }
}

/// A hand edit must fail the check, with the exit code CI branches on.
#[test]
fn an_edited_client_fails_the_check_with_a_user_error() {
    let out = Scratch::new("drift");
    generate(out.path(), "ts", &[]);
    std::fs::write(out.path().join("types.ts"), "// edited\n").expect("edit");

    let output = Command::new(MOSO)
        .args(["client", "--lang", "ts", "--input"])
        .arg(crud_document())
        .arg("--out")
        .arg(out.path())
        .arg("--check")
        .output()
        .expect("the moso binary runs");

    assert_eq!(output.status.code(), Some(1), "a user error is exit code 1");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("moso client"),
        "the fix must be pasteable:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Opt-in: the generated code is handed to a real compiler
// ---------------------------------------------------------------------------

/// The generated TypeScript parses.
///
/// `node --experimental-strip-types --check` is a full TypeScript parse without
/// a TypeScript installation. It only accepts *erasable* syntax, which is a
/// second thing worth holding: the output must pass through esbuild, swc and
/// Node's own loader untouched, so it may not contain an `enum` or a
/// `namespace`.
#[test]
#[ignore = "needs Node 22 or newer; run with --ignored"]
fn the_generated_typescript_parses() {
    if !available("node", &["--version"]) {
        eprintln!("skipping: no `node` on PATH");
        return;
    }

    let out = Scratch::new("tsc");
    generate(out.path(), "ts", &[]);

    for (name, _) in snapshot(out.path()) {
        let output = Command::new("node")
            .arg("--experimental-strip-types")
            .arg("--check")
            .arg(out.path().join(&name))
            .output()
            .expect("node runs");
        assert!(
            output.status.success(),
            "{name} is not valid TypeScript:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// The generated Rust compiles, with no warnings, against `serde` alone.
///
/// The scratch crate is deliberately outside the workspace and depends on
/// nothing but what the generated module's own documentation tells a user to
/// paste. If this passes, that snippet is complete.
#[test]
#[ignore = "builds a scratch crate against the registry; run with --ignored"]
fn the_generated_rust_compiles_against_serde_alone() {
    if !available("cargo", &["--version"]) {
        eprintln!("skipping: no `cargo` on PATH");
        return;
    }

    let scratch = Scratch::new("rustc");
    let crate_root = scratch.path().join("probe");
    std::fs::create_dir_all(crate_root.join("src")).expect("src");
    generate(&crate_root.join("src/api"), "rust", &[]);

    std::fs::write(
        crate_root.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\n\
         serde_json = \"1\"\n\n[workspace]\n",
    )
    .expect("manifest");
    std::fs::write(crate_root.join("src/lib.rs"), PROBE).expect("lib.rs");

    // `cargo rustc` rather than `cargo build`: the flags reach the crate being
    // built and not its dependencies, so a warning in `serde` is not a failure
    // of the generator.
    let output = Command::new("cargo")
        .args(["rustc", "--", "-D", "warnings"])
        .current_dir(&crate_root)
        .output()
        .expect("cargo runs");
    assert!(
        output.status.success(),
        "the generated Rust does not compile:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A program that uses the generated client the way a user would.
///
/// It calls an operation of each shape — query parameters, a JSON body, a path
/// parameter, an empty response — and branches on a validation failure by its
/// field code, which is the ergonomics claim the whole error model makes.
const PROBE: &str = r##"pub mod api;

use api::{ApiError, ApiRequest, ApiResponse, Client, Transport};

/// A transport that answers from memory.
pub struct Canned(pub ApiResponse);

impl Transport for Canned {
    type Error = std::io::Error;

    async fn send(&self, _request: ApiRequest) -> Result<ApiResponse, Self::Error> {
        Ok(self.0.clone())
    }
}

/// Exercise every argument shape the document produces.
pub async fn exercise() {
    let client = Client::new(
        Canned(ApiResponse {
            status: 422,
            headers: Vec::new(),
            body: br#"{"title":"Validation failed","status":422,
                      "errors":[{"pointer":"/title","code":"len","message":"short"}]}"#
                .to_vec(),
        }),
        api::DEFAULT_BASE_URL,
    );

    let listed = client
        .posts_list(&api::PostsListParams {
            limit: Some(10),
            drafts: Some(true),
            ..Default::default()
        })
        .await;
    match listed {
        Err(ApiError::Problem { problem, .. }) => {
            assert!(problem.has_code("len"));
            assert_eq!(
                problem.field_error("/title").map(|entry| entry.code.as_str()),
                Some("len")
            );
        }
        _ => panic!("expected a problem"),
    }

    let created = client
        .posts_create(&api::CreatePost {
            title: "Hello".to_owned(),
            body: "World".to_owned(),
            tags: None,
            publish: None,
        })
        .await;
    assert!(created.is_err());

    assert!(
        client
            .posts_show(&api::PostsShowParams { id: "a/b".to_owned() })
            .await
            .is_err()
    );
    assert!(
        client
            .posts_destroy(&api::PostsDestroyParams { id: "x".to_owned() })
            .await
            .is_err()
    );
}
"##;
