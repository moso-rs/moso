//! `Depends<T>` in a middleware signature.
//!
//! Middleware runs before extraction, so the request-scoped dependency cache is
//! empty when the layer is entered. This is not expressible as a trait bound —
//! `Depends<CurrentUser>` really does implement `Extract`, and `CurrentUser`
//! below really is a `Dependency` — so the macro is the only place that can
//! catch it.

use moso::middleware::Next;
use moso::prelude::*;
use moso::{Request, Response};

/// Who the request acts as. A perfectly good dependency; just not here.
#[derive(Clone)]
struct CurrentUser {
    id: u32,
}

impl Dependency for CurrentUser {
    async fn resolve(_ctx: &RequestCtx) -> Result<Self> {
        Ok(Self { id: 1 })
    }
}

#[moso::middleware]
async fn tenant(
    Depends(user): Depends<CurrentUser>,
    req: Request,
    next: Next,
) -> Result<Response> {
    let _ = user.id;
    Ok(next.run(req).await)
}

fn main() {}
