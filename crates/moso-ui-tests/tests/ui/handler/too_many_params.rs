//! Seventeen parameters, one more than the glue is generated for.
//!
//! Without the macro's check this is sixteen pages of "the trait bound
//! `Handler<_>` is not satisfied" listing every arity impl. With it, it is one
//! sentence naming the count and a fix.

use moso::extract::RequestId;
use moso::prelude::*;

#[endpoint]
async fn list(
    a: RequestId,
    b: RequestId,
    c: RequestId,
    d: RequestId,
    e: RequestId,
    f: RequestId,
    g: RequestId,
    h: RequestId,
    i: RequestId,
    j: RequestId,
    k: RequestId,
    l: RequestId,
    m: RequestId,
    n: RequestId,
    o: RequestId,
    p: RequestId,
    q: RequestId,
) -> Result<NoContent> {
    let _ = (a, b, c, d, e, f, g, h);
    let _ = (i, j, k, l, m, n, o, p, q);
    Ok(NoContent)
}

fn main() {}
