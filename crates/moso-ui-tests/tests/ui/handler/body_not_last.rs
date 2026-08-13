//! A body extractor that is not the last parameter.
//!
//! The body can only be read once, and the extraction glue reads it after every
//! `Extract` parameter, so `Json<T>` has to come last. `#[endpoint]` detects
//! this syntactically (docs/04-devex/41-diagnostics.md, tool 3) so the span
//! lands on the parameter that should not be there rather than on a trait bound.

use moso::prelude::*;

#[derive(Schema)]
struct CreateUser {
    name: String,
}

#[endpoint]
async fn create(Json(body): Json<CreateUser>, Path(id): Path<u32>) -> Result<NoContent> {
    let _ = (body, id);
    Ok(NoContent)
}

fn main() {}
