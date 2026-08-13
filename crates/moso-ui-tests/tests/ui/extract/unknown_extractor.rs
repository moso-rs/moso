//! A parameter that is not an extractor.
//!
//! `Tenant` is an ordinary struct. Nothing tells the framework where to get one
//! from a request, so it cannot be a handler parameter. The diagnostic has to
//! name `Tenant` and list the ways to make it extractable.

use moso::prelude::*;

struct Tenant {
    slug: String,
}

#[endpoint]
async fn list(tenant: Tenant) -> Result<NoContent> {
    let _ = tenant.slug;
    Ok(NoContent)
}

fn main() {}
