//! A handler that forgot `async`.
//!
//! The extraction glue awaits the handler, so a plain `fn` cannot be one. The
//! macro says so on the `fn` token rather than letting `HandlerFn` fail.

use moso::prelude::*;

#[endpoint]
fn list() -> Result<NoContent> {
    Ok(NoContent)
}

fn main() {}
