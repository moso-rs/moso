//! A method a `routes!` table does not know.
//!
//! `PSOT` is a transposition, not a protocol extension. The table names the
//! methods it accepts rather than passing the token through to a runtime lookup.

use moso::prelude::*;

#[endpoint]
async fn create() -> Result<NoContent> {
    Ok(NoContent)
}

pub fn router() -> Router {
    moso::routes! {
        PSOT "/users" => create,
    }
}

fn main() {}
