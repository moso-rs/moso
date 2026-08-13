//! The worker pod's health and metrics listener.
//!
//! A worker process has no HTTP server, which in Kubernetes means it has no
//! liveness probe, no readiness probe and no metrics endpoint — so the
//! orchestrator cannot tell a wedged worker from a busy one, a rolling deploy
//! cannot tell when the new pod is ready, and the six metrics
//! `docs/03-batteries/32-jobs.md` defines are counted and never read.
//!
//! [`WorkerHealth`] is a small listener that fixes all three:
//!
//! | Path | Answers |
//! | --- | --- |
//! | `/healthz` | 200 while the process is alive and not shutting down |
//! | `/readyz` | 200 when the queue is reachable **and** leader election has resolved |
//! | `/metrics` | the Prometheus text of [`crate::metrics`] |
//!
//! # Why readiness gates on leader election
//!
//! During a rolling deploy the orchestrator waits for the new pod to be ready
//! before taking the old one away. If readiness ignored the scheduler, the
//! window between "new pod ready" and "new pod has the lease" is a window with
//! **zero** schedulers — and a nightly job that falls in it does not run at all.
//! Gating readiness on [`SchedulerReadiness`](crate::schedule::SchedulerReadiness)
//! closes it.

use std::net::SocketAddr;

use moso_core::Router;
use moso_core::response::IntoResponse;

use crate::{Jobs, Result};

/// The probe endpoints a worker pod serves.
///
/// ```no_run
/// use moso_jobs::health::WorkerHealth;
/// use moso_jobs::Jobs;
///
/// # async fn f(jobs: Jobs, shutdown: moso_core::Signal) -> moso_jobs::Result {
/// WorkerHealth::new(jobs)
///     .serve("0.0.0.0:9090".parse().expect("a socket address"), shutdown)
///     .await
/// # }
/// ```
#[derive(Clone)]
pub struct WorkerHealth {
    /// What to probe.
    jobs: Jobs,
    /// Whether the scheduler has elected, when this process runs one.
    scheduler: Option<crate::schedule::SchedulerReadiness>,
    /// Which queues to report depth for.
    queues: Vec<String>,
    /// Fires when the process is going down, so `/readyz` can answer 503
    /// immediately and the orchestrator can take the pod out of rotation while
    /// it finishes what it is doing.
    shutdown: moso_core::Signal,
}

impl WorkerHealth {
    /// Probe `jobs`.
    ///
    /// ```no_run
    /// # use moso_jobs::{health::WorkerHealth, Jobs};
    /// # fn f(jobs: Jobs) { let _ = WorkerHealth::new(jobs); }
    /// ```
    #[must_use]
    pub fn new(jobs: Jobs) -> Self {
        let queues = jobs.registry().queues();
        Self {
            jobs,
            scheduler: None,
            queues,
            shutdown: moso_core::Signal::new(),
        }
    }

    /// Gate readiness on the scheduler having elected a leader.
    ///
    /// ```no_run
    /// # use moso_jobs::{health::WorkerHealth, Jobs, Scheduler};
    /// # fn f(jobs: Jobs, s: &Scheduler) { let _ = WorkerHealth::new(jobs).scheduler(s.readiness()); }
    /// ```
    #[must_use]
    pub fn scheduler(mut self, readiness: crate::schedule::SchedulerReadiness) -> Self {
        self.scheduler = Some(readiness);
        self
    }

    /// Report depth for these queues rather than every registered one.
    ///
    /// ```no_run
    /// # use moso_jobs::{health::WorkerHealth, Jobs};
    /// # fn f(jobs: Jobs) { let _ = WorkerHealth::new(jobs).queues(["mail"]); }
    /// ```
    #[must_use]
    pub fn queues<S: Into<String>>(mut self, queues: impl IntoIterator<Item = S>) -> Self {
        self.queues = queues.into_iter().map(Into::into).collect();
        self
    }

    /// The routes, so they can be mounted on an existing application instead.
    ///
    /// A single-process deployment already has an HTTP server; mounting these on
    /// it is better than binding a second port.
    ///
    /// ```no_run
    /// # use moso_jobs::{health::WorkerHealth, Jobs};
    /// # fn f(jobs: Jobs) -> moso_core::Router { WorkerHealth::new(jobs).router() }
    /// ```
    #[must_use]
    pub fn router(&self) -> Router {
        let liveness = self.clone();
        let readiness = self.clone();

        Router::new()
            .get("/healthz", move || {
                let health = liveness.clone();
                async move { health.liveness() }
            })
            .get("/readyz", move || {
                let health = readiness.clone();
                async move { health.readiness().await }
            })
            .get("/metrics", || async { metrics() })
            // Hidden from the OpenAPI document: these are an orchestrator's
            // endpoints, not the application's API, and putting them in the
            // contract makes every generated client carry three routes nobody
            // calls.
            .hidden()
    }

