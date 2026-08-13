//! The only test that proves `moso new` is worth shipping.
//!
//! Everything else in this crate asserts about strings. This drives the real
//! binary, into a real directory, and then makes cargo build and test what came
//! out — which is the acceptance criterion `40-cli.md` states for `moso new`:
//! the generated project compiles and its test passes.
//!
//! # Why the build is opt-in
//!
//! Compiling a generated project means compiling Moso, from scratch, into a
//! target directory the workspace does not share. That is a minute or two, and
//! putting it in the default `cargo test --workspace` path would make the whole
//! suite too slow to run on every change. So:
//!
//! - `generation_*` tests always run, and cover everything that does not need a
//!   compiler: the files, the manifest, the substitutions, the exit codes.
//! - [`the_generated_project_builds_and_its_tests_pass`] is `#[ignore]`d, and is
//!   how the template is verified before a release:
//!
//! ```sh
//! cargo test -p moso-cli -- --ignored --nocapture
//! ```
//!
//! It builds against the Moso in *this checkout* (`--moso-path`), so it tests
//! the template against the framework it will ship with rather than against
//! whatever is on crates.io.

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
            std::env::temp_dir().join(format!("moso-new-{tag}-{}-{nanos}", std::process::id()));
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

/// The `crates/moso` of this checkout, as an absolute path.
fn moso_crate() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<repo>/crates/moso-cli`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .join("moso")
}

/// Run `moso new <name>` into `target`, returning stdout.
fn generate(target: &Path, name: &str, extra: &[&str]) -> std::process::Output {
    let mut command = Command::new(MOSO);
    command
        .arg("new")
        .arg(name)
        .arg("--path")
        .arg(target)
        .arg("--yes")
        .arg("--no-git")
        .arg("--moso-path")
        .arg(moso_crate())
        .args(extra);
    command.output().expect("the CLI runs")
}

#[test]
fn generation_writes_a_complete_project() {
    let scratch = Scratch::new("complete");
    let target = scratch.path().join("shop");
    let output = generate(&target, "shop", &[]);

    assert!(
        output.status.success(),
        "moso new failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for relative in [
        "Cargo.toml",
        ".gitignore",
        ".env.example",
        ".cargo/config.toml",
        // M1's definition-of-done step 8: deployable as a single container
        // image from a Dockerfile the user did not have to write.
        "Dockerfile",
        ".dockerignore",
        "README.md",
        "src/lib.rs",
        "src/main.rs",
        "src/routes.rs",
        "src/dump.rs",
        "tests/api.rs",
    ] {
        assert!(target.join(relative).is_file(), "missing {relative}");
    }

    let manifest = std::fs::read_to_string(target.join("Cargo.toml")).expect("manifest");
    let parsed: toml::Value = toml::from_str(&manifest).expect("valid TOML");
    assert_eq!(parsed["package"]["name"].as_str(), Some("shop"));
    assert!(
        manifest.contains("crates/moso"),
        "--moso-path did not reach the manifest: {manifest}"
    );

    // Not a single placeholder may survive into a generated project.
    for relative in ["Cargo.toml", "src/lib.rs", "src/main.rs", "tests/api.rs"] {
        let contents = std::fs::read_to_string(target.join(relative)).expect(relative);
        assert!(
            !contents.contains("@@"),
            "{relative} still has a placeholder"
        );
    }
}

#[test]
fn generation_is_json_when_asked() {
    let scratch = Scratch::new("json");
    let target = scratch.path().join("shop");
    let output = generate(&target, "shop", &["--json"]);
    assert!(output.status.success());

    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON document");
    assert_eq!(document["ok"], serde_json::json!(true));
    assert_eq!(document["package"], serde_json::json!("shop"));
    assert_eq!(document["env_prefix"], serde_json::json!("SHOP"));
    // Name the files rather than counting them: a bare count says nothing about
    // *which* file went missing, and it has to be edited every time the template
    // grows, which is how it stops being an assertion about anything.
    let files: Vec<&str> = document["files"]
        .as_array()
        .expect("files is an array")
        .iter()
        .map(|file| file.as_str().unwrap_or_default())
        .collect();
    for expected in [
        "Cargo.toml",
        ".gitignore",
        ".env.example",
        ".cargo/config.toml",
        "Dockerfile",
        ".dockerignore",
        "README.md",
        "src/lib.rs",
        "src/main.rs",
        "src/routes.rs",
        "src/dump.rs",
        "tests/api.rs",
    ] {
        assert!(
            files.contains(&expected),
            "{expected} missing from {files:?}"
        );
    }
    assert_eq!(files.len(), 12, "{files:?}");
}

#[test]
fn a_bad_name_exits_one_and_says_what_to_type_instead() {
    let scratch = Scratch::new("bad-name");
    let output = generate(&scratch.path().join("nope"), "my shop!", &[]);

    assert_eq!(output.status.code(), Some(1), "user error is exit code 1");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error:"), "{stderr}");
    assert!(stderr.contains("moso new my-shop"), "{stderr}");
}

#[test]
fn a_bad_command_line_exits_two() {
    let output = Command::new(MOSO)
        .arg("definitely-not-a-subcommand")
        .output()
        .expect("the CLI runs");
    assert_eq!(output.status.code(), Some(2), "usage error is exit code 2");
}

/// `moso auth calibrate` has to find a project before it can measure anything,
/// and "there is no Cargo.toml here" is an environment problem rather than
/// something the caller typed wrongly.
#[test]
fn calibrate_outside_a_project_exits_three_and_says_where_to_run_it() {
    let scratch = Scratch::new("no-project-auth");
    let output = Command::new(MOSO)
        .args(["auth", "calibrate"])
        .current_dir(scratch.path())
        .output()
        .expect("the CLI runs");

    assert_eq!(
        output.status.code(),
        Some(3),
        "environment problem is exit code 3: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("moso new"), "{stderr}");
}

#[test]
fn routes_outside_a_project_exits_three() {
    let scratch = Scratch::new("no-project");
    let output = Command::new(MOSO)
        .arg("routes")
        .current_dir(scratch.path())
        .output()
        .expect("the CLI runs");

    // Nothing above a temp directory is a Cargo package, so discovery fails —
    // an environment problem, not a user error.
    assert_eq!(
        output.status.code(),
        Some(3),
        "environment problem is exit code 3: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn doctor_reports_this_machine_and_exits_zero() {
    let output = Command::new(MOSO)
        .arg("doctor")
        .arg("--json")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("the CLI runs");

    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON document");
    let checks = document["checks"].as_array().expect("checks is an array");
    assert!(checks.len() >= 5, "only {} checks ran", checks.len());

    let names: Vec<&str> = checks
        .iter()
        .filter_map(|check| check["name"].as_str())
        .collect();
    assert!(names.contains(&"rustc"), "{names:?}");
    assert!(names.contains(&"cargo"), "{names:?}");
    assert!(names.contains(&"linker"), "{names:?}");
    assert!(names.contains(&"disk"), "{names:?}");

    // A toolchain able to run this test is a toolchain doctor must be happy
    // with, so the run has to succeed.
    assert_eq!(document["ok"], serde_json::json!(true));
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn completions_are_produced_for_every_shell() {
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let output = Command::new(MOSO)
            .args(["self", "completions", shell])
            .output()
            .expect("the CLI runs");
        assert!(output.status.success(), "{shell} failed");
        assert!(!output.stdout.is_empty(), "{shell} produced nothing");
    }

    let output = Command::new(MOSO)
        .args(["self", "completions", "klingon"])
        .output()
        .expect("the CLI runs");
    assert_eq!(output.status.code(), Some(2), "an unknown shell is usage");
}

/// The acceptance test: generate, compile, run the generated test suite, and
/// then drive every dump-based command against the result.
///
/// Ignored by default; see the module header.
#[test]
#[ignore = "compiles a whole project; run with `cargo test -p moso-cli -- --ignored`"]
fn the_generated_project_builds_and_its_tests_pass() {
    let scratch = Scratch::new("builds");
    let target = scratch.path().join("shop");
    let output = generate(&target, "shop", &[]);
    assert!(
        output.status.success(),
        "moso new failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // A target directory inside the scratch, so a failed run leaves nothing
    // behind and a successful one is cleaned up with everything else.
    let target_dir = scratch.path().join("target");

    let tested = Command::new(env!("CARGO"))
        .arg("test")
        .current_dir(&target)
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()
        .expect("cargo runs");
    assert!(tested.success(), "the generated project's tests failed");

    // The dump protocol, end to end, against a project that has never been
    // touched by hand.
    let moso = |arguments: &[&str]| {
        Command::new(MOSO)
            .args(arguments)
            .current_dir(&target)
            .env("CARGO_TARGET_DIR", &target_dir)
            .output()
            .expect("the CLI runs")
    };

    let routes = moso(&["routes", "--json"]);
    assert!(routes.status.success(), "moso routes failed");
    let document: serde_json::Value =
        serde_json::from_slice(&routes.stdout).expect("routes emitted JSON");
    assert_eq!(
        document["routes"]
            .as_array()
            .expect("routes is an array")
            .len(),
        2
    );

    let exported = moso(&["openapi", "export", "--out", "openapi.json"]);
    assert!(exported.status.success(), "moso openapi export failed");
    assert!(target.join("openapi.json").is_file());

    let checked = moso(&["openapi", "check"]);
    assert!(
        checked.status.success(),
        "a freshly exported document must pass `openapi check`"
    );

    // Break it, and `check` must fail with the user-error code.
    let stale = std::fs::read_to_string(target.join("openapi.json"))
        .expect("openapi.json")
        .replace("\"3.1", "\"9.9");
    std::fs::write(target.join("openapi.json"), stale).expect("rewrite");
    let stale = moso(&["openapi", "check"]);
    assert_eq!(stale.status.code(), Some(1), "a stale document is exit 1");

    // `.env.example` is generated from AppConfig, so regenerating it over the
    // committed file must be a no-op.
    let committed = std::fs::read_to_string(target.join(".env.example")).expect(".env.example");
    let regenerated = moso(&["config", "--env-example"]);
    assert!(
        regenerated.status.success(),
        "moso config --env-example failed"
    );
    assert_eq!(
        String::from_utf8_lossy(&regenerated.stdout),
        committed,
        "the committed .env.example has drifted from the Config type"
    );
}

/// The acceptance test for `moso new --with-db` and `moso db`.
///
/// The database half runs only when `DATABASE_URL` is set, exactly as every
/// other data-layer suite in this workspace does — but the *compile* half runs
/// regardless, because a template that does not build is the failure worth
/// catching on every machine.
///
/// Ignored by default, for the reason in the module header.
#[test]
#[ignore = "compiles a whole project; run with `cargo test -p moso-cli -- --ignored`"]
fn a_with_db_project_builds_and_moso_db_drives_it() {
    let scratch = Scratch::new("with-db");
    let target = scratch.path().join("shop");
    let output = generate(&target, "shop", &["--with-db"]);
    assert!(
        output.status.success(),
        "moso new --with-db failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for relative in ["src/db.rs", "migrations/20260101T000000_init.sql"] {
        assert!(target.join(relative).is_file(), "missing {relative}");
    }
    let manifest = std::fs::read_to_string(target.join("Cargo.toml")).expect("manifest");
    assert!(manifest.contains("moso-migrate"), "{manifest}");

    // Without `--with-db` none of that appears, which is the whole point of the
    // flag: a project that does not need a database must not compile sqlx.
    let plain = Scratch::new("without-db");
    let plain_target = plain.path().join("plain");
    assert!(generate(&plain_target, "plain", &[]).status.success());
    assert!(!plain_target.join("src/db.rs").exists());
    assert!(!plain_target.join("migrations").exists());
    let plain_manifest =
        std::fs::read_to_string(plain_target.join("Cargo.toml")).expect("manifest");
    assert!(!plain_manifest.contains("moso-migrate"), "{plain_manifest}");

    let target_dir = scratch.path().join("target");
    let tested = Command::new(env!("CARGO"))
        .arg("test")
        .current_dir(&target)
        .env("CARGO_TARGET_DIR", &target_dir)
        // A URL that is never connected to: the tests do not touch the
        // database, but the configuration is required and must resolve.
        .env("SHOP__DATABASE_URL", "postgres://unused@localhost/unused")
        .status()
        .expect("cargo runs");
    assert!(tested.success(), "the --with-db project did not build");

    let moso = |arguments: &[&str], url: &str| {
        Command::new(MOSO)
            .args(arguments)
            .current_dir(&target)
            .env("CARGO_TARGET_DIR", &target_dir)
            .env("SHOP__DATABASE_URL", url)
            .output()
            .expect("the CLI runs")
    };

    // `moso db` against a project with no `src/db.rs` is a user error naming the
    // flag that fixes it, and it must not wait out the migration timeout to say
    // so — so this runs against the *plain* project and is expected to be fast.
    let refused = Command::new(MOSO)
        .args(["db", "status"])
        .current_dir(&plain_target)
        .output()
        .expect("the CLI runs");
    assert_eq!(
        refused.status.code(),
        Some(1),
        "a project with no db is exit 1"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("--with-db"), "{stderr}");

    let Ok(base) = std::env::var("DATABASE_URL") else {
        println!(
            "skipping the `moso db` leg: DATABASE_URL is not set. \
             Start one with `./scripts/test-db.sh up`."
        );
        return;
    };

    // A database of this test's own, so a shared `moso_test` ledger written by
    // another suite cannot make `status` report drift that is not ours.
    let url = match base.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/moso_cli_with_db_test"),
        None => base.clone(),
    };
    let _ = Command::new("psql")
        .args([&base, "-c", "DROP DATABASE IF EXISTS moso_cli_with_db_test"])
        .status();
    let created = Command::new("psql")
        .args([&base, "-c", "CREATE DATABASE moso_cli_with_db_test"])
        .status();
    if !created.is_ok_and(|status| status.success()) {
        println!("skipping the `moso db` leg: psql could not create the scratch database");
        return;
    }

    let status = moso(&["db", "status", "--json"], &url);
    assert!(status.status.success(), "a fresh database is clean");
    let document: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status emitted JSON");
    assert_eq!(document["pending"].as_array().map(Vec::len), Some(1));
    assert_eq!(document["applied"].as_array().map(Vec::len), Some(0));

    let migrated = moso(&["db", "migrate", "--json"], &url);
    assert!(migrated.status.success(), "migrate failed");
    let document: serde_json::Value =
        serde_json::from_slice(&migrated.stdout).expect("migrate emitted JSON");
    assert_eq!(document["applied"].as_array().map(Vec::len), Some(1));

    let after = moso(&["db", "status", "--json"], &url);
    let document: serde_json::Value =
        serde_json::from_slice(&after.stdout).expect("status emitted JSON");
    assert_eq!(document["pending"].as_array().map(Vec::len), Some(0));
    assert_eq!(document["applied"].as_array().map(Vec::len), Some(1));
    assert_eq!(document["clean"], serde_json::json!(true));

    // Down and back up again: a `down` section that does not undo its `up` is
    // the defect this catches, and it is only ever found by running it.
    let rolled = moso(&["db", "rollback", "--json"], &url);
    assert!(rolled.status.success(), "rollback failed");
    let document: serde_json::Value =
        serde_json::from_slice(&rolled.stdout).expect("rollback emitted JSON");
    assert_eq!(document["reverted"].as_array().map(Vec::len), Some(1));

    let redone = moso(&["db", "redo", "--json"], &url);
    assert!(redone.status.success(), "redo failed after a rollback");

    let _ = Command::new("psql")
        .args([&base, "-c", "DROP DATABASE IF EXISTS moso_cli_with_db_test"])
        .status();
}

/// The acceptance test for `moso new --auth` and `moso auth calibrate`.
///
/// The bar is the one `--with-db` sets: the generated project has to build and
/// its own tests have to pass. Those tests are the interesting half — they drive
/// registration, login, the session listing, logout and a password reset over
/// HTTP against the composed application — so "the template compiles" and "the
/// flows work" are one assertion here rather than two.
///
/// Then the two things only the CLI can prove: that the copied handlers reach
/// the project's *own* OpenAPI document (which is the whole reason the copy-out
/// tier exists), and that `moso auth calibrate` measures the binary and comes
/// back with parameters at or above OWASP's floor.
///
/// Ignored by default, for the reason in the module header.
#[test]
#[ignore = "compiles a whole project; run with `cargo test -p moso-cli -- --ignored`"]
fn an_auth_project_builds_its_flows_pass_and_calibration_measures_it() {
    let scratch = Scratch::new("auth");
    let target = scratch.path().join("shop");
    let output = generate(&target, "shop", &["--auth"]);
    assert!(
        output.status.success(),
        "moso new --auth failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for relative in ["src/auth.rs", "tests/auth.rs", ".env"] {
        assert!(target.join(relative).is_file(), "missing {relative}");
    }
    let manifest = std::fs::read_to_string(target.join("Cargo.toml")).expect("manifest");
    assert!(manifest.contains("features = [\"auth\"]"), "{manifest}");
    assert!(manifest.contains("moso-kv"), "{manifest}");

    // The signing key is a real one from this machine, and it is the only
    // secret `moso new` ever writes — so it must not be committable.
    let env = std::fs::read_to_string(target.join(".env")).expect(".env");
    assert!(env.contains("SHOP__SESSION_SECRET=base64:"), "{env}");
    let ignored = std::fs::read_to_string(target.join(".gitignore")).expect(".gitignore");
    assert!(
        ignored.lines().any(|line| line.trim() == ".env"),
        "{ignored}"
    );

    // Without `--auth`, none of it appears: an application that does not
    // authenticate anybody must not compile argon2 to find that out.
    let plain = Scratch::new("without-auth");
    let plain_target = plain.path().join("plain");
    assert!(generate(&plain_target, "plain", &[]).status.success());
    assert!(!plain_target.join("src/auth.rs").exists());
    assert!(!plain_target.join(".env").exists());

    // ── the bar: it builds, and its own tests pass ──────────────────────────
    let target_dir = scratch.path().join("target");
    let tested = Command::new(env!("CARGO"))
        .arg("test")
        .current_dir(&target)
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()
        .expect("cargo runs");
    assert!(
        tested.success(),
        "the --auth project did not build, or its flow tests failed"
    );

    let moso = |arguments: &[&str]| {
        Command::new(MOSO)
            .args(arguments)
            .current_dir(&target)
            .env("CARGO_TARGET_DIR", &target_dir)
            .output()
            .expect("the CLI runs")
    };

    // ── the copied handlers are in the project's own contract ───────────────
    //
    // This is what the mounted `moso::auth::routes()` cannot do, and it is the
    // single best reason the copy-out tier exists: `#[endpoint]` is available
    // above the facade and not below it.
    let routes = moso(&["routes", "--json"]);
    assert!(
        routes.status.success(),
        "moso routes failed: {}",
        String::from_utf8_lossy(&routes.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&routes.stdout).expect("routes emitted JSON");
    let rows = document["routes"].as_array().expect("routes is an array");
    for expected in [
        "/auth/register",
        "/auth/login",
        "/auth/logout",
        "/auth/sessions",
        "/auth/password/forgot",
        "/auth/password/reset",
    ] {
        let found = rows
            .iter()
            .find(|route| route["path"].as_str() == Some(expected))
            .unwrap_or_else(|| panic!("{expected} is not registered: {document:#?}"));
        assert_eq!(
            found["documented"],
            serde_json::json!(true),
            "{expected} reached the router without #[endpoint]"
        );
    }

    // A generated project passes its own lints, which is also what proves the
    // committed `.env.example` matches what `#[derive(Config)]` renders — four
    // new keys, hand-written into the template, byte for byte.
    let checked = moso(&["check", "--json"]);
    let findings: serde_json::Value =
        serde_json::from_slice(&checked.stdout).expect("check emitted JSON");
    assert_eq!(
        findings["findings"].as_array().expect("findings").len(),
        0,
        "a generated project must pass its own lints: {findings:#?}"
    );
    assert!(checked.status.success(), "a clean project is exit 0");

    // ── moso auth calibrate ─────────────────────────────────────────────────
    //
    // 50 ms is the floor the parser accepts and it keeps this test to one
    // search rather than a dozen: what is being asserted is the protocol and
    // the refusal, not the number, which is a property of whatever machine is
    // running the suite.
    let calibrated = moso(&["auth", "calibrate", "--target-ms", "50", "--json"]);
    assert!(
        calibrated.status.success(),
        "moso auth calibrate failed: {}",
        String::from_utf8_lossy(&calibrated.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&calibrated.stdout).expect("calibrate emitted JSON");
    assert_eq!(document["available"], serde_json::json!(true));

    let params = &document["params"];
    let floor = &document["floor"];
    assert_eq!(floor["memory_kib"], serde_json::json!(19_456), "{document}");
    for dimension in ["memory_kib", "iterations", "parallelism"] {
        let measured = params[dimension].as_u64().expect(dimension);
        let minimum = floor[dimension].as_u64().expect(dimension);
        assert!(
            measured >= minimum,
            "{dimension}: {measured} is below the floor of {minimum}"
        );
    }

    // The lines it prints are the application's own keys, not a guess.
    let config: Vec<&str> = document["config"]
        .as_array()
        .expect("config is an array")
        .iter()
        .map(|line| line.as_str().unwrap_or_default())
        .collect();
    assert!(
        config
            .iter()
            .any(|line| line.starts_with("SHOP__HASH_MEMORY_KIB=")),
        "{config:?}"
    );

    // Human output, for the shape a person actually reads.
    let printed = moso(&["auth", "calibrate", "--target-ms", "50"]);
    assert!(printed.status.success());
    let stdout = String::from_utf8_lossy(&printed.stdout);
    assert!(stdout.contains("memory_kib"), "{stdout}");
    assert!(stdout.contains("HashParams::new("), "{stdout}");
}

/// The acceptance test for `moso generate`: scaffold every kind into a fresh
/// project and make cargo compile and test the result.
///
/// This is the only assertion that catches the failure mode that matters — a
/// template that renders beautifully and does not compile. The unit tests in
/// `commands::generate` check placement and placeholders; only a compiler
/// checks the code.
///
/// Ignored by default, for the reason in the module header.
#[test]
#[ignore = "compiles a whole project; run with `cargo test -p moso-cli -- --ignored`"]
fn every_generated_kind_compiles_and_its_tests_pass() {
    let scratch = Scratch::new("generate");
    let target = scratch.path().join("shop");
    let output = generate(&target, "shop", &[]);
    assert!(
        output.status.success(),
        "moso new failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let target_dir = scratch.path().join("target");
    let moso = |arguments: &[&str]| {
        Command::new(MOSO)
            .args(arguments)
            .current_dir(&target)
            .env("CARGO_TARGET_DIR", &target_dir)
            .output()
            .expect("the CLI runs")
    };

    for arguments in [
        ["generate", "endpoint", "post"],
        ["generate", "error", "billing"],
        ["generate", "middleware", "observe"],
        ["generate", "schema", "invoice"],
        ["generate", "test", "posts"],
    ] {
        let generated = moso(&arguments);
        assert!(
            generated.status.success(),
            "`moso {}` failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&generated.stderr)
        );
    }

    // The endpoint's three registrations have to compose into a chain that both
    // compiles and boots: `.provide` before `.build`, `.mount` before `.build`.
    let lib = std::fs::read_to_string(target.join("src/lib.rs")).expect("lib.rs");
    assert!(lib.contains("pub mod posts;"), "{lib}");
    assert!(lib.contains(".mount(posts::router())"), "{lib}");
    assert!(
        lib.contains(".provide(posts::PostStore::default())"),
        "{lib}"
    );
    assert!(
        !lib.contains(".build().mount(") && !lib.contains(".build().provide("),
        "an edit landed after build(): {lib}"
    );

    let tested = Command::new(env!("CARGO"))
        .arg("test")
        .current_dir(&target)
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()
        .expect("cargo runs");
    assert!(
        tested.success(),
        "the generated project did not compile or its tests failed"
    );

    // Re-running a generator must refuse rather than clobber.
    let again = moso(&["generate", "endpoint", "post"]);
    assert_eq!(
        again.status.code(),
        Some(1),
        "generating over an existing file is a user error"
    );

    // The five new routes reach the contract, which is what proves the module
    // was mounted and not merely declared.
    let routes = moso(&["routes", "--json"]);
    assert!(routes.status.success(), "moso routes failed");
    let document: serde_json::Value =
        serde_json::from_slice(&routes.stdout).expect("routes emitted JSON");
    let paths: Vec<String> = document["routes"]
        .as_array()
        .expect("routes is an array")
        .iter()
        .map(|route| route["path"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(paths.iter().any(|path| path == "/posts"), "{paths:?}");
    assert!(paths.iter().any(|path| path == "/posts/{id}"), "{paths:?}");
}

/// The acceptance test for the four introspection commands.
///
/// All four run the generated binary with a flag `src/dump.rs` has to
/// recognise. If one of them did not, `main` would fall through to `serve()` and
/// the command would hang until its timeout — so the assertion that matters most
/// here is that each of these *returns*, whatever it returns.
///
/// Ignored by default, for the reason in the module header.
#[test]
#[ignore = "compiles a whole project; run with `cargo test -p moso-cli -- --ignored`"]
fn middleware_check_jobs_and_authz_all_drive_a_generated_project() {
    let scratch = Scratch::new("introspection");
    let target = scratch.path().join("shop");
    assert!(generate(&target, "shop", &[]).status.success());

    let target_dir = scratch.path().join("target");
    let moso = |arguments: &[&str]| {
        Command::new(MOSO)
            .args(arguments)
            .current_dir(&target)
            .env("CARGO_TARGET_DIR", &target_dir)
            .output()
            .expect("the CLI runs")
    };

    // ── moso middleware ───────────────────────────────────────────────────
    let stack = moso(&["middleware", "--json"]);
    assert!(
        stack.status.success(),
        "moso middleware failed: {}",
        String::from_utf8_lossy(&stack.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&stack.stdout).expect("middleware emitted JSON");
    let global = document["global"]
        .as_array()
        .expect("global is an array")
        .clone();
    assert!(!global.is_empty(), "the standard stack is not empty");
    assert!(
        global
            .iter()
            .any(|entry| entry["name"] == "catch_panic" || entry["name"] == "catch_error"),
        "{global:#?}"
    );
    // Every route of a fresh project carries only the global stack, so a
    // `--route` filter that matches nothing must be exit 1 rather than an empty
    // table.
    assert_eq!(
        moso(&["middleware", "--route", "/nope"]).status.code(),
        Some(1)
    );

    // ── moso check ────────────────────────────────────────────────────────
    let listed = moso(&["check", "--list", "--json"]);
    assert!(listed.status.success(), "moso check --list failed");
    let catalogue: serde_json::Value =
        serde_json::from_slice(&listed.stdout).expect("--list emitted JSON");
    assert!(!catalogue["lints"].as_array().expect("lints").is_empty());

    // A freshly generated project is clean, which is the property that makes
    // the command usable as a CI gate at all.
    let checked = moso(&["check", "--json"]);
    let findings: serde_json::Value =
        serde_json::from_slice(&checked.stdout).expect("check emitted JSON");
    assert_eq!(
        findings["findings"].as_array().expect("findings").len(),
        0,
        "a generated project must pass its own lints: {findings:#?}"
    );
    assert!(checked.status.success(), "a clean project is exit 0");

    // Break `.env.example` and the drift lint has to notice.
    let committed = std::fs::read_to_string(target.join(".env.example")).expect(".env.example");
    std::fs::write(
        target.join(".env.example"),
        format!("{committed}\nSHOP__GONE=1\n"),
    )
    .expect("rewrite");
    let drifted = moso(&["check", "--lint", "env_example_drift", "--strict"]);
    assert_eq!(
        drifted.status.code(),
        Some(1),
        "drift at --strict is exit 1"
    );
    std::fs::write(target.join(".env.example"), &committed).expect("restore");

    // `--authz` against a project that does not use the battery is a user
    // error, not a pass. A deny-by-default check that silently succeeds is the
    // worst of the available answers.
    assert_eq!(moso(&["check", "--authz"]).status.code(), Some(1));

    // ── moso jobs and moso authz ──────────────────────────────────────────
    // Neither battery is wired, so each must say so and exit 1 — and, crucially,
    // must do it by *answering*, which is what proves `src/dump.rs` recognises
    // the flag rather than falling through to `serve()`.
    for arguments in [
        vec!["jobs", "list"],
        vec!["jobs", "status"],
        vec!["jobs", "schedules"],
        vec!["jobs", "dlq"],
        vec!["authz", "permissions"],
        vec!["authz", "roles"],
        vec!["auth", "calibrate"],
    ] {
        let output = moso(&arguments);
        assert_eq!(
            output.status.code(),
            Some(1),
            "`moso {}` should be a user error here",
            arguments.join(" ")
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("moso-jobs") || stderr.contains("moso-auth"),
            "`moso {}` must name the battery it needs: {stderr}",
            arguments.join(" ")
        );
    }

    // …and the one that is not a battery question but a measurement has to say
    // which flag would have made it answerable.
    let stderr = String::from_utf8_lossy(&moso(&["auth", "calibrate"]).stderr).into_owned();
    assert!(stderr.contains("moso new --auth"), "{stderr}");

    // `explain` is refused outright in a production profile, whatever else is
    // missing — the check has to come before anything that could leak the model.
    let explained = Command::new(MOSO)
        .args([
            "authz", "explain", "--actor", "usr_1", "--action", "publish",
        ])
        .current_dir(&target)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("MOSO_PROFILE", "production")
        .output()
        .expect("the CLI runs");
    assert_eq!(explained.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&explained.stderr);
    assert!(stderr.contains("--allow-production"), "{stderr}");
}

/// `moso self update` needs no project and never touches the network without
/// `--check`, so unlike everything else below it, it can run every time.
#[test]
fn self_update_reports_the_running_version_offline() {
    let output = Command::new(MOSO)
        .args(["self", "update", "--json"])
        .output()
        .expect("the CLI runs");

    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON document");
    assert_eq!(document["package"], serde_json::json!("moso-cli"));
    assert_eq!(document["checked"], serde_json::json!(false));
    // Not checked means not known. A version invented here would be the one
    // thing this command must never do.
    assert_eq!(document["latest"], serde_json::Value::Null);
    assert_eq!(document["update_available"], serde_json::Value::Null);
    assert!(
        document["version"].as_str().is_some_and(|v| !v.is_empty()),
        "{document}"
    );
    assert_eq!(output.status.code(), Some(0));
}

/// The acceptance test for `moso run`, `moso build`, `moso test` and
/// `moso deploy checklist` against a project that has never been touched by
/// hand.
///
/// Ignored by default, for the reason in the module header: it compiles a whole
/// project, four times over in the worst case.
#[test]
#[ignore = "compiles a whole project; run with `cargo test -p moso-cli -- --ignored`"]
fn run_build_test_and_the_checklist_all_drive_a_generated_project() {
    let scratch = Scratch::new("lifecycle");
    let target = scratch.path().join("shop");
    let output = generate(&target, "shop", &[]);
    assert!(
        output.status.success(),
        "moso new failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let target_dir = scratch.path().join("target");
    let moso = |arguments: &[&str]| {
        Command::new(MOSO)
            .args(arguments)
            .current_dir(&target)
            .env("CARGO_TARGET_DIR", &target_dir)
            .output()
            .expect("the CLI runs")
    };

    // ── moso run ────────────────────────────────────────────────────────────
    //
    // `-- --dump-routes` is what makes this testable: the generated `main`
    // answers it and exits 0, so the whole path — build, spawn with the project
    // root as the working directory, pass the trailing arguments through,
    // forward the exit code — is exercised by a process that terminates.
    let ran = moso(&["run", "--", "--dump-routes"]);
    assert!(
        ran.status.success(),
        "moso run failed: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
    let document: serde_json::Value = serde_json::from_slice(&ran.stdout)
        .expect("the application's stdout reached ours unaltered");
    assert!(document["routes"].as_array().is_some_and(|r| !r.is_empty()));

    // The profile reaches the application: under `production` the docs are not
    // mounted, so the same run under a different profile is a different app.
    let production = moso(&["run", "--profile", "production", "--", "--dump-config"]);
    assert!(production.status.success());
    let document: serde_json::Value =
        serde_json::from_slice(&production.stdout).expect("dump-config emitted JSON");
    assert_eq!(document["profile"], serde_json::json!("production"));

    // ── moso build ──────────────────────────────────────────────────────────
    let built = moso(&["build", "--debug", "--openapi", "--json"]);
    assert!(
        built.status.success(),
        "moso build failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&built.stdout).expect("build emitted one JSON document");
    let binary = document["binary"].as_str().expect("a binary path");
    assert!(Path::new(binary).is_file(), "{binary} was not written");
    assert!(document["bytes"].as_u64().is_some_and(|bytes| bytes > 0));
    let contract = document["openapi"].as_str().expect("an openapi path");
    assert!(Path::new(contract).is_file(), "{contract} was not written");
    // Beside the binary, so an image that copies the artefact gets the contract.
    assert_eq!(
        Path::new(contract).parent(),
        Path::new(binary).parent(),
        "the document must ship next to the binary"
    );

    // ── moso test ───────────────────────────────────────────────────────────
    let tested = moso(&["test", "--json"]);
    assert!(
        tested.status.success(),
        "moso test failed: {}",
        String::from_utf8_lossy(&tested.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&tested.stdout).expect("test emitted one JSON document");
    assert_eq!(document["ok"], serde_json::json!(true));
    let passes = document["passes"].as_array().expect("passes is an array");
    assert_eq!(passes.len(), 2, "the doctests are a pass of their own");
    assert!(
        passes
            .iter()
            .all(|pass| pass["ok"] == serde_json::json!(true))
    );
    // The whole point of the command: it says which suites it did not run.
    assert!(document["skipped_suites"].is_array(), "{document}");

    // ── moso deploy checklist ───────────────────────────────────────────────
    let checked = moso(&["deploy", "checklist", "--json"]);
    let document: serde_json::Value =
        serde_json::from_slice(&checked.stdout).expect("the checklist emitted one JSON document");
    assert_eq!(document["profile"], serde_json::json!("production"));
    let checks = document["checks"].as_array().expect("checks is an array");
    assert!(checks.len() >= 8, "only {} checks ran", checks.len());
    let names: Vec<&str> = checks
        .iter()
        .filter_map(|check| check["name"].as_str())
        .collect();
    for expected in [
        "profile",
        "expose_internal_errors",
        "expose_docs",
        "trusted_proxies",
        "cors",
        ".env",
        "shutdown grace",
        "/healthz, /readyz",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    // A freshly generated project has nothing to fail on, so the gate is green.
    assert_eq!(document["failed"], serde_json::json!(0), "{document}");
    assert_eq!(checked.status.code(), Some(0));

    // Break it the way a real project breaks: commit a `.env`.
    std::fs::write(target.join(".env"), "SHOP__GREETING=hello\n").expect("write .env");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&target)
            .status()
            .is_ok_and(|status| status.success())
    );
    assert!(
        Command::new("git")
            .args(["add", "-f", ".env"])
            .current_dir(&target)
            .status()
            .is_ok_and(|status| status.success())
    );
    let broken = moso(&["deploy", "checklist", "--json"]);
    assert_eq!(
        broken.status.code(),
        Some(1),
        "a tracked .env must fail the gate"
    );
    let document: serde_json::Value =
        serde_json::from_slice(&broken.stdout).expect("the checklist emitted one JSON document");
    assert_eq!(document["ok"], serde_json::json!(false));
}

// ---------------------------------------------------------------------------
// `moso generate workspace`
// ---------------------------------------------------------------------------

/// The split moves the package, leaves the project alone, and refuses twice.
///
/// Not `#[ignore]`d: it moves files and rewrites two manifests, and none of
/// that needs a compiler. What it deliberately does not do is build the result.
#[test]
fn generation_splits_a_project_into_a_workspace() {
    let scratch = Scratch::new("workspace");
    let target = scratch.path().join("shop");
    let created = generate(&target, "shop", &[]);
    assert!(created.status.success(), "moso new failed");

    let split = Command::new(MOSO)
        .args(["generate", "workspace", "--json"])
        .current_dir(&target)
        .output()
        .expect("the CLI runs");
    assert_eq!(
        split.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&split.stderr)
    );

    let document: serde_json::Value =
        serde_json::from_slice(&split.stdout).expect("stdout is one JSON document");
    assert_eq!(document["ok"], serde_json::json!(true));
    assert_eq!(document["package"], serde_json::json!("shop"));

    // The package moved …
    assert!(target.join("crates/shop/Cargo.toml").is_file());
    assert!(target.join("crates/shop/src/lib.rs").is_file());
    assert!(target.join("crates/shop/src/main.rs").is_file());
    assert!(target.join("crates/shop/tests/api.rs").is_file());
    assert!(!target.join("src").exists(), "src/ was left behind");

    // … and everything the *project* has stayed where every tool looks for it.
    for kept in ["README.md", ".env.example", ".gitignore", "Dockerfile"] {
        assert!(target.join(kept).is_file(), "{kept} moved");
    }
    assert!(target.join(".cargo/config.toml").is_file());

    let root = std::fs::read_to_string(target.join("Cargo.toml")).expect("root manifest");
    let parsed: toml::Value = toml::from_str(&root).expect("the root manifest is valid TOML");
    assert_eq!(
        parsed["workspace"]["members"],
        toml::Value::Array(vec![toml::Value::String("crates/*".to_owned())])
    );
    assert!(
        parsed.get("profile").is_some(),
        "the profiles must be lifted to the root, where cargo honours them: {root}"
    );

    let moved = std::fs::read_to_string(target.join("crates/shop/Cargo.toml")).expect("manifest");
    let parsed: toml::Value = toml::from_str(&moved).expect("the package manifest is valid TOML");
    assert_eq!(
        parsed["package"]["name"],
        toml::Value::String("shop".into())
    );
    assert!(parsed.get("profile").is_none(), "{moved}");
    assert!(parsed.get("workspace").is_none(), "{moved}");
    assert!(
        moved.contains("Moso does not pick your runtime for you"),
        "the comments in the manifest are the manifest: {moved}"
    );

    // Twice is refused, rather than nesting `crates/shop/crates/shop`.
    let again = Command::new(MOSO)
        .args(["generate", "workspace"])
        .current_dir(&target)
        .output()
        .expect("the CLI runs");
    assert_eq!(again.status.code(), Some(1));
    assert!(!target.join("crates/shop/crates").exists());
}

// ---------------------------------------------------------------------------
// `moso config --generate-secret`
// ---------------------------------------------------------------------------

/// The secret goes to stdout alone, and the warning that comes with it does
/// not — so `moso config --generate-secret > key` writes a key and nothing else.
#[test]
fn a_generated_secret_is_the_only_thing_on_standard_output() {
    // Deliberately not inside a project: entropy is not configuration.
    let scratch = Scratch::new("secret");
    let secret = |arguments: &[&str]| {
        let output = Command::new(MOSO)
            .args(arguments)
            .current_dir(scratch.path())
            .output()
            .expect("the CLI runs");
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        (
            String::from_utf8(output.stdout).expect("utf-8"),
            String::from_utf8(output.stderr).expect("utf-8"),
        )
    };

    let (stdout, stderr) = secret(&["config", "--generate-secret"]);
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
    let value = stdout.trim();
    assert_eq!(value.len(), 44, "32 bytes is 44 base64 characters: {value}");
    assert!(
        value
            .chars()
            .all(|glyph| glyph.is_ascii_alphanumeric() || "+/=".contains(glyph)),
        "{value}"
    );
    assert!(
        stderr.contains("keep it out of git"),
        "the reminder belongs on stderr: {stderr:?}"
    );

    let (second, _) = secret(&["config", "--generate-secret"]);
    assert_ne!(stdout, second, "two secrets must not be the same secret");

    let (hex, _) = secret(&[
        "config",
        "--generate-secret",
        "--format",
        "hex",
        "--bytes",
        "16",
    ]);
    let hex = hex.trim();
    assert_eq!(hex.len(), 32);
    assert!(hex.chars().all(|glyph| glyph.is_ascii_hexdigit()), "{hex}");

    // A secret that can be redirected into the repository is a secret in the
    // repository, so the flag that would do it is refused outright.
    let refused = Command::new(MOSO)
        .args(["config", "--generate-secret", "--out", "key.txt"])
        .current_dir(scratch.path())
        .output()
        .expect("the CLI runs");
    assert_eq!(refused.status.code(), Some(2), "usage error");
    assert!(!scratch.path().join("key.txt").exists());
}
