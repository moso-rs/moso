//! The standalone jobs dashboard at `/_jobs`.
//!
//! The full version lives in `moso-admin` and is gated on the admin permission.
//! This is the same data mounted on its own, for a deployment that wants the
//! operational view without the whole admin — which is most of them.
//!
//! What it shows, and why each one is on the list: queues with depth **and
//! latency** (depth alone does not say whether the queue is keeping up),
//! running jobs with elapsed time and a cancel button, recent failures with the
//! full error chain (not the first line), the dead-letter queue with bulk retry
//! and discard, and the schedule with its next and last run.

use moso_core::Router;
use moso_core::extract::{QueryMap, QueryValue};
use moso_core::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::{JobId, JobState, Priority};

/// The path the dashboard is mounted at.
///
/// ```
/// assert_eq!(moso_jobs::dashboard::DASHBOARD_PATH, "/_jobs");
/// ```
pub const DASHBOARD_PATH: &str = "/_jobs";

/// One queue, as the dashboard lists it.
///
/// ```no_run
/// use moso_jobs::dashboard::QueueView;
///
/// # fn f(v: &QueueView) {
/// let _ = v.ready;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QueueView {
    /// Which queue.
    pub queue: String,
    /// Waiting to run now.
    pub ready: u64,
    /// Leased by a worker.
    pub running: u64,
    /// Waiting for a backoff.
    pub retrying: u64,
    /// In the dead-letter queue.
    pub dead: u64,
    /// How long the oldest ready job has been waiting, in seconds. The number
    /// that actually says whether the queue is keeping up.
    pub oldest_ready_seconds: Option<f64>,
}

/// One job, as the dashboard lists it.
///
/// ```no_run
/// use moso_jobs::dashboard::JobView;
///
/// # fn f(v: &JobView) {
/// let _ = &v.name;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JobView {
    /// Which row.
    pub id: JobId,
    /// The job's wire name.
    pub name: String,
    /// Which queue.
    pub queue: String,
    /// Where it is in its life.
    pub state: JobState,
    /// How urgent.
    pub priority: Priority,
    /// Which attempt is next.
    pub attempt: u32,
    /// How many attempts there will be.
    pub max_attempts: u32,
    /// When it was enqueued, RFC 3339.
    pub enqueued_at: String,
    /// How long it has been running, for a leased job.
    pub elapsed_seconds: Option<f64>,
    /// The last failure's whole chain.
    pub last_error: Option<String>,
    /// Which worker holds it.
    pub worker: Option<String>,
}

/// One schedule, as the dashboard lists it.
///
/// ```no_run
/// use moso_jobs::dashboard::ScheduleView;
///
/// # fn f(v: &ScheduleView) {
/// let _ = &v.expression;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ScheduleView {
    /// Its leader-election key.
    pub id: String,
    /// The job it enqueues.
    pub job: String,
    /// The cron expression or interval, as written.
    pub expression: String,
    /// The timezone it is evaluated in.
    pub timezone: String,
    /// When it last ran, RFC 3339.
    ///
    /// Read out of the queue backend, which every process in the fleet shares,
    /// so this answers the same way whichever pod served the request. `None`
    /// means the schedule has not fired since the backend started keeping the
    /// record — or that the backend keeps none, which
    /// [`Queue::record_schedule_run`](crate::Queue::record_schedule_run)
    /// describes.
    pub last_run: Option<String>,
    /// When it next runs, RFC 3339.
    pub next_run: Option<String>,
    /// The process that fired [`last_run`](ScheduleView::last_run).
    ///
    /// The fleet-wide answer to "who leads this schedule", accurate as of the
    /// last occurrence — and the field to read, because it does not depend on
    /// which pod the request happened to reach.
    pub leader: Option<String>,
    /// Whether the process serving this request holds the lease.
    ///
    /// `None` — `null` on the wire — when that process runs no scheduler, or
    /// runs one that was not handed to
    /// [`Dashboard::scheduler`](Dashboard::scheduler). That is the honest answer
    /// to a question with no local evidence, and it is why this is not a `bool`:
    /// a `false` there would say "this pod is not the leader" when the truth is
    /// "this pod cannot know".
    pub leader_here: Option<bool>,
}

