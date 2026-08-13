//! A range whose lower bound is above its upper bound.
//!
//! `len = 32..=3` matches nothing, so every request to this endpoint would 422
//! and the published `minLength`/`maxLength` pair would be nonsense. The macro
//! knows both numbers at expansion time, so it can say which way round they go.

use moso::prelude::*;

#[derive(Schema)]
struct CreateUser {
    #[schema(len = 32..=3)]
    name: String,
    #[schema(range = 130..=13)]
    age: u8,
}

fn main() {}
