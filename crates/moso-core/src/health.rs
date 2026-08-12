//! Liveness and readiness, mounted automatically.
//!
//! | Path | Question | Touches dependencies |
//! | --- | --- | --- |
//! | `/healthz` | can this process serve at all? | **never** |
//! | `/readyz` | should this instance receive traffic? | yes, all of them, concurrently |
//!
//! The distinction is load-bearing. A liveness probe that queries the database
//! turns a database blip into a rolling restart of every instance, which turns
//! a blip into an outage. `/healthz` returns 200 as long as the process is
//! alive and the runtime is scheduling.
//!
//! `/readyz` runs every registered check concurrently with a 2 s budget, and
//! answers:
//!
//! ```json
//! { "status": "degraded",
//!   "checks": { "database": "up", "redis": "down: connection refused" },
//!   "version": "1.4.2", "commit": "a1b2c3d", "uptime_s": 43120 }
//! ```
//!
//! Both are excluded from the OpenAPI document and from access logs: they run
//! several times a second forever and would otherwise be the majority of both.
//!
//! # During shutdown
//!
//! `/readyz` starts returning 503 **immediately** on a shutdown signal, before
//! draining begins, so a load balancer removes the instance while it is still
//! serving the requests it already has. That ordering is the whole of graceful
//! shutdown in a load-balanced deployment.
//!
//! # Concurrency and the budget
//!
//! Checks run concurrently and each is bounded by the *whole* budget rather
//! than a share of it: two checks that each take 1.5 s make a ready instance,
//! not an unready one, because they overlap. A check that exceeds the budget is
//! reported `down: timed out after 2s` — a probe that hangs is a probe that
//! failed, and leaving the orchestrator waiting is worse than answering.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::BoxFuture;
use crate::app::Resolver;

/// A readiness probe.
///
/// Dyn-compatible — the application state holds `Vec<Box<dyn HealthCheck>>` —
/// so `check` returns a boxed future rather than being an `async fn`.
///
/// Register one with `App::new(cfg).health_check("database", DatabaseCheck)`;
/// `/readyz` runs every registered check on every probe and reports each under
/// its name.
///
/// ```
/// use moso::prelude::*;
/// use moso::health::{HealthCheck, HealthStatus};
/// use moso::app::Resolver;
/// use moso::BoxFuture;
///
/// /// A database handle.
/// pub struct Db;
/// impl Db {
///     /// Round-trip one cheap statement.
///     async fn ping(&self) -> Result<()> { Ok(()) }
/// }
///
/// /// Is the database reachable?
/// pub struct DatabaseCheck;
///
/// impl HealthCheck for DatabaseCheck {
///     fn check<'a>(&'a self, r: &'a Resolver) -> BoxFuture<'a, HealthStatus> {
///         Box::pin(async move {
///             match r.get::<Db>() {
///                 Ok(db) => match db.ping().await {
///                     Ok(()) => HealthStatus::Up,
///                     Err(e) => HealthStatus::Down(e.to_string()),
///                 },
///                 Err(e) => HealthStatus::Down(e.to_string()),
///             }
///         })
///     }
/// }
/// # fn main() {}
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a health check",
    note = "implement `check(&self, r: &Resolver) -> BoxFuture<'_, HealthStatus>`",
    note = "help: App::new(cfg).health_check(\"database\", {Self}::new())",
    note = "a check must not be expensive: /readyz runs every check on every probe, several \
            times a second, forever"
)]
pub trait HealthCheck: Send + Sync + 'static {
    /// Run the check.
    ///
    /// Given a [`Resolver`], so a check can reach the pool or client it probes
    /// without holding its own copy.
    fn check<'a>(&'a self, resolver: &'a Resolver) -> BoxFuture<'a, HealthStatus>;

    /// Whether failure makes the instance unready.
    ///
    /// `true` by default. A non-critical check that fails degrades the report —
    /// and shows up in the body — without taking the instance out of rotation,
    /// which is right for an optional cache and wrong for the primary database.
    fn critical(&self) -> bool {
        true
    }
}

/// What one check reported.
///
/// ```
/// use moso::health::HealthStatus;
///
/// assert!(HealthStatus::Up.is_up());
///
/// // Degraded is not down: it shows in the report without pulling the instance
/// // out of rotation.
/// let degraded = HealthStatus::Degraded("replica lag 4s".to_owned());
/// assert!(!degraded.is_down());
/// assert_eq!(degraded.render(), "degraded: replica lag 4s");
///
/// // Down is.
/// let down = HealthStatus::Down("connection refused".to_owned());
/// assert!(down.is_down());
/// assert_eq!(down.render(), "down: connection refused");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthStatus {
    /// Working.
    Up,
    /// Working, but not well. The string is shown in the report.
    Degraded(String),
    /// Not working. The string is shown in the report.
    Down(String),
}

