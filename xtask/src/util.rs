//! The four things every subcommand needs: an error, a process runner, a
//! summary statistic, and a way to say what is happening.
//!
//! Nothing here is clever. It exists so that the seven subcommands do not each
//! grow their own version of it, and so that the parts of `xtask` that are
//! worth testing — percentiles, parsing, rewriting — are separable from the
//! parts that shell out.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

/// The result type every `xtask` function returns.
///
/// ```
/// use xtask::util::{Error, Result};
///
/// fn only_even(n: u32) -> Result<u32> {
///     if n % 2 == 0 { Ok(n) } else { Err(Error::new(format!("{n} is odd"))) }
/// }
///
/// assert_eq!(only_even(4).unwrap(), 4);
/// assert_eq!(only_even(5).unwrap_err().to_string(), "5 is odd");
/// ```
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// A flat, human-readable error.
///
/// `xtask` is a command-line tool whose errors are printed once and never
/// matched on, so an error type is a message and the chain is spelled out in
/// the message. That is the whole design.
///
/// ```
/// use xtask::util::Error;
///
/// let error = Error::new("cargo metadata failed").with_context("check-deps");
/// assert_eq!(error.to_string(), "check-deps: cargo metadata failed");
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    /// Builds an error from anything that can become a `String`.
    ///
    /// ```
    /// use xtask::util::Error;
    ///
    /// assert_eq!(Error::new("boom").to_string(), "boom");
    /// ```
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Prefixes the message with the operation that failed.
    ///
    /// ```
    /// use xtask::util::Error;
    ///
    /// let error = Error::new("no such file").with_context("reading the baseline");
    /// assert_eq!(error.to_string(), "reading the baseline: no such file");
    /// ```
    #[must_use]
    pub fn with_context(self, context: impl fmt::Display) -> Self {
        Self::new(format!("{context}: {}", self.message))
    }

    /// The message, without the trailing newline a process would add.
    ///
    /// ```
    /// use xtask::util::Error;
    ///
    /// assert_eq!(Error::new("nope").message(), "nope");
    /// ```
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::new(format!("malformed JSON: {error}"))
    }
}

impl From<toml::de::Error> for Error {
    fn from(error: toml::de::Error) -> Self {
        Self::new(format!("malformed TOML: {error}"))
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Shorthand for `Err(Error::new(format!(..)))`.
///
/// ```
/// use xtask::{bail, util::Result};
///
/// fn check(n: u32) -> Result<()> {
///     if n > 3 {
///         bail!("{n} is more than {}", 3);
///     }
///     Ok(())
/// }
///
/// assert_eq!(check(9).unwrap_err().to_string(), "9 is more than 3");
/// ```
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return ::core::result::Result::Err($crate::util::Error::new(::std::format!($($arg)*)))
    };
}

/// The absolute path of the workspace root.
///
/// Resolved from `MOSO_WORKSPACE_ROOT` if set, then from the directory this
/// crate was compiled in, then by walking up from the current directory looking
/// for a manifest with a `[workspace]` table. The last fallback is what makes
/// the binary usable after the checkout moves.
///
/// ```
/// let root = xtask::util::workspace_root().expect("a workspace");
/// assert!(root.join("Cargo.toml").is_file());
/// ```
pub fn workspace_root() -> Result<PathBuf> {
    if let Ok(from_env) = std::env::var("MOSO_WORKSPACE_ROOT") {
        let candidate = PathBuf::from(from_env);
        if is_workspace_root(&candidate) {
            return Ok(candidate);
        }
        return Err(Error::new(format!(
            "MOSO_WORKSPACE_ROOT points at {}, which has no Cargo.toml with a [workspace] table",
            candidate.display()
        )));
    }

    let compiled_in = Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(parent) = compiled_in.parent()
        && is_workspace_root(parent)
    {
        return Ok(parent.to_path_buf());
    }

    let mut here = std::env::current_dir()?;
    loop {
        if is_workspace_root(&here) {
            return Ok(here);
        }
        if !here.pop() {
            break;
        }
    }

    Err(Error::new(
        "cannot find the workspace root; run xtask from inside the checkout or set MOSO_WORKSPACE_ROOT",
    ))
}

fn is_workspace_root(dir: &Path) -> bool {
    let manifest = dir.join("Cargo.toml");
    std::fs::read_to_string(manifest)
        .map(|text| text.lines().any(|line| line.trim_end() == "[workspace]"))
        .unwrap_or(false)
}

