# moso-authz

Moso's authorization battery: typed permissions, roles, scoped role assignments, resource policies,
query-level filtering and an explainable audit trail.

Part of [Moso](https://github.com/lowsbarrel/moso). See `docs/03-batteries/31-authorization.md`
for the design.

```rust,ignore
moso::permissions! {
    /// Posts
    posts.read    = "View posts",
    posts.publish = "Publish posts",
    /// Administration
    admin.access  = "Access the admin panel",
}

moso::roles! {
    Viewer = [posts.read],
    Editor = Viewer + [posts.publish],
    Admin  = Editor + [admin.access],
}

// The capability check. `#[requires]` goes *above* `#[endpoint]`.
#[requires(Perm::PostsPublish)]
#[endpoint]
async fn publish_all(Inject(db): Inject<Db>) -> Result<NoContent> { /* … */ }

// The resource check: loads the row, runs the policy, hands the row over.
#[endpoint]
async fn publish(post: Authorized<Publish, Post>) -> Result<PostOut> { /* … */ }

// The query-level filter, which is what makes a list endpoint correct *and* fast.
let posts = Post::query().authorized_for::<Read>(&actor).paginate(cursor, 20).fetch(&db).await?;
```

## Status

**Implemented.** All eight acceptance criteria of WP-18 have tests, and the statement-counting ones
run against SQLite always and PostgreSQL when `DATABASE_URL` is set.

Two things are the application's to wire: an `ActorSource<Role>` (the one provider this crate needs;
a missing one is a boot error) and, if `#[requires]` is used, an
`ActorPermissions::<Role>::new()` registered as `dyn PermissionSource`.

## Licence

MIT — see the root [`LICENSE`](../../LICENSE).
