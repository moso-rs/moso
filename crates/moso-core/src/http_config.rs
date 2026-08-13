//! The framework's own configuration sections.
//!
//! [`HttpConfig`], [`ServerConfig`] and [`TracingConfig`] are plain structs with
//! `Default` impls, handed to the builder at the composition root:
//!
//! ```
//! use moso::config::prelude::*;
//! use moso::http_config::{HttpConfig, ServerConfig};
//! use std::time::Duration;
//!
//! /// Everything this application reads from its environment.
//! #[derive(moso::Config, Clone, Debug)]
//! pub struct AppConfig {
//!     /// Human-readable service name.
//!     #[config(default = "shop")]
//!     pub name: String,
//! }
//!
//! # fn main() {
//! let app = moso::App::new(AppConfig { name: "shop".to_owned() })
//!     .http_config(HttpConfig {
//!         expose_internal_errors: false,
//!         ..HttpConfig::default()
//!     })
//!     .server_config(ServerConfig {
//!         shutdown_grace: Duration::from_secs(25),
//!         ..ServerConfig::default()
//!     });
//! # let _ = app;
//! # }
//! ```
//!
//! These are *not* `#[config(nested)]` sections: `moso-core` cannot depend on
//! `moso-macros`, so it cannot derive [`Config`](crate::Config) for its own
//! types. An application that wants them driven from the same file as its own
//! keys reads them into its config struct and passes them on.

use std::net::SocketAddr;
use std::time::Duration;

use crate::ctx::Limits;

/// Request limits, timeouts and error-disclosure policy.
///
/// The defaults are the documented table in `01-http/12`. Every one of them is
/// a number an operator may need to change, and none of them is a number an
/// application should hard-code.
///
/// ```
/// use moso::http_config::HttpConfig;
///
/// let http = HttpConfig::default();
///
/// // The defaults are the safe ones; a deployment relaxes what it needs to.
/// assert!(!http.expose_internal_errors);
/// assert_eq!(http.limits().body_max, 2 * 1024 * 1024);
///
/// let permissive = HttpConfig { expose_internal_errors: true, ..HttpConfig::default() };
/// assert!(permissive.expose_internal_errors);
/// ```
///
/// Handed to the builder with `App::new(cfg).http_config(...)`.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Maximum request body. `http.body_max`, default 2 MiB.
    pub body_max: usize,
    /// Maximum total multipart payload. Default 32 MiB.
    pub multipart_max: usize,
    /// Maximum single multipart file. Default 16 MiB.
    pub multipart_file_max: usize,
    /// Maximum number of request headers. Default 100.
    pub header_max_count: usize,
    /// Maximum total header bytes. Default 16 KiB.
    pub header_max_bytes: usize,
    /// Maximum request-target length. Default 8 KiB.
    pub uri_max: usize,
    /// Maximum bracket nesting in a query string. Default 8.
    pub query_depth_max: usize,
    /// Maximum JSON nesting depth. Default 64.
    pub json_depth_max: usize,
    /// Per-request timeout. Default 30 s.
    pub timeout: Duration,
    /// Whether a 5xx may carry its `detail` and source chain.
    ///
    /// `false` in every profile. Turning it on is a deliberate decision and is
    /// logged at boot, because it is the difference between an error page and a
    /// disclosure.
    pub expose_internal_errors: bool,
    /// Whether `/docs` and `/openapi.json` are mounted.
    ///
    /// Requires the `openapi` cargo feature as well; this is the runtime half
    /// of the decision.
    pub expose_docs: bool,
    /// Where the documentation UI is mounted.
    pub docs_path: String,
    /// Where the OpenAPI document is served.
    pub openapi_path: String,
    /// Where the liveness probe is mounted. Configurable because some platforms
    /// reserve `/healthz`.
    pub health_path: String,
    /// Where the readiness probe is mounted.
    pub ready_path: String,
    /// Peers whose `X-Forwarded-For` is trusted, in CIDR notation.
    ///
    /// Empty by default: an unconfigured deployment must not believe a header
    /// any client can send, because rate limits and audit logs are built on it.
    pub trusted_proxies: Vec<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        let limits = Limits::DEFAULT;
        Self {
            body_max: limits.body_max,
            multipart_max: limits.multipart_max,
            multipart_file_max: limits.multipart_file_max,
            header_max_count: limits.header_max_count,
            header_max_bytes: limits.header_max_bytes,
            uri_max: limits.uri_max,
            query_depth_max: limits.query_depth_max,
            json_depth_max: limits.json_depth_max,
            timeout: Duration::from_secs(30),
            expose_internal_errors: false,
            expose_docs: true,
            docs_path: "/docs".to_owned(),
            openapi_path: "/openapi.json".to_owned(),
            health_path: "/healthz".to_owned(),
            ready_path: "/readyz".to_owned(),
            trusted_proxies: Vec::new(),
        }
    }
}

