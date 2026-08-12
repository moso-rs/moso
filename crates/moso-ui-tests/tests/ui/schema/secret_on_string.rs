//! `#[schema(secret)]` on a plain `String`.
//!
//! Marking the field redacts *this* struct's `Debug`, and nothing else: the
//! `String` behind it still has `Display`, still has `AsRef<str>`, and still
//! formats itself into the first `tracing::info!` that touches it. Secrecy has
//! to be a property of the type, which is what `Password` and `SecretString`
//! are for.

use moso::prelude::*;

#[derive(Schema)]
struct Credentials {
    email: Email,
    #[schema(secret, len = 12..)]
    password: String,
}

fn main() {}
