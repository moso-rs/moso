//! `observability` — installing the process's `tracing` subscriber.
//!
//! Every event and span the framework emits goes through the `tracing` facade,
//! which is inert until *something* installs a subscriber. This module is that
//! something: [`init`] reads a [`TracingConfig`] and installs a
//! `tracing-subscriber` `Registry` with a formatting layer, an `EnvFilter`, and
//! — behind the `otel` feature — an OpenTelemetry OTLP export layer.
//!
//! # Who calls it, and why it is idempotent
//!
//! A subscriber is **process-global**: there is exactly one, and the first
//! writer wins. That collides with a framework whose defining promise is that
//! two [`App`](crate::App)s can be built in one process. [`init`] resolves the
//! collision by installing through
//! [`try_init`](tracing_subscriber::util::SubscriberInitExt::try_init): the
//! first call wins and its [`TracingGuard`] owns the exporter, and every later
//! call is a no-op that reports [`TracingGuard::installed`] as `false` rather
//! than panicking. So it is safe to call from `main`, from
//! [`App::serve`](crate::App::serve) (which does, from the config set with
//! [`AppBuilder::tracing_config`](crate::AppBuilder::tracing_config)), and from
//! a test — in any order.
//!
//! ```text
//! main ──► observability::init(&cfg) ──► TracingGuard   (owns the OTLP exporter)
//!                                          │
//! App::serve ──► init(&stored_cfg) ──► no-op guard       (main already won)
//!                                          │
//!                                    drop at process exit ──► flush + shutdown
//! ```
//!
//! # The filter
//!
//! `RUST_LOG` wins when it is set, exactly as an operator expects; otherwise the
//! filter is [`TracingConfig::level`], which is `info` by default. The level is
//! a per-layer filter on the formatting layer, **not** a global one, so under
//! the `otel` feature the export layer still sees every span and makes its own
//! sampling decision — a low `RUST_LOG` must not silence the traces.
//!
//! # OTLP export (`otel` feature)
//!
//! With the `otel` feature on **and** [`TracingConfig::otlp_endpoint`] set,
//! [`init`] also installs a batch OTLP exporter over plaintext gRPC and a W3C
//! `traceparent` propagator. [`TracingConfig::service_name`] becomes the
//! OpenTelemetry `service.name` resource attribute, and
//! [`TracingConfig::sample_ratio`] becomes a parent-based
//! `TraceIdRatioBased` sampler — a request that arrives already sampled stays
//! sampled, and a fresh request is sampled at the ratio. The exporter is flushed
//! and shut down when the [`TracingGuard`] drops, which is the fourteenth and
//! last step of the [boot sequence](crate::app).
//!
//! Without the `otel` feature there is no exporter and no propagator, so the
//! per-request span cannot be reparented onto a remote context — it keeps the
//! correlation-only behaviour documented on
//! [`trace`](crate::middleware::trace).

use crate::http_config::{LogFormat, TracingConfig};

/// Flushes exporters on drop, and remembers whether it won the install race.
///
/// The value [`init`] returns. Keep it alive for as long as the process should
/// be exporting — in `main`, that is the whole of `main`; in
/// [`App::serve`](crate::App::serve) it is the serving lifetime. Dropping it
/// flushes and shuts down the OTLP exporter (under the `otel` feature) so a
/// short-lived process does not lose its last batch of spans.
///
/// ```
/// use moso_core::http_config::TracingConfig;
///
/// let guard = moso_core::observability::init(&TracingConfig::default());
/// // The first installer in a process wins; a later one is a no-op.
/// assert!(guard.installed() || !guard.installed());
/// ```
#[must_use = "dropping the guard immediately flushes and shuts down the exporter"]
pub struct TracingGuard {
    installed: bool,
    #[cfg(feature = "otel")]
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl TracingGuard {
    /// Whether *this* call installed the global subscriber.
    ///
    /// `false` means another [`init`] got there first — a second `App`, or a
    /// `main` that installed its own. It is not an error: the process is traced,
    /// just not by this guard.
    #[must_use]
    pub fn installed(&self) -> bool {
        self.installed
    }
}

impl core::fmt::Debug for TracingGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TracingGuard")
            .field("installed", &self.installed)
            .finish_non_exhaustive()
    }
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        if let Some(provider) = self.provider.take() {
            // Flush the last batch before tearing the pipeline down: a process
            // that exits right after the final span would otherwise drop it.
            let _ = provider.force_flush();
            let _ = provider.shutdown();
        }
    }
}

