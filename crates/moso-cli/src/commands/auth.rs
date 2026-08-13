//! `moso auth calibrate` — what this machine can afford to spend on a password.
//!
//! ```text
//!   one hash takes 243 ms here, against a 250 ms target
//!
//!   PARAMETER    VALUE   NOTE
//!   memory_kib   65536   64 MiB, 3.4× OWASP's minimum
//!   iterations   3
//!   parallelism  1
//! ```
//!
//! # Why this runs the application instead of answering from a table
//!
//! Argon2id's cost is a property of the hardware the hash will run on. The
//! parameters that take 250 ms on a development laptop take three times that in
//! a container with half a CPU, and half as long on the machine somebody buys
//! next year — so a constant compiled into this binary would be wrong on every
//! machine except the one it was measured on. The measurement has to happen
//! inside the process that will do the hashing, which is what the existing
//! `--dump-auth` protocol reaches: this command builds the project, runs its
//! binary, and renders the answer.
//!
//! That also makes the answer honest about *which* machine. Run it on the
//! deployment target, or in the image, and the number means something; run it on
//! a laptop and configure a container from it and you have measured the wrong
//! computer. The command cannot tell the difference, so it says nothing it
//! cannot know and leaves the decision named in the documentation.
//!
//! It *can* tell one thing apart, and does: a debug build. An unoptimised
//! argon2 is several times slower, so it reaches the target with parameters
//! several times weaker than the release binary needs — which is a downgrade
//! arriving as a recommendation, the exact failure the floor check exists to
//! prevent. `--release` was not passed is a warning on the human output and a
//! `"release": false` in the JSON.
//!
//! # Why it refuses to print a downgrade
//!
//! A calibration that recommends weaker parameters than the ones already in
//! force is worse than no calibration: it is a plausible-looking instruction to
//! make an application less safe, arriving with a tool's authority behind it. So
//! anything below `HashParams::OWASP_MINIMUM` is refused, and refused loudly.
//!
//! The floor **travels with the answer** rather than being kept here. OWASP's
//! minimum has exactly one home — `HashParams::OWASP_MINIMUM`, in `moso-auth` —
//! and this crate deliberately does not depend on that battery (it would pull a
//! database driver into the CLI). A second copy of the three numbers here would
//! be a second thing to keep in step, and the day they diverged neither would be
//! wrong on its face. So `src/dump.rs` reports the floor it read from the
//! constant, and this command checks the measurement against it.

use serde_json::Value;

use crate::cli::{AuthCalibrateArgs, AuthCommand};
use crate::exit::{CliError, Outcome};
use crate::project::{Battery, Project};
use crate::ui::{Level, Ui};

/// Dispatch one `moso auth` subcommand.
///
/// # Errors
/// Anything the dump protocol can fail with; a project that does not use
/// `moso-auth`, which is a user error naming what to add; and a measurement
/// that came back below OWASP's minimum.
pub fn run(ui: &Ui, command: &AuthCommand) -> Outcome<()> {
    match command {
        AuthCommand::Calibrate(args) => calibrate(ui, args),
    }
}

/// One argon2id parameter set.
///
/// A copy of the three numbers and nothing else: this crate does not depend on
/// `moso-auth`, so `HashParams` itself is not nameable here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Params {
    /// Memory cost, in kibibytes.
    memory_kib: u32,
    /// Time cost: how many passes.
    iterations: u32,
    /// How many lanes.
    parallelism: u32,
}

impl Params {
    /// Read one out of the application's answer.
    fn from_json(value: Option<&Value>) -> Option<Self> {
        let value = value?;
        Some(Self {
            memory_kib: field(value, "memory_kib")?,
            iterations: field(value, "iterations")?,
            parallelism: field(value, "parallelism")?,
        })
    }

    /// The dimensions in which these are weaker than `floor`.
    ///
    /// All of them, not the first: an operator fixing a configuration wants the
    /// whole list, and "weaker in any dimension counts as weaker" is the rule
    /// `PasswordHash::needs_rehash` uses too.
    fn weaker_than(self, floor: Self) -> Vec<String> {
        let mut weak = Vec::new();
        for (name, mine, theirs) in [
            ("memory_kib", self.memory_kib, floor.memory_kib),
            ("iterations", self.iterations, floor.iterations),
            ("parallelism", self.parallelism, floor.parallelism),
        ] {
            if mine < theirs {
                weak.push(format!("{name} {mine} < {theirs}"));
            }
        }
        weak
    }

