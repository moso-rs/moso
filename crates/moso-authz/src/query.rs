//! Query-level filtering: `Post::query().authorized_for::<Read>(&actor)`.
//!
//! The feature that separates an authorization *layer* from an authorization
//! *decorator*. Filtering rows after they are loaded is wrong at scale twice
//! over: the database reads rows the caller may not see, and every count and
//! cursor computed from that query is a lie. A [`ScopedPolicy`] contributes a
//! `WHERE` clause instead, so list endpoints are correct *and* fast and
//! pagination totals are right.

use moso_orm::{Entity, Select};

use crate::{Action, ScopedPolicy};

/// Narrow a query to what an actor may see.
///
/// ```text
/// let posts = Post::query()
///     .authorized_for::<Read>(&actor)
///     .paginate(cursor, 20)
///     .fetch(&db)
///     .await?;
/// ```
///
/// # Why the actor arrives as `&dyn ScopedPolicy<A, E>`
///
/// So that `authorized_for::<Read>` works. Rust has no partial turbofish: a
/// method with two type parameters cannot have one supplied and one inferred,
/// and `impl Trait` in argument position forbids turbofish altogether. Erasing
/// the actor to a trait object leaves exactly one parameter — the action —
/// which is the one a reader wants to see at the call site. [`ScopedPolicy`] is
/// synchronous, so the trait object costs one indirect call per query.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a query that can be authorized",
    label = "not an authorizable query",
    note = "`authorized_for` is available on `Select<E>` — the shape a query has after tenant \
            scoping",
    note = "help: if this is a `Select<E, NeedsTenant>`, scope it first: `.scoped(tenant)`. \
            Authorizing before tenant scoping would filter across every tenant's rows"
)]
pub trait AuthorizedQuery<E: Entity>: Sized {
    /// Apply the scoped policy for `A`.
    ///
    /// Shape-stable (non-negotiable N1): `Select<E>` in, `Select<E>` out. The
    /// filter is added to the same builder, so this composes with `filter`,
    /// `paginate` and everything else in any order.
    fn authorized_for<A: Action>(self, actor: &dyn ScopedPolicy<A, E>) -> Self;

    /// Apply the scoped policy only when `condition` holds.
    ///
    /// For the "operators bypass the filter" case, kept explicit at the call
    /// site rather than hidden inside a policy.
    fn authorized_for_if<A: Action>(self, condition: bool, actor: &dyn ScopedPolicy<A, E>) -> Self {
        if condition {
            self.authorized_for(actor)
        } else {
            self
        }
    }
}

// A blanket impl over every entity: `Select<E>` is the only query shape there
// is, so specialising per entity would be noise. `do_not_recommend` keeps a
// failed `Entity` bound from being reported as "consider implementing
// `AuthorizedQuery`", which is the internal half nobody writes.
#[diagnostic::do_not_recommend]
impl<E: Entity> AuthorizedQuery<E> for Select<E> {
    fn authorized_for<A: Action>(self, actor: &dyn ScopedPolicy<A, E>) -> Self {
        // Shape-stable by construction: the policy is handed the query and hands
        // one back, so `filter`, `paginate` and `order_by` compose around it in
        // any order and the type never changes (non-negotiable N1).
        let narrowed = actor.scope_query(self);
        tracing::trace!(
            target: "moso::authz",
            action = A::NAME,
            entity = E::NAME,
            filters = narrowed.filters().len(),
            "query scoped by policy"
        );
        narrowed
    }
}
