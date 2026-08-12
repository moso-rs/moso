//! [`JobRegistry`] — what a worker knows how to run.
//!
//! Registration is a statement you can read (ADR-0004: no `inventory`, no
//! `ctor`). What that buys is the boot check: `App::build()` can prove that
//! every job type enqueued anywhere is registered, and fail loudly if not:
//!
//! ```text
//! ✗ job `ReindexSearch` is enqueued but not registered
//!     enqueued at  src/services/search.rs:42
//!     fix          add `.register::<ReindexSearch>()` in src/jobs/mod.rs
//! ```
//!
//! # What boot can and cannot know
//!
//! ADR-0004 forbids link-time registries, so *nothing* in this process knows
//! the set of enqueue call sites before one of them runs. What
//! [`JobRegistry::validate`] proves at boot is everything that is knowable
//! statically — a duplicate wire name, a schedule naming a job nobody
//! registered, a cron expression that does not parse, a retry budget of zero —
//! and the enqueue path carries `#[track_caller]` so the *first*
//! enqueue of an unregistered job produces exactly the message above, naming the
//! line to change. The alternative would be an `inventory` section, which is the
//! thing ADR-0004 refuses.

use std::time::Duration;

use moso_core::BoxFuture;
use moso_core::error::{BootError, BootErrors};

use crate::{Job, JobCtx, Priority, Result, RetryPolicy, Schedule};

/// One registered job, erased so the registry can hold them together.
///
/// ```no_run
/// use moso_jobs::RegisteredJob;
///
/// # fn f(r: &RegisteredJob) {
/// let _: &'static str = r.name();
/// # }
/// ```
pub struct RegisteredJob {
    /// The wire name.
    name: &'static str,
    /// The Rust type's name, for a message a human can find in the source.
    type_name: &'static str,
    /// The queue it runs on.
    queue: &'static str,
    /// The retry policy from its constants.
    retry: RetryPolicy,
    /// How long one attempt may take.
    timeout: Duration,
    /// Its default priority.
    priority: Priority,
    /// Its deduplication window.
    unique_for: Option<Duration>,
    /// Whether a unique-key chain runs strictly in order.
    serial: bool,
    /// A disagreement between `Job::BACKOFF` and `Job::backoff`, held for
    /// [`JobRegistry::validate`] to turn into a boot problem.
    inconsistent_backoff: Option<String>,
    /// Deserialise the payload and run it. The erasure that lets a worker hold
    /// every job type in one map.
    run: RunFn,
}

/// The erased body of a registered job.
///
/// Takes the payload as JSON because that is what came off the queue, and
/// returns a boxed future because the map holds many of these.
pub type RunFn = fn(serde_json::Value, JobCtx) -> BoxFuture<'static, Result>;

/// The erased body for one job type, with its failure hook.
///
/// A free function rather than a closure so it coerces to a plain `fn` pointer:
/// the registry holds one of these per job, and a boxed closure per job would be
/// an allocation nobody needs.
///
/// The payload is decoded twice on the failure path — once for `run`, which
/// consumes it, and once for `on_failure`, which must see it. A hook that could
/// not name the row it is alerting about would not be worth having, and the
/// second decode only happens when something already went wrong.
fn run_erased<J: Job>(payload: serde_json::Value, ctx: JobCtx) -> BoxFuture<'static, Result> {
    Box::pin(async move {
        // The poison-payload guard. A deploy that changed a payload's shape
        // must not turn 40,000 queued rows into 1,000,000 failed attempts, so
        // this error skips the retry budget entirely.
        let args: J::Args = match serde_json::from_value(payload.clone()) {
            Ok(args) => args,
            Err(error) => {
                return Err(crate::Error::Payload {
                    job: J::NAME.to_owned(),
                    detail: error.to_string(),
                });
            }
        };
        match J::run(args, ctx.clone()).await {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Ok(args) = serde_json::from_value::<J::Args>(payload) {
                    J::on_failure(&args, &error, &ctx).await;
                }
                Err(error)
            }
        }
    })
}