impl HttpConfig {
    /// The request limits this configuration implies.
    pub fn limits(&self) -> Limits {
        Limits::from_config(self)
    }

    /// The defaults for a profile.
    ///
    /// `dev` exposes the docs UI; `test` and `production` expose it only if
    /// asked, because a test should exercise the shape that will actually run.
    /// **No profile exposes internal errors** — that is the one switch a
    /// profile is not allowed to flip, because it is the difference between an
    /// error page and a disclosure.
    ///
    /// Every limit keeps its default in every profile, deliberately: a limit
    /// that is looser in `dev` is a limit whose failure mode is only ever seen
    /// in production.
    pub fn for_profile(profile: crate::config::Profile) -> Self {
        Self {
            expose_docs: matches!(profile, crate::config::Profile::Dev),
            ..Self::default()
        }
    }

    /// Warn, loudly, when a deployment has turned disclosure on.
    ///
    /// `expose_internal_errors` is the difference between an error page and a
    /// disclosure, so switching it on outside `dev` is announced at boot rather
    /// than discovered in a bug report. It is a warning and not a boot error
    /// because there are legitimate uses — a staging environment, an incident —
    /// and a framework that refuses is a framework people patch out.
    pub fn warn_if_disclosing(&self, profile: crate::config::Profile) {
        if self.expose_internal_errors && !profile.exposes_errors() {
            tracing::warn!(
                target: "moso::config",
                profile = profile.as_str(),
                "http.expose_internal_errors is ON: 5xx responses will carry their detail, source \
                 chain and backtrace to every client"
            );
        }
    }
}

/// Where and how the listener binds, and how it shuts down.
///
/// ```
/// use moso::http_config::ServerConfig;
/// use std::time::Duration;
///
/// let server = ServerConfig::default();
///
/// // The grace is under the 30 s an orchestrator typically allows before SIGKILL,
/// // deliberately: a longer one is killed mid-drain.
/// assert!(server.shutdown_grace <= Duration::from_secs(30));
///
/// let patient = ServerConfig { shutdown_grace: Duration::from_secs(10), ..ServerConfig::default() };
/// assert_eq!(patient.shutdown_grace, Duration::from_secs(10));
/// ```
///
/// Handed to the builder with `App::new(cfg).server_config(...)`.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The address to bind. `0.0.0.0:3000` by default.
    pub bind: SocketAddr,
    /// How long in-flight requests get after a shutdown signal.
    ///
    /// 25 s, deliberately under the 30 s an orchestrator typically allows
    /// before `SIGKILL`. A grace longer than the kill timeout is the same as no
    /// grace at all, and is a common misconfiguration.
    pub shutdown_grace: Duration,
    /// TCP keep-alive for idle connections.
    pub keep_alive: Option<Duration>,
    /// Whether to accept HTTP/2 without TLS, for a mesh that terminates it.
    pub http2_prior_knowledge: bool,
    /// Whether to set `TCP_NODELAY`. On: a request/response protocol should not
    /// wait for Nagle.
    pub nodelay: bool,
    /// Worker threads. `None` uses Tokio's default, which is the core count.
    pub worker_threads: Option<usize>,
    /// TLS material, when the process terminates TLS itself.
    ///
    /// **Not yet implemented.** The keys exist so `moso config` and
    /// `.env.example` know about them and so a later release can wire them
    /// without a breaking change — but a configuration that sets them is a
    /// **boot error**, not a silent plaintext listener. See
    /// [`ServerConfig::validate`].
    pub tls: Option<TlsConfig>,
}