    /// How much stronger than `floor` the memory cost is, as a note.
    fn note(self, floor: Self) -> String {
        let mebibytes = f64::from(self.memory_kib) / 1024.0;
        if self.memory_kib == floor.memory_kib {
            return format!("{mebibytes:.0} MiB, OWASP's minimum");
        }
        let ratio = f64::from(self.memory_kib) / f64::from(floor.memory_kib.max(1));
        format!("{mebibytes:.0} MiB, {ratio:.1}× OWASP's minimum")
    }
}

/// `moso auth calibrate`.
fn calibrate(ui: &Ui, args: &AuthCalibrateArgs) -> Outcome<()> {
    let request = serde_json::json!({
        "action": "calibrate",
        "target_ms": args.target_ms,
    });

    let project = Project::discover(args.app.manifest_path.as_deref())?;
    project.require_moso()?;

    let answer = project.battery(&args.app, &Battery::Auth(request.to_string()))?;
    let document: Value = serde_json::from_str(&answer).map_err(|error| {
        CliError::user(format!(
            "`{}` answered `--dump-auth` with something that is not JSON: {error}",
            project.name
        ))
        .with_help("src/dump.rs must print exactly one JSON document to stdout")
    })?;

    let (params, floor) = checked(&document)?;

    if ui.is_json() {
        // The application's own document, plus the one thing only this side
        // knows: whether the binary that was measured was optimised.
        let mut document = document;
        document["release"] = Value::Bool(args.app.release);
        ui.emit_json(&document);
        return Ok(());
    }

    report(ui, args, &document, params, floor);
    Ok(())
}

/// Pull the measurement out of the answer, refusing anything unusable.
///
/// Three refusals, and they are different facts: the battery is not wired, the
/// answer does not carry the numbers, and the numbers are a downgrade.
fn checked(document: &Value) -> Outcome<(Params, Params)> {
    if document.get("available").and_then(Value::as_bool) != Some(true) {
        let reason = document
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("this project does not use moso-auth, so there is nothing to calibrate");
        let help = document.get("help").and_then(Value::as_str).unwrap_or(
            "create the project with `moso new --auth`, or add the `auth` feature and \
             implement `fn auth` in src/dump.rs",
        );
        return Err(CliError::user(reason.to_owned()).with_help(help.to_owned()));
    }

    let params = Params::from_json(document.get("params")).ok_or_else(|| {
        CliError::user("the application's answer carries no argon2id parameters").with_help(
            "`fn auth` in src/dump.rs must answer with \
             `\"params\": {\"memory_kib\": .., \"iterations\": .., \"parallelism\": ..}`",
        )
    })?;

    // The floor is not optional and is not defaulted here. A missing one would
    // have to be filled in from a second copy of OWASP's minimum living in this
    // crate, which is exactly the duplication the protocol exists to avoid.
    let floor = Params::from_json(document.get("floor")).ok_or_else(|| {
        CliError::user("the application's answer carries no minimum to check against").with_help(
            "`fn auth` in src/dump.rs must answer with \
             `\"floor\": { .. HashParams::OWASP_MINIMUM .. }`",
        )
    })?;

    let weak = params.weaker_than(floor);
    if !weak.is_empty() {
        return Err(CliError::user(format!(
            "the measurement came back below OWASP's minimum ({}), so it is not printed",
            weak.join(", ")
        ))
        .with_help(
            "keep the parameters you have: being slow hardware is not a reason to hash \
             weakly, and `install_params` raises anything below the floor back to it",
        ));
    }

    Ok((params, floor))
}

/// Print the measurement and the lines to paste.
fn report(ui: &Ui, args: &AuthCalibrateArgs, document: &Value, params: Params, floor: Params) {
    ui.blank();
    match document.get("measured_ms").and_then(Value::as_u64) {
        Some(measured) => ui.status(
            Level::Ok,
            &format!("one hash takes {measured} ms here"),
            &format!("(target {} ms)", args.target_ms),
        ),
        // The search ran and the confirming hash did not. Say what is known.
        None => ui.status(
            Level::Ok,
            &format!("calibrated for a {} ms hash", args.target_ms),
            "(the confirming hash did not run)",
        ),
    }

    ui.blank();
    ui.table(
        &["PARAMETER", "VALUE", "NOTE"],
        &[
            vec![
                "memory_kib".to_owned(),
                params.memory_kib.to_string(),
                params.note(floor),
            ],
            vec![
                "iterations".to_owned(),
                params.iterations.to_string(),
                String::new(),
            ],
            vec![
                "parallelism".to_owned(),
                params.parallelism.to_string(),
                String::new(),
            ],
        ],
    );

    // The application's own keys, because only it knows what it reads them
    // from. A generic `AUTH__HASH_MEMORY_KIB` printed here would be advice
    // about a project this is not.
    let keys = strings(document, "config");
    if !keys.is_empty() {
        ui.blank();
        ui.line("  paste into .env, or your platform's configuration:");
        ui.blank();
        for key in &keys {
            ui.line(&format!("    {key}"));
        }
    }

    ui.blank();
    ui.line("  or, where you build the AuthConfig:");
    ui.blank();
    ui.line(&format!(
        "    config.hash_params = Some(HashParams::new({}, {}, {}));",
        params.memory_kib, params.iterations, params.parallelism
    ));
    ui.blank();
    ui.line(&ui.dim("      measured on this machine; run it on the one that will serve logins"));

    // The one caveat this side knows and the application cannot: an
    // unoptimised argon2 is several times slower, so a debug build hits the
    // target with parameters that are several times too weak for the release
    // binary that will actually run them. Saying so is the difference between a
    // number and a wrong number.
    if !args.app.release {
        ui.blank();
        ui.status(
            Level::Warn,
            "this measured a debug build",
            "(an optimised argon2 is several times faster)",
        );
        ui.fix("moso auth calibrate --release");
    }
    ui.blank();
}