impl HealthStatus {
    /// Whether this counts as up.
    pub fn is_up(&self) -> bool {
        matches!(self, HealthStatus::Up)
    }

    /// Whether this counts as down.
    pub fn is_down(&self) -> bool {
        matches!(self, HealthStatus::Down(_))
    }

    /// The rendering used in the report: `up`, `degraded: …`, `down: …`.
    pub fn render(&self) -> String {
        match self {
            HealthStatus::Up => "up".to_owned(),
            HealthStatus::Degraded(reason) => format!("degraded: {reason}"),
            HealthStatus::Down(reason) => format!("down: {reason}"),
        }
    }

    /// How bad this is: `0` up, `1` degraded, `2` down.
    ///
    /// The report keeps the worst rank it saw among the critical checks, which
    /// is what makes "one degraded and one down" render as `down`.
    fn rank(&self) -> u8 {
        match self {
            HealthStatus::Up => 0,
            HealthStatus::Degraded(_) => 1,
            HealthStatus::Down(_) => 2,
        }
    }
}

impl Serialize for HealthStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.render())
    }
}

/// The overall statuses a report can carry, as they appear on the wire.
///
/// Public so a caller comparing `report.status` has something to compare
/// against rather than a string literal repeated in five places.
pub mod status {
    /// Every critical check passed.
    pub const UP: &str = "up";
    /// A critical check reported [`HealthStatus::Degraded`].
    ///
    /// [`HealthStatus::Degraded`]: super::HealthStatus::Degraded
    pub const DEGRADED: &str = "degraded";
    /// A critical check failed, or the process is shutting down.
    pub const DOWN: &str = "down";
}

/// The body `/readyz` returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    /// `up`, `degraded` or `down` — the worst of the critical checks.
    pub status: String,
    /// Each check's result, keyed by the name it was registered under.
    pub checks: indexmap::IndexMap<String, String>,
    /// The application version, from `CARGO_PKG_VERSION`.
    pub version: String,
    /// The build commit, when one was baked in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Seconds since the process started serving.
    pub uptime_s: u64,
}

impl HealthReport {
    /// Fold check results into a report.
    ///
    /// `critical` names the checks whose failure is disqualifying, so a
    /// non-critical failure shows in `checks` and leaves `status` at `up`.
    ///
    /// The `version` and `commit` are filled from the environment (see
    /// [`app_version`] and [`build_commit`]); `App` overwrites `version` with
    /// the OpenAPI document's `info.version` when the application declared one,
    /// which is the value an operator recognises.
    pub fn from_results(
        results: impl IntoIterator<Item = (&'static str, HealthStatus, bool)>,
        uptime: Duration,
    ) -> Self {
        let mut checks = indexmap::IndexMap::new();
        let mut worst = 0u8;

        for (name, status, critical) in results {
            if critical {
                worst = worst.max(status.rank());
            }
            checks.insert(name.to_owned(), status.render());
        }

        Self {
            status: match worst {
                0 => status::UP,
                1 => status::DEGRADED,
                _ => status::DOWN,
            }
            .to_owned(),
            checks,
            version: app_version(),
            commit: build_commit(),
            uptime_s: uptime.as_secs(),
        }
    }

    /// The report a shutting-down instance answers with, before any check runs.
    ///
    /// No check is consulted: the answer is already known, and the point of the
    /// 503 is that it arrives within milliseconds of the signal rather than
    /// after a 2 s budget.
    pub fn shutting_down(uptime: Duration) -> Self {
        let mut checks = indexmap::IndexMap::new();
        checks.insert(
            "process".to_owned(),
            "down: shutting down, draining in-flight requests".to_owned(),
        );
        Self {
            status: status::DOWN.to_owned(),
            checks,
            version: app_version(),
            commit: build_commit(),
            uptime_s: uptime.as_secs(),
        }
    }

    /// The HTTP status: 200 when ready, 503 when not.
    ///
    /// `degraded` is 200 on purpose. Degraded means "serving, imperfectly";
    /// answering 503 would take the last instance out of rotation over a warm
    /// cache being cold, which turns a partial outage into a total one.
    pub fn http_status(&self) -> http::StatusCode {
        if self.is_ready() {
            http::StatusCode::OK
        } else {
            http::StatusCode::SERVICE_UNAVAILABLE
        }
    }

    /// Whether this instance should receive traffic.
    pub fn is_ready(&self) -> bool {
        self.status != status::DOWN
    }
}

/// How long every readiness check has, in total.
///
/// A probe that hangs is a probe that fails: exceeding the budget reports the
/// slow checks as down rather than leaving the orchestrator waiting.
pub const READINESS_BUDGET: Duration = Duration::from_secs(2);

/// Run every check concurrently, bounding each by `budget`.
///
/// Returns one row per check, in registration order, whatever order they
/// finished in — a report whose key order changed between probes would defeat
/// every diff a human runs against it.
pub async fn run_checks<'a>(
    checks: &'a [(&'static str, Arc<dyn HealthCheck>)],
    resolver: &'a Resolver,
    budget: Duration,
) -> Vec<(&'static str, HealthStatus, bool)> {
    let running = checks.iter().map(|(name, check)| {
        let critical = check.critical();
        async move {
            let status = match tokio::time::timeout(budget, check.check(resolver)).await {
                Ok(status) => status,
                Err(_) => HealthStatus::Down(format!(
                    "timed out after {}",
                    humantime::format_duration(budget)
                )),
            };
            (*name, status, critical)
        }
    });
    futures_util::future::join_all(running).await
}