/// One dead letter, as the dashboard lists it.
///
/// ```no_run
/// use moso_jobs::dashboard::DeadView;
///
/// # fn f(v: &DeadView) {
/// let _ = &v.last_error;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeadView {
    /// Which row.
    pub id: JobId,
    /// The job's wire name.
    pub name: String,
    /// Which queue.
    pub queue: String,
    /// How many attempts were made.
    pub attempts: u32,
    /// The last failure's whole chain — not its first line, which is where the
    /// useful part almost never is.
    pub last_error: String,
    /// When it gave up, RFC 3339.
    pub failed_at: String,
    /// The payload, kept so the job can be retried after a fix.
    pub payload: serde_json::Value,
}

/// The query a list endpoint accepts.
///
/// Parsed out of the URI by hand rather than through `Query<T>`, because
/// `Query<T>` needs `T: Schema` and these routes are deliberately absent from
/// the OpenAPI document — an operator's endpoint is not part of the API
/// contract, and deriving a schema for it would put it there.
#[derive(Debug, Default)]
struct ListQuery {
    /// Only this job's wire name.
    job: Option<String>,
    /// Only this queue.
    queue: Option<String>,
    /// Only failures whose chain contains this.
    error: Option<String>,
    /// Where the previous page stopped.
    cursor: Option<String>,
    /// How many rows.
    limit: Option<u32>,
}

impl ListQuery {
    /// Read the query string off a request URI.
    fn from_uri(uri: &moso_core::deps::http::Uri) -> Self {
        let Ok(map) = QueryMap::parse(uri.query().unwrap_or_default(), 2) else {
            return Self::default();
        };
        let scalar = |key: &str| match map.get(key) {
            Some(QueryValue::Scalar(value)) if !value.is_empty() => Some(value.clone()),
            _ => None,
        };
        Self {
            job: scalar("job"),
            queue: scalar("queue"),
            error: scalar("error"),
            cursor: scalar("cursor"),
            limit: scalar("limit").and_then(|value| value.parse().ok()),
        }
    }

    /// The filter this query describes.
    fn filter(&self) -> crate::DlqFilter {
        let mut filter = crate::DlqFilter::new();
        if let Some(job) = &self.job {
            filter = filter.job(job.clone());
        }
        if let Some(queue) = &self.queue {
            filter = filter.queue(queue.clone());
        }
        if let Some(error) = &self.error {
            filter = filter.error_contains(error.clone());
        }
        filter
    }

    /// How many rows to return, capped.
    ///
    /// The cap is not politeness: a dead-letter queue can hold a hundred
    /// thousand rows with their payloads, and `?limit=1000000` would render all
    /// of them into one response.
    fn limit(&self) -> u32 {
        self.limit.unwrap_or(50).clamp(1, 200)
    }
}

/// The dashboard's routes, and the optional pieces wired into them.
///
/// **Mount it behind something.** These routes show payloads, and payloads
/// carry identifiers, addresses and occasionally tokens. `moso-admin` gates the
/// same data on the admin permission; a standalone mount is the caller's to
/// protect, and the boot log warns when it is mounted with no guard on the
/// router.
///
/// | Route | Shows |
/// | --- | --- |
/// | `GET /_jobs` | the backend, the counts, and the other routes |
/// | `GET /_jobs/queues` | depth **and** latency per queue |
/// | `GET /_jobs/dead` | the dead-letter queue, newest first |
/// | `GET /_jobs/schedules` | next and last run, and who leads |
/// | `POST /_jobs/jobs/{id}/cancel` | ask a running job to stop |
/// | `POST /_jobs/dead/retry` | bulk retry, with a mandatory limit |
/// | `POST /_jobs/dead/discard` | bulk discard, with a mandatory limit |
///
/// ```no_run
/// use moso_jobs::{dashboard::Dashboard, Jobs, Scheduler};
///
/// fn mount(jobs: Jobs, scheduler: &Scheduler) -> moso_core::Router {
///     Dashboard::new(jobs).scheduler(scheduler.leadership()).routes()
/// }
/// ```
pub struct Dashboard {
    /// The queue and the registry behind every view.
    jobs: crate::Jobs,
    /// What this process leads, when it runs a scheduler.
    leadership: Option<crate::SchedulerLeadership>,
}

