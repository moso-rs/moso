//! `/status` — what this instance is and what it holds.
//!
//! Distinct from the `/healthz` and `/readyz` that `App::build()` mounts on its
//! own. Those answer an orchestrator; this one answers a human, and exists here
//! to show a plain unguarded endpoint reading two providers.

use moso::db::Db;
use moso::prelude::*;

use crate::config::AppConfig;
use crate::middleware::Metrics;
use crate::models::Post;

/// The router for the status endpoint.
pub fn router() -> Router {
    moso::routes! {
        GET "/status" => status,
    }
    .tag("status")
}

/// What this instance is doing.
#[derive(Schema, Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The configured name of this instance.
    pub name: String,
    /// The version of the application.
    pub version: String,
    /// How many posts are stored.
    pub posts: u64,
    /// How many requests this process has served.
    pub requests: u64,
}

/// Report the instance name, the version, and what it is holding.
#[endpoint]
async fn status(
    Inject(config): Inject<AppConfig>,
    Inject(db): Inject<Db>,
    Inject(metrics): Inject<Metrics>,
) -> Result<Json<Status>> {
    let posts = Post::query().count(&*db).await.map_err(Error::internal)?;
    Ok(Json(Status {
        name: config.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        posts,
        requests: metrics.requests(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_router_registers_one_operation() {
        assert_eq!(router().len(), 1);
    }
}
