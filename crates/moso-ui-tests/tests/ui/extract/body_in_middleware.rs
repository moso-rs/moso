//! A body extractor in a middleware signature.
//!
//! Middleware is handed the whole request and passes it inwards. An extractor
//! that consumes the body would leave nothing for the handler to read, and the
//! failure would surface as an empty body at runtime rather than here.

use moso::middleware::Next;
use moso::prelude::*;
use moso::{Request, Response};

#[derive(Schema)]
struct Audit {
    action: String,
}

#[moso::middleware]
async fn audit(Json(entry): Json<Audit>, req: Request, next: Next) -> Result<Response> {
    let _ = entry;
    Ok(next.run(req).await)
}

fn main() {}