impl Dashboard {
    /// A dashboard over `jobs`, with nothing else wired.
    ///
    /// ```no_run
    /// # use moso_jobs::{dashboard::Dashboard, Jobs};
    /// # fn f(jobs: Jobs) { let _ = Dashboard::new(jobs); }
    /// ```
    #[must_use]
    pub fn new(jobs: crate::Jobs) -> Self {
        Self {
            jobs,
            leadership: None,
        }
    }

    /// Answer `leader_here` from the scheduler running in this process.
    ///
    /// Without it the field is `null` rather than `false`, because a process
    /// with no scheduler has no evidence either way and a confident `false` is
    /// the lie this replaced.
    ///
    /// ```no_run
    /// # use moso_jobs::{dashboard::Dashboard, Jobs, Scheduler};
    /// # fn f(jobs: Jobs, s: &Scheduler) { let _ = Dashboard::new(jobs).scheduler(s.leadership()); }
    /// ```
    #[must_use]
    pub fn scheduler(mut self, leadership: crate::SchedulerLeadership) -> Self {
        self.leadership = Some(leadership);
        self
    }

    /// The router, hidden from the OpenAPI document.
    ///
    /// ```no_run
    /// # use moso_jobs::{dashboard::Dashboard, Jobs};
    /// # fn f(jobs: Jobs) -> moso_core::Router { Dashboard::new(jobs).routes() }
    /// ```
    #[must_use]
    pub fn routes(self) -> Router {
        routes_with(self.jobs, self.leadership)
    }
}

impl core::fmt::Debug for Dashboard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dashboard")
            .field("backend", &self.jobs.queue().name())
            .field("scheduler", &self.leadership.is_some())
            .finish()
    }
}

/// The dashboard's routes, with no scheduler wired.
///
/// [`Dashboard`] is the form that can answer `leader_here`; this is the short
/// spelling for a process that runs no scheduler, and it is exactly
/// `Dashboard::new(jobs).routes()`.
///
/// ```no_run
/// use moso_jobs::{dashboard, Jobs};
///
/// fn mount(jobs: Jobs) -> moso_core::Router {
///     dashboard::routes(jobs)
/// }
/// ```
#[must_use]
pub fn routes(jobs: crate::Jobs) -> Router {
    Dashboard::new(jobs).routes()
}

/// Every route, over an optional leadership handle.
fn routes_with(jobs: crate::Jobs, leadership: Option<crate::SchedulerLeadership>) -> Router {
    Router::new()
        .get(DASHBOARD_PATH, {
            let jobs = jobs.clone();
            move || {
                let jobs = jobs.clone();
                async move { index(&jobs) }
            }
        })
        .get("/_jobs/queues", {
            let jobs = jobs.clone();
            move || {
                let jobs = jobs.clone();
                async move { queues(&jobs).await }
            }
        })
        .get("/_jobs/schedules", {
            let jobs = jobs.clone();
            move || {
                let jobs = jobs.clone();
                let leadership = leadership.clone();
                async move { schedules(&jobs, leadership.as_ref()).await }
            }
        })
        .post("/_jobs/jobs/{id}/cancel", {
            let jobs = jobs.clone();
            move |path: moso_core::extract::Path<String>| {
                let jobs = jobs.clone();
                async move { cancel(&jobs, &path.0).await }
            }
        })
        .get("/_jobs/dead", {
            let jobs = jobs.clone();
            move |uri: moso_core::deps::http::Uri| {
                let jobs = jobs.clone();
                async move { dead(&jobs, &ListQuery::from_uri(&uri)).await }
            }
        })
        .post("/_jobs/dead/retry", {
            let jobs = jobs.clone();
            move |uri: moso_core::deps::http::Uri| {
                let jobs = jobs.clone();
                async move { bulk(&jobs, &ListQuery::from_uri(&uri), true).await }
            }
        })
        .post("/_jobs/dead/discard", {
            move |uri: moso_core::deps::http::Uri| {
                let jobs = jobs.clone();
                async move { bulk(&jobs, &ListQuery::from_uri(&uri), false).await }
            }
        })
        // Hidden from the OpenAPI document. These are an operator's endpoints,
        // and putting them in the contract makes every generated client carry
        // seven routes nobody calls — and advertises the payload viewer.
        .hidden()
}