/// Paths to the certificate and key a TLS listener would use.
///
/// **Not yet implemented**; see [`ServerConfig::tls`]. The overwhelmingly
/// common deployment terminates TLS in front of the process — an ingress
/// controller, a load balancer, a service mesh — and shipping a half-configured
/// TLS stack would be worse than shipping none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    /// PEM-encoded certificate chain.
    pub cert_path: std::path::PathBuf,
    /// PEM-encoded private key.
    pub key_path: std::path::PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 3000)),
            shutdown_grace: Duration::from_secs(25),
            keep_alive: Some(Duration::from_secs(75)),
            http2_prior_knowledge: false,
            nodelay: true,
            worker_threads: None,
            tls: None,
        }
    }
}

impl ServerConfig {
    /// Check the settings that cannot be checked by their types.
    ///
    /// Called by `App::build`, so every problem lands in the one grouped boot
    /// report rather than surfacing as a runtime surprise.
    ///
    /// Two things are checked:
    ///
    /// - **TLS is configured.** Moso does not terminate TLS in this release,
    ///   and a listener that quietly serves plaintext for a configuration that
    ///   says `tls` is exactly the failure this method exists to prevent.
    /// - **The shutdown grace exceeds the usual kill timeout.** A grace longer
    ///   than the orchestrator's `SIGKILL` deadline means the process is killed
    ///   mid-drain, which is the thing the grace existed to prevent.
    pub fn validate(&self, errors: &mut crate::error::BootErrors) {
        if self.tls.is_some() {
            errors.push(crate::error::BootError::Other {
                message: "server.tls is set, but Moso does not terminate TLS".to_owned(),
                notes: vec![
                    "the listener would serve plaintext, which is not what this configuration says"
                        .to_owned(),
                ],
                fix: Some(
                    "terminate TLS in front of the process — an ingress controller, a load \
                     balancer or a service mesh — and remove `server.tls`"
                        .to_owned(),
                ),
            });
        }

        if self.shutdown_grace > USUAL_KILL_TIMEOUT {
            errors.push(crate::error::BootError::Other {
                message: format!(
                    "server.shutdown_grace ({}) is longer than the usual SIGKILL deadline ({})",
                    humantime::format_duration(self.shutdown_grace),
                    humantime::format_duration(USUAL_KILL_TIMEOUT),
                ),
                notes: vec![
                    "the orchestrator will kill the process mid-drain, which is what the grace \
                     existed to prevent"
                        .to_owned(),
                ],
                fix: Some(
                    "set `server.shutdown_grace` under the platform's termination grace period, \
                     or raise that period to match"
                        .to_owned(),
                ),
            });
        }
    }

    /// The defaults for a profile.
    ///
    /// Only the bind address moves: `dev` and `test` listen on the loopback, so
    /// starting a development server does not expose it to whatever network the
    /// laptop is on. `production` binds every interface, because a container
    /// that binds `127.0.0.1` is a container nothing can reach.
    pub fn for_profile(profile: crate::config::Profile) -> Self {
        let bind = match profile {
            crate::config::Profile::Dev | crate::config::Profile::Test => {
                SocketAddr::from(([127, 0, 0, 1], 3000))
            }
            crate::config::Profile::Production => SocketAddr::from(([0, 0, 0, 0], 3000)),
        };
        Self {
            bind,
            ..Self::default()
        }
    }
}

/// The grace period an orchestrator typically allows before `SIGKILL`.
///
/// Kubernetes' `terminationGracePeriodSeconds` defaults to 30 s and every other
/// platform landed near it. [`ServerConfig::validate`] measures against it.
pub const USUAL_KILL_TIMEOUT: Duration = Duration::from_secs(30);

/// Log and trace output.
///
/// ```
/// use moso::http_config::TracingConfig;
///
/// let tracing = TracingConfig::default();
///
/// // A default that is quiet enough to run and loud enough to debug.
/// assert!(!tracing.level.is_empty());
/// ```
#[derive(Debug, Clone)]
pub struct TracingConfig {
    /// The filter directive, `RUST_LOG` syntax. `info` by default.
    pub level: String,
    /// The output format.
    pub format: LogFormat,
    /// Whether to record a span per request.
    ///
    /// Off is a legitimate choice behind a service mesh that already traces,
    /// and the saving is measurable — span creation dominates the default
    /// middleware stack's per-request cost.
    pub spans: bool,
    /// The OTLP endpoint to export traces to. `None` disables export.
    pub otlp_endpoint: Option<String>,
    /// The `service.name` attached to exported spans.
    pub service_name: Option<String>,
    /// The fraction of traces sampled, 0.0 to 1.0.
    pub sample_ratio: f64,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            format: LogFormat::default(),
            spans: true,
            otlp_endpoint: None,
            service_name: None,
            sample_ratio: 1.0,
        }
    }
}

