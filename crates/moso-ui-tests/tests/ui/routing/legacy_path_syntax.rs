//! Axum 0.7 / Actix / Rocket path syntax.
//!
//! `:id` is what a reader coming from any other Rust router will type. Moso uses
//! OpenAPI syntax everywhere so that the route table and the published document
//! spell a parameter the same way, and `routes!` wraps every literal in
//! `route_path!` so the check happens at compile time rather than at boot.

use moso::prelude::*;

#[endpoint]
async fn show(Path(id): Path<u32>) -> Result<NoContent> {
    let _ = id;
    Ok(NoContent)
}

pub fn router() -> Router {
    moso::routes! {
        GET "/users/:id" => show,
    }
}

fn main() {}