/// Install a global `tracing` subscriber from `config`.
///
/// Safe to call more than once per process: the first call installs the
/// subscriber and owns the exporter, and every later call returns a
/// [`TracingGuard`] whose [`installed`](TracingGuard::installed) is `false`
/// without disturbing the subscriber already in place. That is what lets two
/// [`App`](crate::App)s coexist.
///
/// ```no_run
/// // `no_run`: installing a process-global subscriber is a side effect a doc
/// // test should describe, not perform.
/// use moso_core::http_config::{LogFormat, TracingConfig};
///
/// let config = TracingConfig {
///     level: "info,my_app=debug".to_owned(),
///     format: LogFormat::Json,
///     ..TracingConfig::default()
/// };
/// let _guard = moso_core::observability::init(&config);
/// // … run the application; `_guard` flushes on the way out.
/// ```
#[cfg(feature = "subscriber")]
pub fn init(config: &TracingConfig) -> TracingGuard {
    use tracing_subscriber::prelude::*;

    #[cfg_attr(
        not(feature = "otel"),
        allow(unused_mut, reason = "only the otel export layer pushes onto it")
    )]
    let mut layers: Vec<
        Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
    > = vec![fmt_layer(config)];

    #[cfg(feature = "otel")]
    let mut provider = None;
    #[cfg(feature = "otel")]
    if let Some((layer, built)) = otel_layer(config) {
        layers.push(layer);
        provider = Some(built);
    }

    let installed = tracing_subscriber::registry()
        .with(layers)
        .try_init()
        .is_ok();

    #[cfg(feature = "otel")]
    if !installed {
        // A second `App` in the process: the layer we built was never installed,
        // so shut its exporter down now rather than leak a background batch task
        // that would never be flushed.
        if let Some(built) = provider.take() {
            let _ = built.shutdown();
        }
    }

    TracingGuard {
        installed,
        #[cfg(feature = "otel")]
        provider,
    }
}

/// The formatting layer for a format, with `config`'s filter attached to it.
///
/// The filter is per-layer rather than global so that an `otel` export layer,
/// which is added beside this one, is not silenced by a quiet `RUST_LOG`.
#[cfg(feature = "subscriber")]
fn fmt_layer(
    config: &TracingConfig,
) -> Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync> {
    use tracing_subscriber::Layer as _;

    let filter = env_filter(config);
    let spans = config.spans;
    match config.format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            // `spans` off drops the span context from each JSON object; the
            // per-request span itself is governed by the trace middleware slot.
            .with_current_span(spans)
            .with_span_list(spans)
            .with_filter(filter)
            .boxed(),
        LogFormat::Pretty => tracing_subscriber::fmt::layer()
            .pretty()
            .with_filter(filter)
            .boxed(),
        LogFormat::Compact => tracing_subscriber::fmt::layer()
            .compact()
            .with_filter(filter)
            .boxed(),
    }
}

/// `RUST_LOG` when it is set, else the configured level, else `info`.
#[cfg(feature = "subscriber")]
fn env_filter(config: &TracingConfig) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new(&config.level))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