/// How log lines are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-readable, coloured on a TTY. The `dev` default.
    #[default]
    Pretty,
    /// One JSON object per line. What a log aggregator wants, and the
    /// `production` default.
    Json,
    /// Compact single-line text, for a constrained environment.
    Compact,
}

impl LogFormat {
    /// Parse a format name.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "pretty" => Some(LogFormat::Pretty),
            "json" => Some(LogFormat::Json),
            "compact" => Some(LogFormat::Compact),
            _ => None,
        }
    }

    /// The canonical name.
    pub const fn as_str(self) -> &'static str {
        match self {
            LogFormat::Pretty => "pretty",
            LogFormat::Json => "json",
            LogFormat::Compact => "compact",
        }
    }
}

impl TracingConfig {
    /// The defaults for a profile: `pretty` in `dev`, `json` elsewhere.
    ///
    /// The level does not change with the profile. `debug` in development is
    /// tempting and wrong: it trains people to ignore the log, and it means the
    /// first time anyone reads production output is during an incident.
    pub fn for_profile(profile: crate::config::Profile) -> Self {
        Self {
            format: match profile {
                crate::config::Profile::Dev => LogFormat::Pretty,
                crate::config::Profile::Test | crate::config::Profile::Production => {
                    LogFormat::Json
                }
            },
            ..Self::default()
        }
    }
}

impl crate::config::Coerce for LogFormat {
    const TYPE_NAME: &'static str = "log format (`pretty`, `json` or `compact`)";