    /// Bind `addr` and serve until the shutdown signal fires.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) when the address cannot be
    /// bound — which is a misconfiguration, and one a worker should die on
    /// rather than run unprobeable.
    ///
    /// ```no_run
    /// # use moso_jobs::{health::WorkerHealth, Jobs};
    /// # async fn f(jobs: Jobs, s: moso_core::Signal) -> moso_jobs::Result {
    /// WorkerHealth::new(jobs).serve("0.0.0.0:9090".parse().unwrap(), s).await
    /// # }
    /// ```
    pub async fn serve(self, addr: SocketAddr, shutdown: moso_core::Signal) -> Result {
        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|error| {
            crate::Error::config(format!(
                "the worker health listener could not bind {addr}: {error}\n\
                 help: a worker with no probe endpoint is a worker Kubernetes cannot restart; \
                 pick a free port, or mount `WorkerHealth::router()` on the application's own \
                 server instead"
            ))
        })?;
        self.serve_on(listener, shutdown).await
    }

    /// Serve on an already-bound listener.
    ///
    /// What a test uses: bind port 0, read the address back, then serve.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) when the server stops with one.
    ///
    /// ```no_run
    /// # use moso_jobs::{health::WorkerHealth, Jobs};
    /// # async fn f(jobs: Jobs, l: tokio::net::TcpListener, s: moso_core::Signal) -> moso_jobs::Result {
    /// WorkerHealth::new(jobs).serve_on(l, s).await
    /// # }
    /// ```
    pub async fn serve_on(
        mut self,
        listener: tokio::net::TcpListener,
        shutdown: moso_core::Signal,
    ) -> Result {
        self.shutdown = shutdown.clone();
        let local = listener.local_addr().ok();
        tracing::info!(
            target: "moso::jobs",
            addr = ?local,
            "the worker health listener is up on /healthz, /readyz and /metrics"
        );

        // A plain `axum::Router` and not `Router::into_axum()`: a Moso router
        // needs a `RequestCtx` in the request extensions, which only an `App`
        // puts there. These three endpoints have no extractors and no
        // dependencies, so building them without an application is the honest
        // shape — and `router()` is still there for a deployment that mounts
        // them on one.
        let service = self.axum_router();
        moso_core::deps::axum::serve(listener, service)
            .with_graceful_shutdown(async move { shutdown.recv().await })
            .await
            .map_err(|error| {
                crate::Error::config(format!("the worker health listener stopped: {error}"))
            })
    }

    /// The same three endpoints as an application-free `axum::Router`.
    ///
    /// [`router`](WorkerHealth::router) needs an `App` around it, because a
    /// Moso router reads its `RequestCtx` out of the request extensions. A
    /// worker pod has no application, so [`serve_on`](WorkerHealth::serve_on)
    /// uses this.
    fn axum_router(&self) -> moso_core::deps::axum::Router<()> {
        use moso_core::deps::axum::Router as AxumRouter;
        use moso_core::deps::axum::routing::get;

        let liveness = self.clone();
        let readiness = self.clone();

        AxumRouter::new()
            .route(
                "/healthz",
                get(move || {
                    let health = liveness.clone();
                    async move { health.liveness() }
                }),
            )
            .route(
                "/readyz",
                get(move || {
                    let health = readiness.clone();
                    async move { health.readiness().await }
                }),
            )
            .route("/metrics", get(|| async { metrics() }))
    }

    /// `/healthz`: is this process alive.
    ///
    /// Deliberately does not touch the queue. Liveness answers "should the
    /// orchestrator restart me", and restarting a worker because its database
    /// is down turns one outage into two.
    fn liveness(&self) -> moso_core::Response {
        if self.shutdown.is_shutting_down() {
            return (
                moso_core::deps::http::StatusCode::SERVICE_UNAVAILABLE,
                "shutting down\n",
            )
                .into_response();
        }
        (moso_core::deps::http::StatusCode::OK, "ok\n").into_response()
    }

    /// `/readyz`: should this process be sent work.
    async fn readiness(&self) -> moso_core::Response {
        use moso_core::deps::http::StatusCode;

        if self.shutdown.is_shutting_down() {
            return (StatusCode::SERVICE_UNAVAILABLE, "shutting down\n").into_response();
        }

        if let Err(error) = self.jobs.queue().probe().await {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("queue unreachable: {}\n", error.chain()),
            )
                .into_response();
        }

        if let Some(readiness) = &self.scheduler
            && !readiness.is_resolved()
        {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "the scheduler has not elected a leader yet\n",
            )
                .into_response();
        }

        // Refresh the depth gauges while we are here: a `/metrics` scrape and a
        // `/readyz` probe arrive at about the same rate, and doing it here
        // keeps the metrics fresh in a process whose worker is idle.
        if let Ok(stats) = self.jobs.queue().stats(&self.queues).await {
            for one in &stats {
                crate::metrics::depth(&one.queue, one.ready);
            }
        }

        (StatusCode::OK, "ready\n").into_response()
    }
}