impl RegisteredJob {
    /// Describe a job from its type.
    ///
    /// ```no_run
    /// # use moso_jobs::{Job, RegisteredJob};
    /// # fn f<J: Job>() { let _ = RegisteredJob::of::<J>(); }
    /// ```
    #[must_use]
    pub fn of<J: Job>() -> Self {
        Self {
            name: J::NAME,
            type_name: core::any::type_name::<J>(),
            queue: J::QUEUE,
            retry: J::retry_policy(),
            timeout: J::TIMEOUT,
            priority: J::PRIORITY,
            unique_for: J::UNIQUE_FOR,
            serial: J::SERIAL,
            inconsistent_backoff: backoff_disagreement::<J>(),
            run: run_erased::<J>,
        }
    }

    /// The wire name.
    ///
    /// ```no_run
    /// # use moso_jobs::RegisteredJob;
    /// # fn f(r: &RegisteredJob) { let _: &'static str = r.name(); }
    /// ```
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The Rust type's name, for a message somebody has to find in the source.
    ///
    /// ```no_run
    /// # use moso_jobs::RegisteredJob;
    /// # fn f(r: &RegisteredJob) { let _: &'static str = r.type_name(); }
    /// ```
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.type_name
    }

    /// The queue it runs on.
    ///
    /// ```no_run
    /// # use moso_jobs::RegisteredJob;
    /// # fn f(r: &RegisteredJob) { let _: &'static str = r.queue(); }
    /// ```
    #[must_use]
    pub fn queue(&self) -> &'static str {
        self.queue
    }

    /// The retry policy.
    ///
    /// ```no_run
    /// # use moso_jobs::{RegisteredJob, RetryPolicy};
    /// # fn f(r: &RegisteredJob) { let _: RetryPolicy = r.retry(); }
    /// ```
    #[must_use]
    pub fn retry(&self) -> RetryPolicy {
        self.retry
    }

    /// How long one attempt may take.
    ///
    /// ```no_run
    /// # use moso_jobs::RegisteredJob;
    /// # fn f(r: &RegisteredJob) { let _: std::time::Duration = r.timeout(); }
    /// ```
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The default priority.
    ///
    /// ```no_run
    /// # use moso_jobs::{Priority, RegisteredJob};
    /// # fn f(r: &RegisteredJob) { let _: Priority = r.priority(); }
    /// ```
    #[must_use]
    pub fn priority(&self) -> Priority {
        self.priority
    }

    /// The deduplication window.
    ///
    /// ```no_run
    /// # use moso_jobs::RegisteredJob;
    /// # fn f(r: &RegisteredJob) { let _: Option<std::time::Duration> = r.unique_for(); }
    /// ```
    #[must_use]
    pub fn unique_for(&self) -> Option<Duration> {
        self.unique_for
    }

    /// Whether a unique-key chain runs strictly in order.
    ///
    /// ```no_run
    /// # use moso_jobs::RegisteredJob;
    /// # fn f(r: &RegisteredJob) { let _: bool = r.serial(); }
    /// ```
    #[must_use]
    pub fn serial(&self) -> bool {
        self.serial
    }

    /// Run it with a payload from the queue.
    ///
    /// # Errors
    ///
    /// [`Error::Payload`](crate::Error::Payload) when the JSON does not
    /// deserialise — the poison-payload guard, which skips retries entirely —
    /// or whatever the job body returns.
    ///
    /// ```no_run
    /// # use moso_jobs::{JobCtx, RegisteredJob};
    /// # async fn f(r: &RegisteredJob, p: serde_json::Value, c: JobCtx) -> moso_jobs::Result {
    /// r.run(p, c).await
    /// # }
    /// ```
    pub async fn run(&self, payload: serde_json::Value, ctx: JobCtx) -> Result {
        (self.run)(payload, ctx).await
    }
}

