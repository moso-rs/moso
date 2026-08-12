//! A factory default written on the field instead of on the struct.
//!
//! `factory` is a declared helper attribute of `#[derive(Factory)]`, so rustc
//! accepts it wherever it appears and strips it. Only the container attribute
//! is read, so without this error the default would vanish without a word and
//! `email` would quietly fall back to `Default::default()` — a value nobody
//! goes looking for, because nothing ever looks wrong.
//!
//! The field's type is `Email` on purpose. It has no `Default`, and
//! `Author::factory()` is called below, so this case also pins the two ways the
//! refusal could turn one mistake into three errors: it must expand to a
//! factory, and that factory must go on using the expression that was written
//! in the wrong place.

use moso::schema::Email;
use moso::{Entity, Factory};

/// Someone who can write a post.
#[derive(Entity, Factory, Debug, Clone)]
#[entity(table = "authors")]
pub struct Author {
    /// The primary key.
    #[entity(pk)]
    pub id: i64,

    /// Login identity; one row per address.
    #[factory(default = "Email::new(format!(\"a{n}@example.com\")).expect(\"valid\")")]
    pub email: Email,
}

fn main() {
    let _ = Author::factory().build();
}