/// What a finished process said.
///
/// ```
/// use xtask::util::Output;
///
/// let output = Output { code: 0, stdout: "hi\n".into(), stderr: String::new() };
/// assert!(output.ok());
/// ```
#[derive(Clone, Debug)]
pub struct Output {
    /// The exit code, or `-1` when the process was killed by a signal.
    pub code: i32,
    /// Everything the process wrote to standard output, lossily decoded.
    pub stdout: String,
    /// Everything the process wrote to standard error, lossily decoded.
    pub stderr: String,
}

impl Output {
    /// Whether the process exited zero.
    ///
    /// ```
    /// use xtask::util::Output;
    ///
    /// let output = Output { code: 101, stdout: String::new(), stderr: "boom".into() };
    /// assert!(!output.ok());
    /// ```
    #[must_use]
    pub fn ok(&self) -> bool {
        self.code == 0
    }

    /// The last `n` non-empty lines of standard error, for error messages.
    ///
    /// ```
    /// use xtask::util::Output;
    ///
    /// let output = Output { code: 1, stdout: String::new(), stderr: "a\n\nb\nc\n".into() };
    /// assert_eq!(output.stderr_tail(2), "b\nc");
    /// ```
    #[must_use]
    pub fn stderr_tail(&self, n: usize) -> String {
        let lines: Vec<&str> = self
            .stderr
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }
}

/// A process to run, with the environment `xtask` always wants.
///
/// Colour is off and `CARGO_TERM_PROGRESS_WHEN` is `never` on every cargo
/// invocation, because these outputs are parsed and compared, not read.
///
/// ```no_run
/// use xtask::util::Cmd;
///
/// let output = Cmd::new("cargo").args(["--version"]).capture().unwrap();
/// assert!(output.stdout.starts_with("cargo "));
/// ```
#[derive(Clone, Debug)]
pub struct Cmd {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<PathBuf>,
}

