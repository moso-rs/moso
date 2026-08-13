//! Two body extractors in one handler.
//!
//! A request body can be read once. Two body extractors is the more specific
//! diagnosis than "the body must be last", so it is the one reported — one
//! error, not a cascade.

use moso::prelude::*;

#[derive(Schema)]
struct CreatePost {
    title: String,
}

#[endpoint]
async fn create(Json(post): Json<CreatePost>, Form(draft): Form<CreatePost>) -> Result<NoContent> {
    let _ = (post, draft);
    Ok(NoContent)
}

fn main() {}