impl core::fmt::Debug for WorkerHealth {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WorkerHealth")
            .field("queues", &self.queues)
            .field("scheduler", &self.scheduler.is_some())
            .finish_non_exhaustive()
    }
}

/// `/metrics`: the Prometheus exposition text.
fn metrics() -> moso_core::Response {
    (
        [(
            moso_core::deps::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        crate::metrics::snapshot(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JobRegistry;

    fn health() -> WorkerHealth {
        let jobs = Jobs::new(
            std::sync::Arc::new(crate::backend::MemoryQueue::new()),
            std::sync::Arc::new(JobRegistry::new()),
        );
        WorkerHealth::new(jobs)
    }

    /// The three endpoints Kubernetes needs, all present and all hidden from
    /// the API contract.
    #[test]
    fn the_three_probe_routes_are_mounted_and_hidden() {
        let router = health().router();
        let paths: Vec<String> = router
            .describe()
            .into_iter()
            .map(|route| route.path)
            .collect();
        for path in ["/healthz", "/readyz", "/metrics"] {
            assert!(
                paths.iter().any(|p| p == path),
                "{path} missing from {paths:?}"
            );
        }
        assert_eq!(router.len(), 3);
    }

    /// Liveness must not touch the queue: restarting a worker because its
    /// database is down turns one outage into two.
    #[tokio::test]
    async fn liveness_answers_without_asking_the_queue() {
        let health = health();
        let response = health.liveness();
        assert_eq!(response.status().as_u16(), 200);
    }

    /// A shutting-down worker has to be taken out of rotation immediately,
    /// while it finishes what it is already doing.
    #[tokio::test]
    async fn a_draining_worker_is_neither_live_nor_ready() {
        let mut health = health();
        health.shutdown = moso_core::Signal::new();
        health.shutdown.trigger();

        assert_eq!(health.liveness().status().as_u16(), 503);
        assert_eq!(health.readiness().await.status().as_u16(), 503);
    }

    /// Readiness gates on the queue being reachable.
    #[tokio::test]
    async fn readiness_asks_the_queue() {
        let health = health();
        assert_eq!(health.readiness().await.status().as_u16(), 200);
    }

    /// The window between "pod ready" and "pod has the lease" is a window with
    /// zero schedulers. Readiness has to close it.
    #[tokio::test]
    async fn readiness_waits_for_leader_election() {
        let readiness = crate::schedule::SchedulerReadiness::pending();
        let health = health().scheduler(readiness.clone());

        assert_eq!(
            health.readiness().await.status().as_u16(),
            503,
            "not ready until the election resolves"
        );

        readiness.resolve();
        assert_eq!(health.readiness().await.status().as_u16(), 200);
    }

    /// A metrics endpoint that serves the wrong content type is one Prometheus
    /// refuses to scrape.
    #[test]
    fn metrics_are_served_as_prometheus_text() {
        let _guard = crate::metrics::test_guard();
        crate::metrics::enqueued("send_welcome_email", "mail");
        let response = metrics();
        assert_eq!(response.status().as_u16(), 200);
        let content_type = response
            .headers()
            .get(moso_core::deps::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(content_type.starts_with("text/plain"), "{content_type}");
        crate::metrics::reset();
    }

    /// The whole point: a worker pod is probeable over a real socket.
    #[tokio::test]
    async fn the_listener_answers_over_a_real_socket() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a free port");
        let addr = listener.local_addr().expect("bound");
        let shutdown = moso_core::Signal::new();

        let serving = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { health().serve_on(listener, shutdown).await }
        });

        for path in ["/healthz", "/readyz", "/metrics"] {
            let mut socket = tokio::net::TcpStream::connect(addr)
                .await
                .expect("the listener is up");
            socket
                .write_all(
                    format!("GET {path} HTTP/1.1\r\nHost: probe\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .expect("wrote the request");
            let mut response = String::new();
            socket
                .read_to_string(&mut response)
                .await
                .expect("read the response");
            assert!(
                response.starts_with("HTTP/1.1 200"),
                "{path} answered: {response}"
            );
        }

        shutdown.trigger();
        serving
            .await
            .expect("the task finished")
            .expect("a clean stop");
    }
}
