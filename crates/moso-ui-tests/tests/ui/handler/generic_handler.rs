//! A generic handler.
//!
//! A route stores one erased handler, so there is no call site at which `T`
//! could be chosen. Rust would say "type annotations needed" somewhere inside
//! generated code; the macro says it on the type parameter.

use moso::prelude::*;

#[endpoint]
async fn list<T: Send>(Inject(value): Inject<T>) -> Result<NoContent> {
    let _ = value;
    Ok(NoContent)
}

fn main() {}
