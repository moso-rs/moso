//! An unclosed brace in a route template.
//!
//! `matchit` would reject this at boot with a message about its internal node
//! structure. Moso rejects it at compile time, on the literal, in the vocabulary
//! the user typed.

use moso::prelude::*;

#[endpoint]
async fn show(Path(id): Path<u32>) -> Result<NoContent> {
    let _ = id;
    Ok(NoContent)
}

pub fn router() -> Router {
    moso::routes! {
        GET "/users/{id" => show,
    }
}

fn main() {}