/// Whether `J::BACKOFF` and `J::backoff` describe the same ladder.
///
/// A job that overrides the function without the constant would enqueue rows
/// promising one ladder and document another. Comparing the first eight steps
/// catches every disagreement a real policy can have, since all four
/// [`Backoff`](crate::Backoff) shapes have separated by then.
fn backoff_disagreement<J: Job>() -> Option<String> {
    for attempt in 1..=8_u32 {
        let declared = J::BACKOFF.delay(attempt);
        let computed = J::backoff(attempt);
        if declared != computed {
            return Some(format!(
                "attempt {attempt} would wait {computed:?} but the row would carry {declared:?}"
            ));
        }
    }
    None
}

/// Everything a worker can run, and everything the scheduler runs on a clock.
///
/// ```no_run
/// use moso_jobs::{Job, JobRegistry};
///
/// fn registry<A: Job, B: Job>() -> JobRegistry {
///     JobRegistry::new().register::<A>().register::<B>()
/// }
/// ```
#[derive(Debug, Default)]
pub struct JobRegistry {
    /// Registered jobs, by wire name.
    jobs: std::collections::BTreeMap<&'static str, RegisteredJob>,
    /// Everything on a clock.
    schedules: Vec<Schedule>,
    /// Names registered twice, held for [`JobRegistry::validate`] rather than
    /// silently overwriting: two jobs sharing a name means one never runs, and
    /// finding that out in production is expensive.
    duplicates: Vec<(&'static str, &'static str, &'static str)>,
}

impl JobRegistry {
    /// An empty registry.
    ///
    /// ```
    /// use moso_jobs::JobRegistry;
    ///
    /// assert!(JobRegistry::new().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a job.
    ///
    /// Registering the same wire name twice is a *boot* error, not a silent
    /// overwrite: two jobs sharing a name means one of them never runs, and
    /// finding that out in production is expensive. The error is collected by
    /// [`validate`](JobRegistry::validate).
    ///
    /// ```no_run
    /// # use moso_jobs::{Job, JobRegistry};
    /// # fn f<J: Job>(r: JobRegistry) { let _ = r.register::<J>(); }
    /// ```
    #[must_use]
    pub fn register<J: Job>(mut self) -> Self {
        let entry = RegisteredJob::of::<J>();
        if let Some(existing) = self.jobs.get(J::NAME) {
            self.duplicates
                .push((J::NAME, existing.type_name, entry.type_name));
            return self;
        }
        self.jobs.insert(J::NAME, entry);
        self
    }

    /// Run something on a clock.
    ///
    /// ```no_run
    /// # use moso_jobs::{JobRegistry, Schedule};
    /// # fn f(r: JobRegistry, s: Schedule) { let _ = r.schedule(s); }
    /// ```
    #[must_use]
    pub fn schedule(mut self, schedule: impl Into<Schedule>) -> Self {
        self.schedules.push(schedule.into());
        self
    }

    /// Look a job up by wire name.
    ///
    /// ```no_run
    /// # use moso_jobs::{JobRegistry, RegisteredJob};
    /// # fn f(r: &JobRegistry) { let _: Option<&RegisteredJob> = r.get("send_welcome_email"); }
    /// ```
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&RegisteredJob> {
        self.jobs.get(name)
    }

    /// Every registered job.
    ///
    /// ```no_run
    /// # use moso_jobs::{JobRegistry, RegisteredJob};
    /// # fn f(r: &JobRegistry) { let _: Vec<&RegisteredJob> = r.all().collect(); }
    /// ```
    pub fn all(&self) -> impl Iterator<Item = &RegisteredJob> {
        self.jobs.values()
    }

    /// Every distinct queue any registered job runs on.
    ///
    /// What a worker started with no `--queues` listens to, and what
    /// [`Jobs::stats`](crate::Jobs::stats) reports on.
    ///
    /// ```no_run
    /// # use moso_jobs::JobRegistry;
    /// # fn f(r: &JobRegistry) { let _: Vec<String> = r.queues(); }
    /// ```
    #[must_use]
    pub fn queues(&self) -> Vec<String> {
        let mut seen = std::collections::BTreeSet::new();
        let mut queues = Vec::new();
        for job in self.jobs.values() {
            if seen.insert(job.queue) {
                queues.push(job.queue.to_owned());
            }
        }
        // A registry with no jobs still has a queue to listen to, because a
        // worker that listens to nothing looks identical to one that is broken.
        if queues.is_empty() {
            queues.push(crate::DEFAULT_QUEUE.to_owned());
        }
        queues
    }

