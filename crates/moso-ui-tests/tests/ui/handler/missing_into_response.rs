//! A handler returning a type that is not a response.
//!
//! `Report` is a plain struct: it has no `IntoResponse`, so there is no status
//! code and no body, and no `Describe`, so there is nothing to write into the
//! OpenAPI document. The fix is one derive.

use moso::prelude::*;

struct Report {
    total: u32,
}

#[endpoint]
async fn summary() -> Report {
    Report { total: 0 }
}

fn main() {}