impl Cmd {
    /// Starts building an invocation of `program`.
    ///
    /// ```
    /// use xtask::util::Cmd;
    ///
    /// assert_eq!(Cmd::new("cargo").rendered(), "cargo");
    /// ```
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
        }
    }

    /// A cargo invocation, honouring `CARGO` so a `+toolchain` shim is kept.
    ///
    /// ```
    /// use xtask::util::Cmd;
    ///
    /// let cmd = Cmd::cargo().args(["check"]);
    /// assert!(cmd.rendered().ends_with("check"));
    /// ```
    #[must_use]
    pub fn cargo() -> Self {
        Self::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
    }

    /// Appends one argument.
    ///
    /// ```
    /// use xtask::util::Cmd;
    ///
    /// assert_eq!(Cmd::new("git").arg("status").rendered(), "git status");
    /// ```
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends several arguments.
    ///
    /// ```
    /// use xtask::util::Cmd;
    ///
    /// let cmd = Cmd::new("git").args(["tag", "-l"]);
    /// assert_eq!(cmd.rendered(), "git tag -l");
    /// ```
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets one environment variable.
    ///
    /// ```
    /// use xtask::util::Cmd;
    ///
    /// let cmd = Cmd::new("cargo").env("RUSTC_BOOTSTRAP", "1");
    /// assert_eq!(cmd.rendered(), "RUSTC_BOOTSTRAP=1 cargo");
    /// ```
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Runs the process in `dir`.
    ///
    /// ```
    /// use xtask::util::Cmd;
    ///
    /// let cmd = Cmd::new("ls").cwd("/tmp");
    /// assert_eq!(cmd.rendered(), "ls");
    /// ```
    #[must_use]
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// The command as a copy-pasteable line, for error messages and reports.
    ///
    /// An absolute program path is shortened to its file name: `$CARGO` is an
    /// absolute path inside the active toolchain, and a benchmark baseline that
    /// records it would differ on every machine for no reason. An argument with
    /// a space in it is quoted, so that pasting the line runs the command that
    /// was run rather than a different one.
    ///
    /// ```
    /// use xtask::util::Cmd;
    ///
    /// let cmd = Cmd::cargo().args(["build", "-p", "example-crud"]);
    /// assert!(cmd.rendered().contains("build -p example-crud"));
    /// assert!(!cmd.rendered().contains('/'), "no machine-specific path: {}", cmd.rendered());
    ///
    /// let tag = Cmd::new("git").args(["tag", "-m", "moso v0.1.0"]);
    /// assert_eq!(tag.rendered(), "git tag -m 'moso v0.1.0'");
    /// ```
    #[must_use]
    pub fn rendered(&self) -> String {
        let mut out = String::new();
        for (key, value) in &self.env {
            out.push_str(key);
            out.push('=');
            out.push_str(value);
            out.push(' ');
        }
        let program = Path::new(&self.program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&self.program);
        out.push_str(program);
        for arg in &self.args {
            out.push(' ');
            if arg.contains(char::is_whitespace) {
                out.push('\'');
                out.push_str(arg);
                out.push('\'');
            } else {
                out.push_str(arg);
            }
        }
        out
    }

    fn build(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        if let Some(dir) = &self.cwd {
            command.current_dir(dir);
        }
        command
    }

    /// Runs the process, capturing both streams.
    ///
    /// ```no_run
    /// use xtask::util::Cmd;
    ///
    /// let output = Cmd::new("git").args(["rev-parse", "HEAD"]).capture().unwrap();
    /// assert_eq!(output.stdout.trim().len(), 40);
    /// ```
    pub fn capture(&self) -> Result<Output> {
        let out = self
            .build()
            .stdin(Stdio::null())
            .output()
            .map_err(|error| Error::new(format!("cannot run `{}`: {error}", self.rendered())))?;
        Ok(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Runs the process, capturing it, and fails when it exits non-zero.
    ///
    /// ```no_run
    /// use xtask::util::Cmd;
    ///
    /// let output = Cmd::cargo().args(["metadata", "--format-version", "1"]).run()?;
    /// assert!(output.stdout.starts_with('{'));
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn run(&self) -> Result<Output> {
        let output = self.capture()?;
        if output.ok() {
            return Ok(output);
        }
        Err(Error::new(format!(
            "`{}` exited {}\n{}",
            self.rendered(),
            output.code,
            indent(&output.stderr_tail(20))
        )))
    }

    /// Runs the process with both streams inherited, and reports the exit code.
    ///
    /// Used by `xtask ci`, where the point is that a contributor sees exactly
    /// what CI would print.
    ///
    /// ```no_run
    /// use xtask::util::Cmd;
    ///
    /// let code = Cmd::cargo().args(["fmt", "--all", "--check"]).stream()?;
    /// assert_eq!(code, 0);
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn stream(&self) -> Result<i32> {
        let status = self
            .build()
            .stdin(Stdio::null())
            .status()
            .map_err(|error| Error::new(format!("cannot run `{}`: {error}", self.rendered())))?;
        Ok(status.code().unwrap_or(-1))
    }

    /// Runs the process, capturing it, and returns how long it took in
    /// milliseconds along with the output.
    ///
    /// The clock starts before the fork and stops after the wait, so it
    /// includes cargo's own start-up. That is deliberate: it is what the
    /// developer waits for.
    ///
    /// ```no_run
    /// use xtask::util::Cmd;
    ///
    /// let (millis, output) = Cmd::cargo().args(["--version"]).timed()?;
    /// assert!(output.ok() && millis >= 0.0);
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn timed(&self) -> Result<(f64, Output)> {
        let started = Instant::now();
        let output = self.capture()?;
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        // Rounded to a hundredth of a millisecond: the seventeen significant
        // figures a bare f64 serialises to make a committed baseline unreadable
        // as a diff, and no build time is meaningful past two decimals.
        Ok(((elapsed * 100.0).round() / 100.0, output))
    }
}

/// Indents every line by four spaces, for nesting a process's output under a
/// message of ours.
///
/// ```
/// assert_eq!(xtask::util::indent("a\nb"), "    a\n    b");
/// ```
#[must_use]
pub fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Percentiles and spread over a set of timings.
///
/// Percentiles are nearest-rank on the sorted samples, which is the only
/// definition that never invents a value that was not measured — important
/// when the number the gate compares is printed in a report.
///
/// ```
/// use xtask::util::Stats;
///
/// let stats = Stats::new(&[100.0, 104.0, 96.0, 102.0, 98.0]).expect("five samples");
/// assert_eq!(stats.runs, 5);
/// assert_eq!(stats.p50_ms, 100.0);
/// assert_eq!(stats.p95_ms, 104.0);
/// assert_eq!(stats.min_ms, 96.0);
/// // The worst run is 4 ms from the median, so the spread is 4%.
/// assert!((stats.deviation_pct - 4.0).abs() < 1e-9);
/// assert!(stats.reproducible(5.0));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    /// How many samples were taken.
    pub runs: usize,
    /// The fastest run, in milliseconds.
    pub min_ms: f64,
    /// The slowest run, in milliseconds.
    pub max_ms: f64,
    /// The arithmetic mean, in milliseconds.
    pub mean_ms: f64,
    /// The median (nearest-rank 50th percentile), in milliseconds.
    pub p50_ms: f64,
    /// The nearest-rank 95th percentile, in milliseconds.
    pub p95_ms: f64,
    /// The population standard deviation, in milliseconds.
    pub stdev_ms: f64,
    /// The largest distance of any single run from the median, as a percentage
    /// of the median. This is the number the "reproducible within ±5%"
    /// criterion in `docs/04-devex/42-compile-times.md` is checked against.
    pub deviation_pct: f64,
}