    /// Everything on a clock.
    ///
    /// ```no_run
    /// # use moso_jobs::{JobRegistry, Schedule};
    /// # fn f(r: &JobRegistry) { let _: &[Schedule] = r.schedules(); }
    /// ```
    #[must_use]
    pub fn schedules(&self) -> &[Schedule] {
        &self.schedules
    }

    /// The closest registered name to `name`, for a "did you mean".
    ///
    /// ```
    /// # use moso_jobs::{Job, JobCtx, JobRegistry, Result};
    /// # struct Reindex;
    /// # impl Job for Reindex {
    /// #     type Args = ();
    /// #     const NAME: &'static str = "reindex_search";
    /// #     async fn run(_a: (), _c: JobCtx) -> Result { Ok(()) }
    /// # }
    /// let registry = JobRegistry::new().register::<Reindex>();
    /// assert_eq!(registry.suggest("reindex_serch"), Some("reindex_search"));
    /// assert_eq!(registry.suggest("completely_different"), None);
    /// ```
    #[must_use]
    pub fn suggest(&self, name: &str) -> Option<&'static str> {
        // The same rule `moso_core::error::did_you_mean` uses: a suggestion
        // further away than a third of the name is noise, and noise in a
        // diagnostic is worse than silence.
        let budget = (name.chars().count() / 3).max(1);
        self.jobs
            .keys()
            .map(|candidate| (levenshtein(name, candidate), *candidate))
            .filter(|(distance, _)| *distance <= budget)
            .min_by_key(|(distance, candidate)| (*distance, *candidate))
            .map(|(_, candidate)| candidate)
    }

    /// Everything wrong with this registry, as boot problems.
    ///
    /// Called by `App::build()`, so all the problems are reported at once
    /// rather than one per restart: a duplicate wire name, a schedule naming an
    /// unregistered job, a cron expression that does not parse, a retry budget
    /// of zero.
    ///
    /// ```
    /// # use moso_jobs::JobRegistry;
    /// assert!(JobRegistry::new().validate().is_empty());
    /// ```
    /// A job whose timeout outlives a worker's lease is deliberately **not**
    /// reported: [`Worker`](crate::Worker) renews the lease automatically at a
    /// third of its length for as long as the job runs, which is what makes a
    /// five-minute default timeout safe under a sixty-second default lease.
    #[must_use]
    pub fn validate(&self) -> BootErrors {
        let mut errors = BootErrors::new();

        for (name, first, second) in &self.duplicates {
            errors.push(BootError::Other {
                message: format!("two jobs share the wire name `{name}`"),
                notes: vec![
                    format!("first        {first}"),
                    format!("second       {second}"),
                    "one of them would never run, and which one is an implementation detail \
                     of the registration order"
                        .to_owned(),
                ],
                fix: Some(format!(
                    "give one of them a different name: `#[job(name = \"…\")]`, or change the \
                     function name `{name}` is derived from"
                )),
            });
        }

        for job in self.jobs.values() {
            if let Some(detail) = &job.inconsistent_backoff {
                errors.push(BootError::Other {
                    message: format!(
                        "job `{}` overrides `backoff()` without matching `BACKOFF`",
                        job.name
                    ),
                    notes: vec![
                        format!("job          {}", job.type_name),
                        detail.clone(),
                        "the row carries `BACKOFF`, so the override would be silently ignored \
                         at retry time"
                            .to_owned(),
                    ],
                    fix: Some(
                        "set `const BACKOFF: Backoff = …;` to the same ladder, and delete the \
                         `fn backoff` override — the default already delegates to it"
                            .to_owned(),
                    ),
                });
            }

            if job.retry.max_attempts() == 0 {
                errors.push(BootError::Other {
                    message: format!("job `{}` has a retry budget of zero", job.name),
                    notes: vec![
                        format!("job          {}", job.type_name),
                        "a budget of zero means the job never runs at all, not that it never \
                         retries"
                            .to_owned(),
                    ],
                    fix: Some(
                        "`retries = 0` is spelled `retries = 1`: one attempt, no retries"
                            .to_owned(),
                    ),
                });
            }
        }

        let mut seen_schedules = std::collections::BTreeSet::new();
        for schedule in &self.schedules {
            if let Some(detail) = schedule.error() {
                errors.push(BootError::Other {
                    message: format!("the schedule for `{}` does not parse", schedule.job()),
                    notes: vec![detail.to_owned()],
                    fix: Some(
                        "a cron expression is five fields — minute hour day-of-month month \
                         day-of-week — and a timezone is an IANA name like `Europe/Rome`"
                            .to_owned(),
                    ),
                });
                continue;
            }

            if !self.jobs.contains_key(schedule.job()) {
                let mut notes = vec![format!("schedule     {}", schedule.id())];
                if let Some(suggestion) = self.suggest(schedule.job()) {
                    notes.push(format!("did you mean  `{suggestion}`?"));
                }
                errors.push(BootError::Other {
                    message: format!("job `{}` is scheduled but not registered", schedule.job()),
                    notes,
                    fix: Some(format!(
                        "add `.register::<{}>()` before the `.schedule(..)` that names it",
                        schedule.job()
                    )),
                });
            }

            if !seen_schedules.insert(schedule.id().clone()) {
                errors.push(BootError::Other {
                    message: format!("two schedules share the key `{}`", schedule.id()),
                    notes: vec![
                        "the key is the leader-election lease, so the two would fight over one \
                         lease and only one of them would ever fire"
                            .to_owned(),
                    ],
                    fix: Some(
                        "two schedules of the same job need different expressions; if they are \
                         genuinely the same, delete one"
                            .to_owned(),
                    ),
                });
            }
        }

        errors
    }

