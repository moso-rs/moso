//! The route table, assembled.
//!
//! One module per resource, each exporting a `router()`, merged and nested
//! here. `nest` rewrites the documented paths too, so `/posts` registered in
//! `posts.rs` appears as `/api/v1/posts` in `openapi.json`.

use moso::prelude::*;

pub mod health;
pub mod posts;

/// Every route this application serves.
pub fn router() -> Router {
    Router::new()
        .merge(health::router())
        .nest("/api/v1", api_v1())
}

/// Version 1 of the API.
///
/// `.responds(429, …)` documents the rate-limit response on every operation
/// underneath, so the contract is stated once rather than copied into six
/// handlers.
fn api_v1() -> Router {
    posts::router().responds(429, ResponseSpec::problem("Too many requests."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_is_registered_exactly_once() {
        let router = router();
        assert_eq!(router.len(), 7, "six post routes plus the status endpoint");
        assert!(
            router.conflicts().is_empty(),
            "{:?}",
            router.conflicts().len()
        );
    }

    #[test]
    fn the_api_is_nested_under_its_version() {
        let router = router();
        let paths: Vec<&str> = router
            .entries()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert!(paths.contains(&"/api/v1/posts"), "{paths:?}");
        assert!(paths.contains(&"/api/v1/posts/{id}/publish"), "{paths:?}");
        assert!(paths.contains(&"/status"), "{paths:?}");
    }
}