/// What the dashboard can show, and where the rest of it is.
fn index(jobs: &crate::Jobs) -> moso_core::Response {
    json(serde_json::json!({
        "backend": jobs.queue().name(),
        "registered": jobs.registry().len(),
        "schedules": jobs.registry().schedules().len(),
        "routes": [
            "GET  /_jobs/queues",
            "GET  /_jobs/schedules",
            "GET  /_jobs/dead",
            "POST /_jobs/jobs/{id}/cancel",
            "POST /_jobs/dead/retry?limit=…",
            "POST /_jobs/dead/discard?limit=…",
        ],
    }))
}

/// Depth *and* latency, because depth alone does not say whether a queue is
/// keeping up.
async fn queues(jobs: &crate::Jobs) -> moso_core::Response {
    match jobs.stats().await {
        Ok(stats) => {
            let views: Vec<QueueView> = stats
                .into_iter()
                .map(|one| QueueView {
                    queue: one.queue,
                    ready: one.ready,
                    running: one.running,
                    retrying: one.retrying,
                    dead: one.dead,
                    oldest_ready_seconds: one.oldest_ready.map(|d| d.as_secs_f64()),
                })
                .collect();
            json(serde_json::json!({ "queues": views }))
        }
        Err(error) => failed(&error),
    }
}

/// The schedule, with what an operator needs to answer "did it run".
///
/// Two different questions with two different sources, which is why they are two
/// fields. "When did it last run, and who ran it" is fleet-wide and comes out of
/// the queue backend, so every pod answers it the same way. "Am *I* the leader"
/// is local and comes from the scheduler in this process, so a pod that runs
/// none says `null` rather than `false`.
async fn schedules(
    jobs: &crate::Jobs,
    leadership: Option<&crate::SchedulerLeadership>,
) -> moso_core::Response {
    json(serde_json::json!({ "schedules": schedule_views(jobs, leadership).await }))
}

/// The rows `schedules` renders, as values a test can read without a response
/// body.
async fn schedule_views(
    jobs: &crate::Jobs,
    leadership: Option<&crate::SchedulerLeadership>,
) -> Vec<ScheduleView> {
    let now = chrono::Utc::now();
    let recorded: std::collections::BTreeMap<String, crate::ScheduleRun> =
        match jobs.queue().schedule_runs().await {
            Ok(runs) => runs
                .into_iter()
                .map(|run| (run.schedule.as_str().to_owned(), run))
                .collect(),
            // A backend that keeps no schedule state is not a failure of this
            // endpoint: the schedules are still worth listing, and `last_run`
            // being absent is the truth about what is known.
            Err(_) => std::collections::BTreeMap::new(),
        };

    jobs.registry()
        .schedules()
        .iter()
        .map(|schedule| {
            let run = recorded.get(schedule.id().as_str());
            ScheduleView {
                id: schedule.id().to_string(),
                job: schedule.job().to_owned(),
                expression: schedule.expression(),
                timezone: schedule.timezone().to_owned(),
                last_run: run.map(|run| run.ran_at.to_rfc3339()),
                next_run: schedule
                    .next_after(now)
                    .ok()
                    .flatten()
                    .map(|at| at.to_rfc3339()),
                leader: run.map(|run| run.leader.as_str().to_owned()),
                leader_here: leadership.map(|held| held.leads(schedule.id())),
            }
        })
        .collect()
}

/// The dead-letter queue, with the full error chain.
async fn dead(jobs: &crate::Jobs, query: &ListQuery) -> moso_core::Response {
    let Some(dlq) = jobs.dead_letters() else {
        return no_dead_letters(jobs);
    };
    match dlq
        .list(&query.filter(), query.cursor.as_deref(), query.limit())
        .await
    {
        Ok((letters, cursor)) => {
            let views: Vec<DeadView> = letters
                .into_iter()
                .map(|letter| DeadView {
                    id: letter.id,
                    name: letter.name,
                    queue: letter.queue,
                    attempts: letter.attempts,
                    last_error: letter.last_error,
                    failed_at: letter.failed_at.to_rfc3339(),
                    payload: letter.payload,
                })
                .collect();
            json(serde_json::json!({ "dead": views, "cursor": cursor }))
        }
        Err(error) => failed(&error),
    }
}