    /// How many jobs are registered.
    ///
    /// ```
    /// # use moso_jobs::JobRegistry;
    /// assert_eq!(JobRegistry::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    /// Whether nothing is registered.
    ///
    /// ```
    /// # use moso_jobs::JobRegistry;
    /// assert!(JobRegistry::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}

impl core::fmt::Debug for RegisteredJob {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RegisteredJob")
            .field("name", &self.name)
            .field("queue", &self.queue)
            .finish_non_exhaustive()
    }
}

/// The edit distance between two names, for a "did you mean".
///
/// Two rows rather than a full matrix: job names are short, and this runs once
/// per registered job on a path that only executes when something is already
/// wrong.
fn levenshtein(left: &str, right: &str) -> usize {
    let right_chars: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right_chars.len()).collect();
    let mut current = vec![0_usize; right_chars.len() + 1];

    for (i, left_char) in left.chars().enumerate() {
        current[0] = i + 1;
        for (j, right_char) in right_chars.iter().enumerate() {
            let substitution = previous[j] + usize::from(left_char != *right_char);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        core::mem::swap(&mut previous, &mut current);
    }
    previous[right_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job whose payload is a struct, so the poison-payload path has
    /// something to fail on.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Args {
        user_id: u64,
    }

    struct Welcome;
    impl Job for Welcome {
        type Args = Args;
        const NAME: &'static str = "send_welcome_email";
        const QUEUE: &'static str = "mail";
        const RETRIES: u32 = 5;
        async fn run(_args: Args, _ctx: JobCtx) -> Result {
            Ok(())
        }
    }

    struct Cleanup;
    impl Job for Cleanup {
        type Args = ();
        const NAME: &'static str = "nightly_cleanup";
        async fn run(_args: (), _ctx: JobCtx) -> Result {
            Ok(())
        }
    }

    /// A second job claiming a registered wire name, to prove the duplicate is
    /// reported rather than silently overwriting.
    struct Impostor;
    impl Job for Impostor {
        type Args = ();
        const NAME: &'static str = "send_welcome_email";
        async fn run(_args: (), _ctx: JobCtx) -> Result {
            Ok(())
        }
    }

    /// The registry reads the constants off the type, so a job's declared queue
    /// and budget are what a worker will actually use.
    #[test]
    fn a_registration_carries_the_types_constants() {
        let registry = JobRegistry::new()
            .register::<Welcome>()
            .register::<Cleanup>();
        assert_eq!(registry.len(), 2);

        let welcome = registry.get("send_welcome_email").expect("registered");
        assert_eq!(welcome.queue(), "mail");
        assert_eq!(welcome.retry().max_attempts(), 5);
        assert_eq!(welcome.timeout(), crate::DEFAULT_TIMEOUT);
        assert!(!welcome.serial());
        assert!(welcome.type_name().ends_with("Welcome"));

        assert!(registry.get("no_such_job").is_none());
    }

    /// A worker with no `--queues` listens to every queue the registry knows,
    /// deduplicated and in a stable order.
    #[test]
    fn the_queue_list_is_deduplicated_and_never_empty() {
        let registry = JobRegistry::new()
            .register::<Welcome>()
            .register::<Cleanup>();
        let mut queues = registry.queues();
        queues.sort();
        assert_eq!(queues, vec!["default".to_owned(), "mail".to_owned()]);

        assert_eq!(JobRegistry::new().queues(), vec!["default".to_owned()]);
    }

    /// Two jobs sharing a wire name means one never runs. Reporting it at boot
    /// is the whole reason registration is explicit.
    #[test]
    fn a_duplicate_wire_name_is_a_boot_error_naming_both_types() {
        let registry = JobRegistry::new()
            .register::<Welcome>()
            .register::<Impostor>();
        let errors = registry.validate();
        assert_eq!(errors.len(), 1, "{}", errors.render(false));

        let rendered = errors.render(false);
        assert!(
            rendered.contains("share the wire name `send_welcome_email`"),
            "{rendered}"
        );
        assert!(rendered.contains("Welcome"), "{rendered}");
        assert!(rendered.contains("Impostor"), "{rendered}");
        assert!(rendered.contains("#[job(name ="), "{rendered}");

        // The first registration wins, so the report is about the second.
        assert!(
            registry
                .get("send_welcome_email")
                .unwrap()
                .type_name()
                .ends_with("Welcome")
        );
    }

    /// Acceptance criterion 3, at boot: a schedule naming a job nobody
    /// registered names the fix.
    #[test]
    fn a_schedule_for_an_unregistered_job_names_the_registration_to_add() {
        let registry = JobRegistry::new()
            .register::<Welcome>()
            .schedule(crate::Cron::new::<Cleanup>("0 3 * * *", ()));

        let errors = registry.validate();
        let rendered = errors.render(false);
        assert!(
            rendered.contains("`nightly_cleanup` is scheduled but not registered"),
            "{rendered}"
        );
        assert!(
            rendered.contains(".register::<nightly_cleanup>()"),
            "{rendered}"
        );
    }

    /// A cron expression that does not parse must not become a scheduler that
    /// silently never fires.
    #[test]
    fn an_unparseable_expression_is_reported_at_boot() {
        let registry = JobRegistry::new()
            .register::<Cleanup>()
            .schedule(crate::Cron::new::<Cleanup>("not a cron expression", ()));

        let rendered = registry.validate().render(false);
        assert!(rendered.contains("does not parse"), "{rendered}");
        assert!(rendered.contains("five fields"), "{rendered}");
    }

    /// `retries = 0` reads as "no retries" and means "never runs". Catching the
    /// off-by-one at boot is cheaper than catching it in a queue that never
    /// drains.
    #[test]
    fn a_zero_retry_budget_is_reported_as_the_off_by_one_it_is() {
        struct Never;
        impl Job for Never {
            type Args = ();
            const NAME: &'static str = "never";
            const RETRIES: u32 = 0;
            async fn run(_args: (), _ctx: JobCtx) -> Result {
                Ok(())
            }
        }

        let rendered = JobRegistry::new()
            .register::<Never>()
            .validate()
            .render(false);
        assert!(rendered.contains("retry budget of zero"), "{rendered}");
        assert!(
            rendered.contains("`retries = 0` is spelled `retries = 1`"),
            "{rendered}"
        );
    }

    /// Overriding `fn backoff` without `BACKOFF` would make the row promise one
    /// ladder and the type document another, and the row wins silently.
    #[test]
    fn a_backoff_override_that_disagrees_with_the_constant_is_caught() {
        struct Divergent;
        impl Job for Divergent {
            type Args = ();
            const NAME: &'static str = "divergent";
            fn backoff(_attempt: u32) -> Duration {
                Duration::from_secs(1)
            }
            async fn run(_args: (), _ctx: JobCtx) -> Result {
                Ok(())
            }
        }

        let rendered = JobRegistry::new()
            .register::<Divergent>()
            .validate()
            .render(false);
        assert!(
            rendered.contains("without matching `BACKOFF`"),
            "{rendered}"
        );
        assert!(rendered.contains("const BACKOFF"), "{rendered}");
    }

    /// Every problem at once, not one per restart. A registry with four
    /// mistakes reports four.
    #[test]
    fn every_problem_is_reported_in_one_pass() {
        let registry = JobRegistry::new()
            .register::<Welcome>()
            .register::<Impostor>()
            .schedule(crate::Cron::new::<Cleanup>("0 3 * * *", ()))
            .schedule(crate::Cron::new::<Cleanup>("nope", ()));

        let errors = registry.validate();
        assert_eq!(errors.len(), 3, "{}", errors.render(false));
    }

    /// The "did you mean" is what turns a typo into a one-line fix, and a
    /// suggestion that fires on anything is noise.
    #[test]
    fn the_suggestion_fires_on_a_typo_and_not_on_a_different_name() {
        let registry = JobRegistry::new()
            .register::<Welcome>()
            .register::<Cleanup>();
        assert_eq!(
            registry.suggest("send_welcome_emai"),
            Some("send_welcome_email")
        );
        assert_eq!(registry.suggest("nightly_cleanup"), Some("nightly_cleanup"));
        assert_eq!(registry.suggest("generate_invoice"), None);
    }

    /// The poison-payload guard: a payload that will not deserialise fails with
    /// `Error::Payload`, which skips the retry budget entirely.
    #[tokio::test]
    async fn a_payload_that_does_not_deserialise_goes_straight_to_the_dead_letter() {
        let registry = JobRegistry::new().register::<Welcome>();
        let job = registry.get("send_welcome_email").expect("registered");

        let error = job
            .run(
                serde_json::json!({ "wrong": "shape" }),
                crate::test_support::ctx(),
            )
            .await
            .expect_err("the payload does not match");
        assert!(matches!(error, crate::Error::Payload { .. }));
        assert!(error.skips_retries());
        assert!(!error.retryable());
    }

    /// `on_failure` sees the payload, which is the whole reason it is worth
    /// having: an alert with no identifier is an alert nobody can act on.
    #[tokio::test]
    async fn the_failure_hook_sees_the_payload_and_the_error() {
        static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        struct Failing;
        impl Job for Failing {
            type Args = Args;
            const NAME: &'static str = "failing";
            async fn run(_args: Args, _ctx: JobCtx) -> Result {
                Err(crate::Error::retry("upstream said no"))
            }
            async fn on_failure(args: &Args, error: &crate::Error, _ctx: &JobCtx) {
                assert!(error.retryable());
                SEEN.store(args.user_id, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let registry = JobRegistry::new().register::<Failing>();
        let job = registry.get("failing").expect("registered");
        let error = job
            .run(
                serde_json::json!({ "user_id": 42 }),
                crate::test_support::ctx(),
            )
            .await
            .expect_err("the body failed");
        assert!(error.retryable());
        assert_eq!(SEEN.load(std::sync::atomic::Ordering::SeqCst), 42);
    }

    /// Edit distance, since the suggestion is only as good as this.
    #[test]
    fn edit_distance_counts_what_it_says_it_counts() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "abd"), 1);
        assert_eq!(levenshtein("abc", "ab"), 1);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}