/// A `u32` field, if it is there and fits.
fn field(value: &Value, key: &str) -> Option<u32> {
    u32::try_from(value.get(key)?.as_u64()?).ok()
}

/// A string-array field, or an empty vector.
fn strings(document: &Value, key: &str) -> Vec<String> {
    document
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OWASP's minimum, as `src/dump.rs` reports it.
    const FLOOR: &str = r#"{"memory_kib":19456,"iterations":2,"parallelism":1}"#;

    fn answer(params: &str) -> Value {
        serde_json::from_str(&format!(
            r#"{{"available":true,"params":{params},"floor":{FLOOR},
                 "measured_ms":243,
                 "config":["SHOP__HASH_MEMORY_KIB=65536"]}}"#
        ))
        .expect("valid JSON")
    }

    #[test]
    fn a_calibration_above_the_floor_is_accepted_with_the_floor_it_was_checked_against() {
        let document = answer(r#"{"memory_kib":65536,"iterations":3,"parallelism":1}"#);
        let (params, floor) = checked(&document).expect("accepted");
        assert_eq!(params.memory_kib, 65_536);
        assert_eq!(params.iterations, 3);
        assert_eq!(floor.memory_kib, 19_456);
    }

    #[test]
    fn the_floor_itself_is_not_a_downgrade() {
        let document = answer(FLOOR);
        let (params, floor) = checked(&document).expect("accepted");
        assert_eq!(params, floor);
        assert!(params.note(floor).contains("OWASP"));
    }

    #[test]
    fn anything_weaker_in_any_dimension_is_refused_and_names_every_one() {
        let document = answer(r#"{"memory_kib":8192,"iterations":1,"parallelism":1}"#);
        let error = checked(&document).expect_err("refused");
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert!(error.message.contains("memory_kib 8192 < 19456"), "{error}");
        assert!(error.message.contains("iterations 1 < 2"), "{error}");
        // A downgrade must never reach the terminal as a recommendation.
        assert!(!error.message.contains("HashParams::new"), "{error}");
    }

    #[test]
    fn a_project_without_the_battery_is_a_user_error_carrying_the_applications_own_help() {
        let document: Value = serde_json::from_str(
            r#"{"available":false,"reason":"no moso-auth here","help":"moso new --auth"}"#,
        )
        .expect("valid JSON");

        let error = checked(&document).expect_err("refused");
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert_eq!(error.message, "no moso-auth here");
        assert_eq!(error.help.as_deref(), Some("moso new --auth"));
    }

    #[test]
    fn an_answer_with_no_floor_is_refused_rather_than_checked_against_a_local_copy() {
        let document: Value = serde_json::from_str(
            r#"{"available":true,"params":{"memory_kib":65536,"iterations":3,"parallelism":1}}"#,
        )
        .expect("valid JSON");

        let error = checked(&document).expect_err("refused");
        assert!(error.message.contains("no minimum"), "{error}");
    }

    #[test]
    fn an_answer_with_no_parameters_names_the_shape_it_wanted() {
        let document: Value =
            serde_json::from_str(r#"{"available":true,"floor":{}}"#).expect("valid JSON");
        let error = checked(&document).expect_err("refused");
        assert!(error.message.contains("no argon2id parameters"), "{error}");
    }

    #[test]
    fn the_configuration_lines_come_from_the_application_and_are_not_invented() {
        let document = answer(r#"{"memory_kib":65536,"iterations":3,"parallelism":1}"#);
        assert_eq!(
            strings(&document, "config"),
            ["SHOP__HASH_MEMORY_KIB=65536"]
        );
        // An application that reports none gets none printed, rather than a
        // guess at what it calls its own keys.
        assert!(strings(&serde_json::json!({}), "config").is_empty());
    }
}