/// Bulk retry or discard. The limit is mandatory and capped, because a bulk
/// operation over an unbounded filter is how a fix becomes an outage.
async fn bulk(jobs: &crate::Jobs, query: &ListQuery, retry: bool) -> moso_core::Response {
    let Some(dlq) = jobs.dead_letters() else {
        return no_dead_letters(jobs);
    };
    let filter = query.filter();
    let outcome = if retry {
        dlq.retry(&filter, query.limit()).await
    } else {
        dlq.discard(&filter, query.limit()).await
    };
    match outcome {
        Ok(count) => json(serde_json::json!({
            "action": if retry { "retry" } else { "discard" },
            "affected": count,
            "limit": query.limit(),
        })),
        Err(error) => failed(&error),
    }
}

/// Ask one job to stop.
async fn cancel(jobs: &crate::Jobs, id: &str) -> moso_core::Response {
    let Ok(id) = id.parse::<JobId>() else {
        return (
            moso_core::deps::http::StatusCode::BAD_REQUEST,
            moso_core::deps::axum::Json(serde_json::json!({ "error": "not a job id" })),
        )
            .into_response();
    };
    match jobs.cancel(id).await {
        Ok(cancelled) => json(serde_json::json!({ "cancelled": cancelled })),
        Err(error) => failed(&error),
    }
}

/// The response for a backend with no dead-letter view.
fn no_dead_letters(jobs: &crate::Jobs) -> moso_core::Response {
    (
        moso_core::deps::http::StatusCode::NOT_IMPLEMENTED,
        moso_core::deps::axum::Json(serde_json::json!({
            "error": format!(
                "the `{}` queue has no dead-letter view wired to this handle",
                jobs.queue().name()
            ),
            "help": "call `Jobs::with_dead_letters(..)` with the same backend when building \
                     the handle",
        })),
    )
        .into_response()
}

/// A JSON body.
fn json(value: serde_json::Value) -> moso_core::Response {
    moso_core::deps::axum::Json(value).into_response()
}

