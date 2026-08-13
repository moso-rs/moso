//! A handler whose future is not `Send`.
//!
//! `Rc` is held across the `.await`, so the future cannot be moved to another
//! worker thread and the server cannot spawn it. This is the worst error shape
//! in async Rust; the macro's job is to make sure the first line names the
//! handler and the fix, not `Pin<Box<dyn Future<Output = …>>>`.

use moso::prelude::*;
use std::rc::Rc;

async fn tick() {}

#[endpoint]
async fn list() -> Result<NoContent> {
    let counter = Rc::new(0u32);
    tick().await;
    let _ = counter;
    Ok(NoContent)
}

fn main() {}
