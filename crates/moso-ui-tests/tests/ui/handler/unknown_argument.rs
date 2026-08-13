//! A misspelt `#[endpoint(…)]` argument.
//!
//! `tags` is one keystroke from `tag`, and silently ignoring it would ship an
//! operation that is missing from every tagged section of the documentation.

use moso::prelude::*;

#[endpoint(tags = "users")]
async fn list() -> Result<NoContent> {
    Ok(NoContent)
}

fn main() {}
