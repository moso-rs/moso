//! The application's own middleware.
//!
//! `#[moso::middleware]` turns one `async fn` into a `tower::Layer` /
//! `tower::Service` pair — here `ObserveLayer` and `ObserveService` — named
//! after the function and registered like any other layer:
//!
//! ```
//! use example_crud::config::AppConfig;
//! use example_crud::middleware::ObserveLayer;
//! use example_crud::routes;
//! use moso::prelude::*;
//!
//! # fn main() -> Result<()> {
//! let app = App::new(AppConfig::defaults()?)
//!     .mount(routes::router().layer(ObserveLayer::new()));
//! # let _ = app;
//! # Ok(())
//! # }
//! ```
//!
//! Parameters before `req` are extracted before the function body runs, so
//! `Inject<Metrics>` works and its provider requirement is folded into
//! `ObserveLayer::PROVIDER_REQ` — forgetting `.provide(Metrics::default())` is
//! a boot error naming `Metrics`, not a panic on the first request.

use std::sync::atomic::{AtomicU64, Ordering};

use moso::deps::http::HeaderValue;
use moso::middleware::Next;
use moso::prelude::*;
use moso::{Request, Response};

/// The header every response carries, so a proxy can tell which service
/// answered.
pub const APP_HEADER: &str = "x-app";

/// Counters shared by every request.
///
/// Provided once at boot with `.provide(Metrics::default())` and reached by the
/// middleware through `Inject<Metrics>`.
#[derive(Debug, Default)]
pub struct Metrics {
    /// How many requests the layer has seen.
    pub requests: AtomicU64,
    /// How many of those the application answered with a 4xx or a 5xx.
    pub failures: AtomicU64,
}

impl Metrics {
    /// The number of requests seen so far.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// The number of failed responses seen so far.
    #[must_use]
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }
}

/// Count every request and stamp every response.
///
/// The counting half proves the layer runs on the way *in*; the header proves
/// it runs on the way *out*. A middleware that only did one of the two would
/// pass a test that only checked the other.
#[moso::middleware]
pub async fn observe(
    Inject(metrics): Inject<Metrics>,
    req: Request,
    next: Next,
) -> Result<Response> {
    metrics.requests.fetch_add(1, Ordering::Relaxed);

    let mut response = next.run(req).await;

    if response.status().is_client_error() || response.status().is_server_error() {
        metrics.failures.fetch_add(1, Ordering::Relaxed);
    }
    response
        .headers_mut()
        .insert(APP_HEADER, HeaderValue::from_static("moso-crud"));

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counters_start_at_zero_and_move_independently() {
        let metrics = Metrics::default();
        assert_eq!(metrics.requests(), 0);
        assert_eq!(metrics.failures(), 0);

        metrics.requests.fetch_add(1, Ordering::Relaxed);
        assert_eq!(metrics.requests(), 1);
        assert_eq!(metrics.failures(), 0);
    }

    #[test]
    fn the_generated_layer_declares_what_it_injects() {
        // The provider requirement the macro folded in from `Inject<Metrics>`.
        // This is what makes a missing `.provide(Metrics::default())` a boot
        // error rather than a 500 on the first request.
        let names: Vec<&str> = ObserveLayer::PROVIDER_REQ
            .iter()
            .map(moso::ProviderReq::name)
            .collect();
        assert!(
            names.iter().any(|name| name.contains("Metrics")),
            "{names:?}"
        );
    }
}