impl Stats {
    /// Summarises a non-empty set of millisecond timings.
    ///
    /// ```
    /// use xtask::util::Stats;
    ///
    /// assert!(Stats::new(&[]).is_none());
    /// assert_eq!(Stats::new(&[7.0]).unwrap().p95_ms, 7.0);
    /// ```
    #[must_use]
    pub fn new(samples: &[f64]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        let runs = sorted.len();
        let sum: f64 = sorted.iter().sum();
        let mean_ms = sum / runs as f64;
        let variance = sorted
            .iter()
            .map(|sample| (sample - mean_ms) * (sample - mean_ms))
            .sum::<f64>()
            / runs as f64;
        let p50_ms = percentile(&sorted, 50.0);
        let deviation_pct = if p50_ms > 0.0 {
            sorted
                .iter()
                .map(|sample| (sample - p50_ms).abs() / p50_ms * 100.0)
                .fold(0.0_f64, f64::max)
        } else {
            0.0
        };
        Some(Self {
            runs,
            min_ms: sorted[0],
            max_ms: sorted[runs - 1],
            mean_ms,
            p50_ms,
            p95_ms: percentile(&sorted, 95.0),
            stdev_ms: variance.sqrt(),
            deviation_pct,
        })
    }

    /// Whether every run landed within `tolerance_pct` of the median.
    ///
    /// ```
    /// use xtask::util::Stats;
    ///
    /// let noisy = Stats::new(&[100.0, 130.0, 101.0]).unwrap();
    /// assert!(!noisy.reproducible(5.0));
    /// ```
    #[must_use]
    pub fn reproducible(&self, tolerance_pct: f64) -> bool {
        self.deviation_pct <= tolerance_pct
    }
}

/// The nearest-rank percentile of an already-sorted slice.
///
/// ```
/// use xtask::util::percentile;
///
/// let sorted = [1.0, 2.0, 3.0, 4.0];
/// assert_eq!(percentile(&sorted, 50.0), 2.0);
/// assert_eq!(percentile(&sorted, 95.0), 4.0);
/// assert_eq!(percentile(&sorted, 0.0), 1.0);
/// ```
#[must_use]
pub fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (pct / 100.0 * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

/// Formats a millisecond duration the way a build time is read.
///
/// ```
/// use xtask::util::fmt_ms;
///
/// assert_eq!(fmt_ms(842.0), "0.84 s");
/// assert_eq!(fmt_ms(61_500.0), "61.5 s");
/// ```
#[must_use]
pub fn fmt_ms(millis: f64) -> String {
    let seconds = millis / 1000.0;
    if seconds < 10.0 {
        format!("{seconds:.2} s")
    } else {
        format!("{seconds:.1} s")
    }
}

/// The civil date, as `YYYY-MM-DD`, for a count of seconds since the Unix
/// epoch.
///
/// Hand-rolled because `xtask` will not pull `chrono` in for one line, and
/// because a release's changelog date must be reproducible in a test.
///
/// ```
/// use xtask::util::civil_date;
///
/// assert_eq!(civil_date(0), "1970-01-01");
/// assert_eq!(civil_date(1_769_731_200), "2026-01-30");
/// ```
#[must_use]
pub fn civil_date(unix_seconds: i64) -> String {
    // Howard Hinnant's `civil_from_days`, which is exact for the whole range
    // and has no table.
    let days = unix_seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}")
}

/// Today's date as `YYYY-MM-DD`, from the system clock.
///
/// ```
/// let today = xtask::util::today();
/// assert_eq!(today.len(), 10);
/// assert_eq!(today.as_bytes()[4], b'-');
/// ```
#[must_use]
pub fn today() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    civil_date(seconds)
}

