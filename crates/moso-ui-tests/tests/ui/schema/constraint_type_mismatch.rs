//! A text constraint on a number, and a numeric one on a string.
//!
//! Both would compile into a check that can never run and a JSON Schema keyword
//! that contradicts the field's `type`. The macro knows the field's shape
//! syntactically, so it can name it.

use moso::prelude::*;

#[derive(Schema)]
struct Filter {
    #[schema(pattern = r"^\d+$")]
    page: u32,
    #[schema(range = 1..=10)]
    query: String,
}

fn main() {}