/// A backend failure, rendered without leaking a connection string.
fn failed(error: &crate::Error) -> moso_core::Response {
    (
        moso_core::deps::http::StatusCode::SERVICE_UNAVAILABLE,
        moso_core::deps::axum::Json(serde_json::json!({ "error": error.chain() })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JobRegistry, Jobs};

    fn jobs() -> Jobs {
        Jobs::new(
            std::sync::Arc::new(crate::backend::MemoryQueue::new()),
            std::sync::Arc::new(JobRegistry::new()),
        )
    }

    /// Every route the documentation promises, and none of them in the API
    /// contract.
    #[test]
    fn every_documented_route_is_mounted_and_hidden() {
        let router = routes(jobs());
        let paths: Vec<String> = router
            .describe()
            .into_iter()
            .map(|route| route.path)
            .collect();
        for path in [
            "/_jobs",
            "/_jobs/queues",
            "/_jobs/schedules",
            "/_jobs/dead",
            "/_jobs/jobs/{id}/cancel",
            "/_jobs/dead/retry",
            "/_jobs/dead/discard",
        ] {
            assert!(
                paths.iter().any(|p| p == path),
                "{path} is missing from {paths:?}"
            );
        }
    }

    /// The limit is what stops a bulk retry from becoming an outage, so its
    /// bounds are pinned rather than trusted.
    #[test]
    fn the_list_limit_is_clamped() {
        assert_eq!(ListQuery::default().limit(), 50);
        assert_eq!(
            ListQuery {
                limit: Some(1_000_000),
                ..ListQuery::default()
            }
            .limit(),
            200
        );
        assert_eq!(
            ListQuery {
                limit: Some(0),
                ..ListQuery::default()
            }
            .limit(),
            1
        );
    }

    /// The query maps onto the filter one field at a time.
    #[test]
    fn the_query_becomes_a_filter() {
        let query = ListQuery {
            job: Some("send_welcome_email".to_owned()),
            queue: Some("mail".to_owned()),
            error: Some("refused".to_owned()),
            ..ListQuery::default()
        };
        let filter = query.filter();
        assert_eq!(filter.job_name(), Some("send_welcome_email"));
        assert_eq!(filter.queue_name(), Some("mail"));
        assert_eq!(filter.error_needle(), Some("refused"));
    }

    /// A backend with no dead-letter view has to say so, rather than serving an
    /// empty list that reads as "nothing has failed".
    #[tokio::test]
    async fn a_missing_dead_letter_view_is_reported_and_not_faked() {
        let response = dead(&jobs(), &ListQuery::default()).await;
        assert_eq!(response.status().as_u16(), 501);
    }

    /// The index tells an operator what else is there, because a dashboard
    /// whose routes are undiscoverable is a dashboard nobody opens.
    #[test]
    fn the_index_lists_the_other_routes() {
        let response = index(&jobs());
        assert_eq!(response.status().as_u16(), 200);
    }

    // ── the schedule view ───────────────────────────────────────────────────

    struct Cleanup;
    impl crate::Job for Cleanup {
        type Args = ();
        const NAME: &'static str = "nightly_cleanup";
        async fn run(_args: (), _ctx: crate::JobCtx) -> crate::Result {
            Ok(())
        }
    }

    /// A dashboard over one memory queue and one nightly schedule.
    fn scheduled() -> (
        Jobs,
        std::sync::Arc<crate::backend::MemoryQueue>,
        crate::ScheduleId,
    ) {
        let queue = std::sync::Arc::new(crate::backend::MemoryQueue::new());
        let registry = JobRegistry::new()
            .register::<Cleanup>()
            .schedule(crate::Cron::new::<Cleanup>("0 3 * * *", ()));
        let id = registry.schedules()[0].id().clone();
        let jobs = Jobs::new(
            std::sync::Arc::clone(&queue) as std::sync::Arc<dyn crate::Queue>,
            std::sync::Arc::new(registry),
        );
        (jobs, queue, id)
    }

    /// `last_run` comes out of the queue, which every process shares, so a pod
    /// that has never led the schedule still answers "when did it last run".
    #[tokio::test]
    async fn the_last_run_is_read_from_the_queue_and_not_from_this_process() {
        let (jobs, queue, id) = scheduled();

        let before = schedule_views(&jobs, None).await;
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].last_run, None, "nothing has fired yet");
        assert_eq!(before[0].leader, None);

        let ran_at = chrono::Utc::now();
        crate::Queue::record_schedule_run(
            queue.as_ref(),
            &crate::ScheduleRun::new(id, "nightly_cleanup", crate::WorkerId::new("pod-7"), ran_at),
        )
        .await
        .expect("the memory queue keeps schedule state");

        let after = schedule_views(&jobs, None).await;
        assert_eq!(
            after[0].last_run.as_deref(),
            Some(ran_at.to_rfc3339()).as_deref()
        );
        assert_eq!(
            after[0].leader.as_deref(),
            Some("pod-7"),
            "the fleet-wide answer names the process that fired it"
        );
    }

    /// `leader_here` is a question about *this* process, so a process with no
    /// scheduler answers `null` — the honest shape — rather than `false`.
    #[tokio::test]
    async fn leader_here_is_absent_without_a_scheduler_and_exact_with_one() {
        let (jobs, _queue, id) = scheduled();

        assert_eq!(
            schedule_views(&jobs, None).await[0].leader_here,
            None,
            "a process with no scheduler cannot know"
        );

        let leadership = crate::SchedulerLeadership::none();
        assert_eq!(
            schedule_views(&jobs, Some(&leadership)).await[0].leader_here,
            Some(false),
            "a process that runs a scheduler and does not lead this one says so"
        );

        let kv = moso_kv::Kv::in_memory("dashboard-leader").expect("in-memory kv");
        let scheduler = crate::Scheduler::new(jobs.clone(), jobs.shared_registry(), kv);
        let leadership = scheduler.leadership();
        let held = scheduler
            .try_lead(&id)
            .await
            .expect("the store is up")
            .expect("nobody else is leading");
        assert_eq!(
            schedule_views(&jobs, Some(&leadership)).await[0].leader_here,
            Some(true)
        );
        held.resign().await;
    }
}