/// Terminal output. Every subcommand prints through this so that a run of
/// `xtask ci` reads as one document.
pub mod ui {
    /// Prints a section heading.
    ///
    /// ```
    /// xtask::util::ui::headline("check-sealed");
    /// ```
    pub fn headline(text: &str) {
        println!("\n== {text}");
    }

    /// Prints a step that passed.
    ///
    /// ```
    /// xtask::util::ui::ok("21 public traits carry a diagnostic");
    /// ```
    pub fn ok(text: &str) {
        println!("  ok    {text}");
    }

    /// Prints a step that failed.
    ///
    /// ```
    /// xtask::util::ui::fail("moso_core::router::DynGuard has no on_unimplemented");
    /// ```
    pub fn fail(text: &str) {
        println!("  FAIL  {text}");
    }

    /// Prints something the reader should notice but which does not fail the
    /// gate — the shape every "the crate does not exist yet" message takes.
    ///
    /// ```
    /// xtask::util::ui::warn("moso-sql is not in the workspace yet; skipping");
    /// ```
    pub fn warn(text: &str) {
        println!("  warn  {text}");
    }

    /// Prints an indented detail line under a step.
    ///
    /// ```
    /// xtask::util::ui::note("crates/moso-core/src/router.rs:651");
    /// ```
    pub fn note(text: &str) {
        println!("        {text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_never_invent_a_value() {
        let sorted = [10.0, 20.0, 30.0, 40.0, 50.0];
        for pct in [0.0, 25.0, 50.0, 75.0, 95.0, 100.0] {
            assert!(sorted.contains(&percentile(&sorted, pct)), "{pct}");
        }
    }

    #[test]
    fn a_single_sample_is_every_percentile() {
        let stats = Stats::new(&[42.0]).expect("one sample");
        assert_eq!(stats.p50_ms, 42.0);
        assert_eq!(stats.p95_ms, 42.0);
        assert_eq!(stats.stdev_ms, 0.0);
        assert_eq!(stats.deviation_pct, 0.0);
        assert!(stats.reproducible(0.0));
    }

    #[test]
    fn deviation_is_measured_from_the_median_not_the_mean() {
        // The mean is dragged up by the outlier; the median is not, which is
        // why the gate uses the median.
        let stats = Stats::new(&[100.0, 100.0, 100.0, 100.0, 200.0]).expect("five");
        assert_eq!(stats.p50_ms, 100.0);
        assert!((stats.deviation_pct - 100.0).abs() < 1e-9);
        assert!(!stats.reproducible(5.0));
    }

    #[test]
    fn empty_samples_have_no_statistics() {
        assert!(Stats::new(&[]).is_none());
    }

    #[test]
    fn dates_round_trip_against_known_instants() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(86_399), "1970-01-01");
        assert_eq!(civil_date(86_400), "1970-01-02");
        assert_eq!(civil_date(951_782_400), "2000-02-29");
        assert_eq!(civil_date(1_767_225_600), "2026-01-01");
        assert_eq!(civil_date(-1), "1969-12-31");
    }

    #[test]
    fn a_command_renders_as_something_you_can_paste() {
        let cmd = Cmd::new("cargo")
            .env("RUSTC_BOOTSTRAP", "1")
            .args(["rustdoc", "-p", "moso-sql"]);
        assert_eq!(
            cmd.rendered(),
            "RUSTC_BOOTSTRAP=1 cargo rustdoc -p moso-sql"
        );
    }

    #[test]
    fn stderr_tail_drops_blank_lines_and_keeps_the_end() {
        let output = Output {
            code: 1,
            stdout: String::new(),
            stderr: "one\n\ntwo\nthree\n".to_owned(),
        };
        assert_eq!(output.stderr_tail(2), "two\nthree");
        assert_eq!(output.stderr_tail(99), "one\ntwo\nthree");
    }

    #[test]
    fn the_workspace_root_is_the_directory_above_this_crate() {
        let root = workspace_root().expect("running inside the checkout");
        assert!(root.join("crates/moso-core/Cargo.toml").is_file());
    }

    #[test]
    fn durations_are_formatted_for_reading_not_for_precision() {
        assert_eq!(fmt_ms(0.0), "0.00 s");
        assert_eq!(fmt_ms(1_234.0), "1.23 s");
        assert_eq!(fmt_ms(45_600.0), "45.6 s");
    }
}
