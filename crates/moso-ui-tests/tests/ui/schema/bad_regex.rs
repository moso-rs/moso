//! A `pattern` that is not a regular expression.
//!
//! The macro compiles the pattern at build time so an unclosed class is a
//! compile error on the literal, not a panic on the first request that happens
//! to reach this field.

use moso::prelude::*;

#[derive(Schema)]
struct CreateUser {
    #[schema(pattern = r"^[a-z0-9_+$")]
    name: String,
}

fn main() {}