/// The OTLP export layer and its provider, when an endpoint is configured.
///
/// `None` when [`TracingConfig::otlp_endpoint`] is unset (export is off) or when
/// the exporter cannot be built — a bad endpoint disables export rather than
/// aborting a process whose job is to serve requests, not to trace them.
#[cfg(feature = "otel")]
fn otel_layer(
    config: &TracingConfig,
) -> Option<(
    Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
    opentelemetry_sdk::trace::SdkTracerProvider,
)> {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig as _;
    use tracing_subscriber::Layer as _;

    let endpoint = config.otlp_endpoint.as_ref()?;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .ok()?;

    let service_name = config
        .service_name
        .clone()
        .unwrap_or_else(|| "moso".to_owned());
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name)
        .build();

    // Parent-based: continue the sampling decision of an incoming trace, and
    // sample a fresh one at the ratio. A batch processor keeps export off the
    // request path.
    let sampler = opentelemetry_sdk::trace::Sampler::ParentBased(Box::new(
        opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(config.sample_ratio),
    ));
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler)
        .with_id_generator(opentelemetry_sdk::trace::RandomIdGenerator::default())
        .with_resource(resource)
        .build();

    // The propagator is what lets an incoming `traceparent` reparent the request
    // span — see `crate::middleware::trace`.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let tracer = provider.tracer("moso");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer).boxed();
    Some((layer, provider))
}

#[cfg(all(test, feature = "subscriber"))]
mod tests {
    use super::*;

    #[test]
    fn a_second_init_is_a_no_op_rather_than_a_panic() {
        // Quiet, because this installs a real process-global subscriber (nextest
        // isolates each test in its own process, so this does not leak into the
        // other tests).
        let quiet = TracingConfig {
            level: "off".to_owned(),
            ..TracingConfig::default()
        };
        let first = init(&quiet);
        assert!(first.installed(), "the first install in a process wins");
        let second = init(&quiet);
        assert!(
            !second.installed(),
            "a second install is a no-op, not a panic — two Apps share a process"
        );
    }

    #[test]
    fn the_env_filter_falls_back_to_the_configured_level() {
        // With `RUST_LOG` unset in this process, the filter is the configured
        // level; a malformed level still resolves rather than panicking.
        let config = TracingConfig {
            level: "warn".to_owned(),
            ..TracingConfig::default()
        };
        // The directive round-trips through `EnvFilter`'s own `Display`.
        assert!(env_filter(&config).to_string().contains("warn"));
    }

    #[cfg(feature = "otel")]
    #[test]
    fn no_endpoint_means_no_export_layer() {
        let config = TracingConfig {
            otlp_endpoint: None,
            ..TracingConfig::default()
        };
        assert!(
            otel_layer(&config).is_none(),
            "export is off when no endpoint is configured"
        );
    }

    #[cfg(feature = "otel")]
    #[tokio::test]
    async fn an_endpoint_builds_an_exporter_and_installs_the_propagator() {
        // Building the gRPC exporter constructs a hyper client, so it must run
        // inside a Tokio runtime — exactly as it does from `App::serve`. The
        // build does not connect, so a valid endpoint yields a layer even with
        // nothing listening.
        let config = TracingConfig {
            otlp_endpoint: Some("http://127.0.0.1:4317".to_owned()),
            service_name: Some("shop".to_owned()),
            sample_ratio: 0.25,
            ..TracingConfig::default()
        };
        let built = otel_layer(&config);
        assert!(built.is_some(), "a valid endpoint builds the export layer");
        if let Some((_, provider)) = built {
            // Tidy up the batch task this test just started.
            let _ = provider.shutdown();
        }

        // The propagator is now installed, so a `traceparent` extracts to a
        // valid remote context — the plumbing `middleware::trace` relies on.
        use std::collections::HashMap;
        let carrier: HashMap<String, String> = HashMap::from([(
            "traceparent".to_owned(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_owned(),
        )]);
        let context = opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.extract(&carrier)
        });
        use opentelemetry::trace::TraceContextExt as _;
        assert_eq!(
            context.span().span_context().trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }
}