/// Run every check and fold the results into a report.
///
/// What `/readyz` calls. Split from [`run_checks`] so a CLI command or a test
/// can read the raw statuses without the reporting layer.
pub async fn readiness_report(
    checks: &[(&'static str, Arc<dyn HealthCheck>)],
    resolver: &Resolver,
    budget: Duration,
    uptime: Duration,
) -> HealthReport {
    HealthReport::from_results(run_checks(checks, resolver, budget).await, uptime)
}

/// The environment variable an application's version is read from.
pub const VERSION_ENV: &str = "MOSO_VERSION";

/// The environment variable a build commit is read from.
pub const COMMIT_ENV: &str = "MOSO_COMMIT";

/// The version reported by `/readyz`.
///
/// `MOSO_VERSION` if the deployment sets one, otherwise the version of the
/// `moso-core` the binary was built against. An application that wants its own
/// version in the report declares it once, in the place it already declares it:
/// `.openapi(|d| d.version(env!("CARGO_PKG_VERSION")))`. `App` copies that
/// value over this one.
///
/// Read from the environment rather than baked in because `CARGO_PKG_VERSION`
/// inside this crate is *this crate's* version — a compile-time constant of the
/// framework, not of the application that embeds it.
pub fn app_version() -> String {
    std::env::var(VERSION_ENV).unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_owned())
}

/// The build commit reported by `/readyz`, when the build baked one in.
///
/// `MOSO_COMMIT`, then the two spellings container builders set by convention.
pub fn build_commit() -> Option<String> {
    for name in [COMMIT_ENV, "GIT_COMMIT", "SOURCE_COMMIT"] {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn statuses_render_for_the_report() {
        assert_eq!(HealthStatus::Up.render(), "up");
        assert_eq!(
            HealthStatus::Down("connection refused".to_owned()).render(),
            "down: connection refused"
        );
    }

    #[test]
    fn only_up_is_up() {
        assert!(HealthStatus::Up.is_up());
        assert!(!HealthStatus::Degraded("slow".to_owned()).is_up());
        assert!(HealthStatus::Down("gone".to_owned()).is_down());
    }

    #[test]
    fn an_empty_report_is_up() {
        let report = HealthReport::from_results([], Duration::from_secs(3));
        assert_eq!(report.status, status::UP);
        assert!(report.is_ready());
        assert_eq!(report.http_status(), http::StatusCode::OK);
        assert_eq!(report.uptime_s, 3);
        assert!(report.checks.is_empty());
    }

    #[test]
    fn a_failing_critical_check_takes_the_instance_out_of_rotation() {
        let report = HealthReport::from_results(
            [
                ("database", HealthStatus::Down("refused".to_owned()), true),
                ("redis", HealthStatus::Up, true),
            ],
            Duration::ZERO,
        );
        assert_eq!(report.status, status::DOWN);
        assert!(!report.is_ready());
        assert_eq!(report.http_status(), http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(report.checks["database"], "down: refused");
        assert_eq!(report.checks["redis"], "up");
    }

    #[test]
    fn a_failing_non_critical_check_is_reported_but_does_not_disqualify() {
        let report = HealthReport::from_results(
            [
                ("database", HealthStatus::Up, true),
                ("search", HealthStatus::Down("refused".to_owned()), false),
            ],
            Duration::ZERO,
        );
        assert_eq!(report.status, status::UP);
        assert!(report.is_ready());
        assert_eq!(report.checks["search"], "down: refused");
    }

    #[test]
    fn the_worst_critical_status_wins() {
        let report = HealthReport::from_results(
            [
                ("cache", HealthStatus::Degraded("cold".to_owned()), true),
                ("database", HealthStatus::Down("refused".to_owned()), true),
            ],
            Duration::ZERO,
        );
        assert_eq!(report.status, status::DOWN);
    }

    #[test]
    fn degraded_still_serves_traffic() {
        let report = HealthReport::from_results(
            [("cache", HealthStatus::Degraded("cold".to_owned()), true)],
            Duration::ZERO,
        );
        assert_eq!(report.status, status::DEGRADED);
        assert!(report.is_ready());
        assert_eq!(report.http_status(), http::StatusCode::OK);
    }

    #[test]
    fn checks_keep_registration_order() {
        let report = HealthReport::from_results(
            [
                ("zebra", HealthStatus::Up, true),
                ("aardvark", HealthStatus::Up, true),
            ],
            Duration::ZERO,
        );
        let names: Vec<&str> = report.checks.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["zebra", "aardvark"]);
    }

    #[test]
    fn a_shutting_down_report_is_down_without_consulting_anything() {
        let report = HealthReport::shutting_down(Duration::from_secs(10));
        assert_eq!(report.status, status::DOWN);
        assert!(!report.is_ready());
        assert!(report.checks["process"].starts_with("down:"));
    }

    #[test]
    fn a_report_round_trips_through_json() {
        let report = HealthReport::from_results(
            [("database", HealthStatus::Up, true)],
            Duration::from_secs(43_120),
        );
        let json = serde_json::to_string(&report).expect("serialises");
        let back: HealthReport = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back.status, report.status);
        assert_eq!(back.uptime_s, 43_120);
        assert_eq!(back.checks["database"], "up");
    }

    // ── the runner ────────────────────────────────────────────────────────

    /// A check that answers immediately with whatever it was built with.
    struct Fixed {
        status: HealthStatus,
        critical: bool,
        runs: Arc<AtomicUsize>,
    }

    impl HealthCheck for Fixed {
        fn check<'a>(&'a self, _resolver: &'a Resolver) -> BoxFuture<'a, HealthStatus> {
            self.runs.fetch_add(1, Ordering::AcqRel);
            let status = self.status.clone();
            Box::pin(async move { status })
        }

        fn critical(&self) -> bool {
            self.critical
        }
    }

    /// A check that never answers, to prove the budget is enforced.
    struct Hangs;

    impl HealthCheck for Hangs {
        fn check<'a>(&'a self, _resolver: &'a Resolver) -> BoxFuture<'a, HealthStatus> {
            Box::pin(async {
                std::future::pending::<()>().await;
                HealthStatus::Up
            })
        }
    }

    fn empty_resolver() -> Resolver {
        Resolver::new(Arc::new(crate::di::ProviderMap::new()))
    }

    #[tokio::test]
    async fn every_check_runs_and_keeps_its_criticality() {
        let runs = Arc::new(AtomicUsize::new(0));
        let checks: Vec<(&'static str, Arc<dyn HealthCheck>)> = vec![
            (
                "database",
                Arc::new(Fixed {
                    status: HealthStatus::Up,
                    critical: true,
                    runs: Arc::clone(&runs),
                }),
            ),
            (
                "search",
                Arc::new(Fixed {
                    status: HealthStatus::Down("refused".to_owned()),
                    critical: false,
                    runs: Arc::clone(&runs),
                }),
            ),
        ];

        let results = run_checks(&checks, &empty_resolver(), READINESS_BUDGET).await;
        assert_eq!(runs.load(Ordering::Acquire), 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "database");
        assert!(results[0].2, "the database check is critical");
        assert!(!results[1].2, "the search check is not");
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_check_is_reported_down_rather_than_hanging_the_probe() {
        let checks: Vec<(&'static str, Arc<dyn HealthCheck>)> = vec![("stuck", Arc::new(Hangs))];

        let report = readiness_report(
            &checks,
            &empty_resolver(),
            Duration::from_secs(2),
            Duration::ZERO,
        )
        .await;

        assert_eq!(report.status, status::DOWN);
        assert_eq!(report.checks["stuck"], "down: timed out after 2s");
    }

    #[tokio::test(start_paused = true)]
    async fn checks_run_concurrently_rather_than_one_after_another() {
        /// A check that takes most of the budget on its own.
        struct Slow;

        impl HealthCheck for Slow {
            fn check<'a>(&'a self, _resolver: &'a Resolver) -> BoxFuture<'a, HealthStatus> {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(1_500)).await;
                    HealthStatus::Up
                })
            }
        }

        let checks: Vec<(&'static str, Arc<dyn HealthCheck>)> = vec![
            ("first", Arc::new(Slow)),
            ("second", Arc::new(Slow)),
            ("third", Arc::new(Slow)),
        ];

        let started = tokio::time::Instant::now();
        let report =
            readiness_report(&checks, &empty_resolver(), READINESS_BUDGET, Duration::ZERO).await;

        // Serially this would be 4.5 s and every check would time out.
        assert!(started.elapsed() < Duration::from_millis(1_600));
        assert_eq!(report.status, status::UP);
    }

    #[test]
    fn the_version_falls_back_to_the_framework_version() {
        // Not asserting the value: the environment may legitimately set one.
        assert!(!app_version().is_empty());
    }
}
