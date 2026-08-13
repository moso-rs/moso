//! The enqueueing actor's identity, carried from the request into the job.
//!
//! A background job has no session and no credential of its own, so "who is this
//! running as?" has no answer unless one is carried in — and an audit trail that
//! records every deletion except the ones a job performed has a hole exactly
//! where the automated, unattended, high-volume actions are. This module closes
//! it: the identity of whoever enqueued the job travels on the row and is
//! restored when a worker runs it, so a job can be attributed to the person or
//! service that scheduled it however long ago that was.
//!
//! # An opaque string, deliberately
//!
//! What crosses this boundary is an **opaque identity string**, exactly as
//! [`trace`](crate::trace) carries an opaque `traceparent`. This crate does not
//! parse it, does not depend on `moso-authz`, and never will: authorization is a
//! battery, and a jobs crate that depended on it would decide for every
//! application that wants jobs that it must also compile an authorization
//! engine. `moso-authz` owns the meaning — `ActorIdentity::to_wire` produces the
//! string an application scopes here, and `ActorIdentity::from_wire` reads it
//! back on the far side — and this crate owns only the *propagation*: the
//! task-local, the capture at enqueue, and the restoration at execution.
//!
//! # What is, and is not, carried — a security note
//!
//! Only the **identity** travels: a stable subject id, the actor kind, the
//! scope. Never a credential, never a session token, never a resolved
//! permission set. A job therefore runs with the enqueuer's *identity for
//! audit*, not with their live authority: a worker that needs to know what the
//! subject may do **now** re-resolves it, so a permission revoked between
//! enqueue and execution is already gone by the next attempt. Carrying live
//! credentials would turn a queued row into a bearer token with the retention of
//! a database backup, which is exactly the artefact you do not want on disk.
//!
//! ```
//! use moso_jobs::actor;
//!
//! # #[tokio::main(flavor = "current_thread")] async fn main() {
//! assert!(actor::current().is_none());
//!
//! // An application scopes the enqueueing actor's `ActorIdentity::to_wire()`
//! // string; here a literal stands in for it.
//! let seen = actor::scope("usr_42".to_owned(), async { actor::current() }).await;
//! assert_eq!(seen.as_deref(), Some("usr_42"));
//! # }
//! ```

tokio::task_local! {
    /// The identity of whoever is acting on this task.
    ///
    /// A task-local rather than a thread-local for the same reason
    /// [`trace`](crate::trace) uses one: a worker runs many jobs concurrently on
    /// one runtime thread, and a thread-local would leak one enqueuer's identity
    /// into the next job's enqueues.
    static CURRENT: String;
}

/// The identity string of whatever is acting on this task, when there is one.
///
/// What [`EnqueueBuilder`](crate::EnqueueBuilder) captures onto the row, so a
/// job enqueued from inside a request is attributed to that request's actor.
///
/// ```
/// use moso_jobs::actor;
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// assert!(actor::current().is_none());
/// let inside = actor::scope("svc_ci".to_owned(), async { actor::current() }).await;
/// assert_eq!(inside.as_deref(), Some("svc_ci"));
/// # }
/// ```
#[must_use]
pub fn current() -> Option<String> {
    CURRENT.try_with(|identity| identity.clone()).ok()
}

/// Run `future` with `identity` as the current acting identity.
///
/// The one seam an application needs on the enqueue side: wrap the request
/// handler (or the whole service) in this with the resolved actor's
/// `ActorIdentity::to_wire()` string, and every enqueue underneath carries that
/// identity onto its row. It mirrors [`trace::scope`](crate::trace::scope)
/// exactly.
///
/// ```
/// use moso_jobs::actor;
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let attributed = actor::scope("usr_1".to_owned(), async { actor::current().is_some() }).await;
/// assert!(attributed);
/// # }
/// ```
pub async fn scope<F: Future>(identity: String, future: F) -> F::Output {
    CURRENT.scope(identity, future).await
}

/// Run a job body with the identity its row carried, when it carried one.
///
/// The restoration half, called by the worker. A row with an identity runs its
/// body inside [`scope`], so a further enqueue from within the job is attributed
/// to the same actor — the chain that lets one subject's action propagate
/// through a fan-out of jobs. A row with none runs the body unscoped rather than
/// inventing an actor: an unattributed job is honest, an actor made up on the
/// worker is not.
pub(crate) async fn scope_for_job<F: Future>(identity: Option<String>, future: F) -> F::Output {
    match identity {
        Some(identity) => CURRENT.scope(identity, future).await,
        None => future.await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scope is what an enqueue reads, and it must not leak out of the
    /// future it wraps — a leaked identity attributes the next request's work to
    /// the previous request's actor.
    #[tokio::test(flavor = "current_thread")]
    async fn a_scope_sets_the_identity_and_does_not_leak() {
        assert!(current().is_none());
        let inside = scope("usr_7".to_owned(), async { current() }).await;
        assert_eq!(inside.as_deref(), Some("usr_7"));
        assert!(current().is_none(), "the scope does not outlive its future");
    }

    /// Two jobs running concurrently on one runtime thread must not see each
    /// other's enqueuer — the reason this is a task-local and not a
    /// thread-local.
    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_tasks_do_not_share_an_identity() {
        let (first, second) = tokio::join!(
            scope("usr_1".to_owned(), async {
                tokio::task::yield_now().await;
                current()
            }),
            scope("usr_2".to_owned(), async {
                tokio::task::yield_now().await;
                current()
            })
        );
        assert_eq!(first.as_deref(), Some("usr_1"));
        assert_eq!(second.as_deref(), Some("usr_2"));
    }

    /// A job whose row carried no identity runs unscoped rather than being
    /// attributed to an invented actor.
    #[tokio::test(flavor = "current_thread")]
    async fn a_job_without_an_identity_runs_unscoped() {
        let seen = scope_for_job(None, async { current() }).await;
        assert!(seen.is_none());

        let attributed = scope_for_job(Some("svc_1".to_owned()), async { current() }).await;
        assert_eq!(attributed.as_deref(), Some("svc_1"));
    }
}