    fn coerce(
        value: &crate::config::RawValue,
    ) -> core::result::Result<Self, crate::config::CoerceError> {
        let text = value
            .as_text()
            .ok_or_else(|| crate::config::CoerceError::mismatch::<Self>(value))?;
        LogFormat::parse(text.trim().to_lowercase().as_str())
            .ok_or_else(|| crate::config::CoerceError::mismatch::<Self>(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_defaults_match_the_documented_table() {
        let config = HttpConfig::default();
        assert_eq!(config.body_max, 2 * 1024 * 1024);
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert!(!config.expose_internal_errors);
        assert!(config.trusted_proxies.is_empty());
    }

    #[test]
    fn the_shutdown_grace_is_under_the_usual_kill_timeout() {
        assert!(ServerConfig::default().shutdown_grace < Duration::from_secs(30));
    }

    #[test]
    fn log_formats_round_trip() {
        for format in [LogFormat::Pretty, LogFormat::Json, LogFormat::Compact] {
            assert_eq!(LogFormat::parse(format.as_str()), Some(format));
        }
    }

    // ── the documented limit table ───────────────────────────────────────

    /// `docs/01-http/12` § "Limits and safety defaults", row for row.
    #[test]
    fn every_documented_limit_has_its_documented_default() {
        let config = HttpConfig::default();
        assert_eq!(config.body_max, 2 * 1024 * 1024, "body_max");
        assert_eq!(config.multipart_max, 32 * 1024 * 1024, "multipart_max");
        assert_eq!(
            config.multipart_file_max,
            16 * 1024 * 1024,
            "multipart_file_max"
        );
        assert_eq!(config.header_max_count, 100, "header_max_count");
        assert_eq!(config.header_max_bytes, 16 * 1024, "header_max_bytes");
        assert_eq!(config.uri_max, 8 * 1024, "uri_max");
        assert_eq!(config.query_depth_max, 8, "query_depth_max");
        assert_eq!(config.json_depth_max, 64, "json_depth_max");
        assert_eq!(config.timeout, Duration::from_secs(30), "timeout");
    }

    #[test]
    fn the_limits_snapshot_is_the_http_config() {
        assert_eq!(HttpConfig::default().limits(), Limits::DEFAULT);
    }

    // ── profiles change defaults, never semantics ────────────────────────

    #[test]
    fn only_dev_exposes_the_docs_ui_by_default() {
        use crate::config::Profile;
        assert!(HttpConfig::for_profile(Profile::Dev).expose_docs);
        assert!(!HttpConfig::for_profile(Profile::Test).expose_docs);
        assert!(!HttpConfig::for_profile(Profile::Production).expose_docs);
    }

    #[test]
    fn no_profile_exposes_internal_errors() {
        use crate::config::Profile;
        for profile in [Profile::Dev, Profile::Test, Profile::Production] {
            assert!(
                !HttpConfig::for_profile(profile).expose_internal_errors,
                "{profile}"
            );
        }
    }

    #[test]
    fn no_profile_loosens_a_limit() {
        use crate::config::Profile;
        for profile in [Profile::Dev, Profile::Test, Profile::Production] {
            assert_eq!(
                HttpConfig::for_profile(profile).limits(),
                Limits::DEFAULT,
                "{profile}"
            );
        }
    }

    #[test]
    fn tracing_is_pretty_in_dev_and_json_everywhere_else() {
        use crate::config::Profile;
        assert_eq!(
            TracingConfig::for_profile(Profile::Dev).format,
            LogFormat::Pretty
        );
        assert_eq!(
            TracingConfig::for_profile(Profile::Test).format,
            LogFormat::Json
        );
        assert_eq!(
            TracingConfig::for_profile(Profile::Production).format,
            LogFormat::Json
        );
        // The level does not move with the profile.
        assert_eq!(TracingConfig::for_profile(Profile::Dev).level, "info");
    }

    #[test]
    fn the_dev_and_test_listeners_stay_on_the_loopback() {
        use crate::config::Profile;
        assert!(
            ServerConfig::for_profile(Profile::Dev)
                .bind
                .ip()
                .is_loopback()
        );
        assert!(
            ServerConfig::for_profile(Profile::Test)
                .bind
                .ip()
                .is_loopback()
        );
        assert!(
            !ServerConfig::for_profile(Profile::Production)
                .bind
                .ip()
                .is_loopback()
        );
    }

    #[test]
    fn log_formats_coerce_from_a_string() {
        use crate::config::{Coerce, RawValue};
        assert_eq!(
            LogFormat::coerce(&RawValue::String("JSON".into())).unwrap(),
            LogFormat::Json
        );
        assert!(LogFormat::coerce(&RawValue::String("xml".into())).is_err());
    }

    // ── validation ───────────────────────────────────────────────────────

    #[test]
    fn a_default_server_config_validates() {
        let mut errors = crate::error::BootErrors::new();
        ServerConfig::default().validate(&mut errors);
        assert!(errors.is_empty(), "{errors}");
    }

    #[test]
    fn configuring_tls_is_a_boot_error_and_not_a_plaintext_listener() {
        let mut errors = crate::error::BootErrors::new();
        ServerConfig {
            tls: Some(TlsConfig {
                cert_path: "/etc/tls/cert.pem".into(),
                key_path: "/etc/tls/key.pem".into(),
            }),
            ..ServerConfig::default()
        }
        .validate(&mut errors);
        assert_eq!(errors.len(), 1);
        let rendered = errors.render(false);
        assert!(rendered.contains("does not terminate TLS"), "{rendered}");
        assert!(rendered.contains("fix"), "{rendered}");
    }

    #[test]
    fn a_grace_longer_than_the_kill_timeout_is_a_boot_error() {
        let mut errors = crate::error::BootErrors::new();
        ServerConfig {
            shutdown_grace: Duration::from_secs(120),
            ..ServerConfig::default()
        }
        .validate(&mut errors);
        assert_eq!(errors.len(), 1);
        assert!(errors.render(false).contains("SIGKILL"));
    }

    #[test]
    fn the_default_grace_is_under_the_usual_kill_timeout() {
        assert!(ServerConfig::default().shutdown_grace < USUAL_KILL_TIMEOUT);
    }

    #[test]
    fn disclosure_outside_dev_is_announced() {
        use crate::config::Profile;
        // The warning path is a `tracing` call, so the assertion here is that
        // the predicate behind it is the documented one.
        let config = HttpConfig {
            expose_internal_errors: true,
            ..HttpConfig::default()
        };
        config.warn_if_disclosing(Profile::Production);
        config.warn_if_disclosing(Profile::Dev);
        assert!(!Profile::Production.exposes_errors());
        assert!(Profile::Dev.exposes_errors());
    }
}
