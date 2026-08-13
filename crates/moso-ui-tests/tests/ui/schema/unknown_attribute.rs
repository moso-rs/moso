//! A misspelt `#[schema(…)]` key.
//!
//! Rule 2 of the macro contract: an unknown attribute key is a compile error
//! with a suggestion, never a silent no-op. A silently ignored `lenght` would
//! ship an unvalidated field *and* an undocumented constraint.

use moso::prelude::*;

#[derive(Schema)]
struct CreateUser {
    #[schema(lenght = 3..=32)]
    name: String,
}

fn main() {}
