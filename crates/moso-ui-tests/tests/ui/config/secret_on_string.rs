//! `#[config(secret)]` on a plain `String`.
//!
//! A configuration value that must never be printed has to be a type that
//! cannot print itself. Marking a `String` "secret" only tells the descriptor
//! to redact it in the boot report; `{database_url}` in a log line still writes
//! the password out in full.

use moso::prelude::*;

#[derive(Config)]
struct AppConfig {
    #[config(secret)]
    database_url: String,
}

fn main() {}
